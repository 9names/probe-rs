use std::{
    fmt::Debug,
    time::{Duration, Instant},
};

use bitvec::{field::BitField, slice::BitSlice};

use crate::{
    Error as ProbeRsError,
    architecture::xtensa::arch::instruction::{Instruction, InstructionEncoding},
    probe::{
        CommandQueue, CommandResult, DeferredResultIndex, DeferredResultSet, JtagAccess,
        JtagCommand, JtagWriteCommand, ShiftDrCommand,
    },
};

use super::communication_interface::XtensaError;

const NARADR_OCDID: u8 = 0x40;
const NARADR_DCRSET: u8 = 0x43;
const NARADR_DCRCLR: u8 = 0x42;
const NARADR_DSR: u8 = 0x44;
const NARADR_DDR: u8 = 0x45;
const NARADR_DDREXEC: u8 = 0x46;
// DIR0 that also executes when written
const NARADR_DIR0EXEC: u8 = 0x47;
// Assume we only support 16-24b instructions for now
const NARADR_DIR0: u8 = 0x48;

#[derive(Clone, Copy, PartialEq, Debug)]
enum TapInstruction {
    Nar,
    Ndr,
    PowerControl,
    PowerStatus,
    Idcode,
}

impl TapInstruction {
    fn code(self) -> u32 {
        match self {
            TapInstruction::Nar => 0x1C,
            TapInstruction::Ndr => 0x1C,
            TapInstruction::PowerControl => 0x08,
            TapInstruction::PowerStatus => 0x09,
            TapInstruction::Idcode => 0x1E,
        }
    }

    fn bits(self) -> u32 {
        match self {
            TapInstruction::Nar => 8,
            TapInstruction::Ndr => 32,
            TapInstruction::PowerControl => 8,
            TapInstruction::PowerStatus => 8,
            TapInstruction::Idcode => 32,
        }
    }
}

/// Power registers are separate from the other registers. They are part of the Access Port.
#[derive(Clone, Copy, PartialEq, Debug)]
enum PowerDevice {
    /// Power Control
    PowerControl,
    /// Power status
    PowerStat,
}

impl From<PowerDevice> for TapInstruction {
    fn from(dev: PowerDevice) -> Self {
        match dev {
            PowerDevice::PowerControl => TapInstruction::PowerControl,
            PowerDevice::PowerStat => TapInstruction::PowerStatus,
        }
    }
}

#[derive(thiserror::Error, Debug, Copy, Clone, docsplay::Display)]
pub enum DebugRegisterError {
    /// Register is busy
    Busy,

    /// Register-specific error
    Error,

    /// Unexpected value {0:#04x}
    Unexpected(u8),
}

#[derive(thiserror::Error, Debug, Clone, Copy, docsplay::Display)]
pub enum Error {
    /// Error {access} register {narsel:#04X}
    Xdm {
        /// Nexus Address Register selector. Contains the ID of the register being accessed.
        narsel: u8,

        /// Access type (reading or writing)
        access: &'static str,

        /// The error that occurred.
        source: DebugRegisterError,
    },

    /// The instruction execution has encountered an exception.
    ExecExeception,

    /// The core is still executing a previous instruction.
    ExecBusy,

    /// Instruction execution overrun.
    ExecOverrun,

    /// The instruction was ignored. Most often this indicates that the core was not halted before
    /// requesting instruction execution.
    InstructionIgnored,

    /// The Xtensa Debug Module is powered off.
    XdmPoweredOff,
}

#[derive(Debug, Default)]
pub struct XdmState {
    /// The last instruction to be executed.
    // This is used to:
    // - detect incorrect uses of `schedule_write_ddr_and_execute` which expects an instruction to
    //   be loaded
    // - wait for the last instruction to complete before proceeding, as some instructions can be
    //   assumed to complete instantly.
    last_instruction: Option<Instruction>,

    /// The command queue for the current batch. JTAG accesses are batched to reduce the number of
    /// IO operations.
    queue: CommandQueue<JtagCommand>,

    /// The results of the reads in the already executed batched JTAG commands.
    jtag_results: DeferredResultSet<CommandResult>,

    /// Read handles for accesses that need to force capturing their bits.
    ///
    /// The batching system tries to minimize the number of captured bits, in order to reduce
    /// the number of JTAG operations. However, some accesses need to capture their bits to
    /// complete correctly, or to - ironically - increase performance. We store their otherwise
    /// ignored handles in this vector and drop them when we're done with the batch.
    status_idxs: Vec<DeferredResultIndex>,
}

/// The lower level functions of the Xtensa Debug Module.
// TODO: this is mostly JTAG-specific, but not specifically. We should probably split this up, e.g.
// move the instruction execution into the current communication_interface module.
#[derive(Debug)]
pub struct Xdm<'probe> {
    /// The JTAG interface.
    pub probe: &'probe mut dyn JtagAccess,

    /// Debug module state.
    state: &'probe mut XdmState,
}

impl<'probe> Xdm<'probe> {
    pub fn new(probe: &'probe mut dyn JtagAccess, state: &'probe mut XdmState) -> Self {
        // TODO implement openocd's esp32_queue_tdi_idle() to prevent potentially damaging flash ICs

        Self { probe, state }
    }

    #[tracing::instrument(skip(self))]
    pub(crate) fn enter_debug_mode(&mut self) -> Result<(), XtensaError> {
        self.state.queue = CommandQueue::new();
        self.state.jtag_results = DeferredResultSet::new();

        self.probe.tap_reset()?;

        // Reset PCM
        let mut pwr_control = PowerControl(0);
        pwr_control.set_debug_reset(true);
        pwr_control.set_debug_wakeup(true);
        self.pwr_write(PowerDevice::PowerControl, pwr_control.0)?;

        // Reset must be high for 10 CPU clocks.
        std::thread::sleep(Duration::from_millis(1));

        let mut pwr_control = PowerControl(0);
        pwr_control.set_debug_wakeup(true);
        pwr_control.set_mem_wakeup(true);
        pwr_control.set_core_wakeup(true);
        // Wakeup. We enable JTAG in a separate write.
        self.pwr_write(PowerDevice::PowerControl, pwr_control.0)?;

        // Set JTAG_DEBUG_USE separately to ensure it doesn't get reset by a previous write.
        // "any write to PWRCTL when JtagDebugUse is set also clears the bit".
        pwr_control.set_jtag_debug_use(true);
        self.pwr_write(PowerDevice::PowerControl, pwr_control.0)?;

        // After software deasserts this bit (DebugReset), before reading other Debug registers,
        // polling on bit 31 of the Debug Status Register (see Table 5-22) should be performed until
        // it returns 1'b1.
        let now = Instant::now();
        loop {
            let status = self.status()?;
            if status.dbgmod_power_on() {
                break;
            }

            if now.elapsed() > Duration::from_millis(100) {
                return Err(XtensaError::CoreDisabled);
            }
        }

        let mut reset_bits = PowerStatus(0);
        reset_bits.set_core_was_reset(true);
        reset_bits.set_debug_was_reset(true);
        self.pwr_write(PowerDevice::PowerStat, reset_bits.0)?;

        self.check_enabled()?;

        // we might find that an old instruction execution left the core with an exception
        // try to clear problematic bits
        self.write_nexus_register({
            let mut status = DebugStatus(0);

            status.set_exec_exception(true);
            status.set_exec_done(true);
            status.set_exec_overrun(true);
            status.set_debug_pend_break(true);
            status.set_debug_pend_host(true);
            status.set_debug_int_break(true);

            status
        })?;

        // configure the debug module
        self.debug_control({
            let mut reg = DebugControlBits(0);
            reg.set_enable_ocd(true);
            reg
        })?;

        Ok(())
    }

    pub(crate) fn debug_control(&mut self, bits: DebugControlBits) -> Result<(), XtensaError> {
        self.schedule_write_nexus_register(DebugControlSet(bits));
        self.schedule_write_nexus_register(DebugControlClear({
            let mut reg = DebugControlBits(0);

            reg.set_break_in_en(!bits.break_in_en());
            reg.set_break_out_en(!bits.break_out_en());
            reg.set_debug_sw_active(!bits.debug_sw_active());
            reg.set_run_stall_in_en(!bits.run_stall_in_en());
            reg.set_debug_mode_out_en(!bits.debug_mode_out_en());

            reg
        }));
        // Clear pending interrupts that would re-enter us into the Stopped state.
        self.schedule_write_nexus_register({
            let mut status = DebugStatus(0);

            status.set_debug_pend_break(true);
            status.set_debug_int_break(true);

            status
        });

        Ok(())
    }

    fn check_enabled(&mut self) -> Result<(), XtensaError> {
        let Ok(device_id) = self.read_nexus_register::<OcdId>() else {
            return Err(XtensaError::CoreDisabled);
        };
        let device_id = device_id.0;
        tracing::debug!("Read OCDID: {:#010X}", device_id);

        if device_id == 0 || device_id == u32::MAX {
            // Disable the debug module if we can't work with it.
            self.pwr_write(PowerDevice::PowerControl, 0)?;
            return Err(XtensaError::CoreDisabled);
        }
        tracing::info!("Found Xtensa device with OCDID: {:#010X}", device_id);
        Ok(())
    }

    /// Read and clear the `PowerStatus` flags.
    pub(crate) fn power_status(&mut self, clear: PowerStatus) -> Result<PowerStatus, XtensaError> {
        let bits = self.pwr_write(PowerDevice::PowerStat, clear.0)?;
        Ok(PowerStatus(bits))
    }

    /// Read and clear the `core_was_reset` flag.
    pub(crate) fn read_power_status(&mut self) -> Result<PowerStatus, XtensaError> {
        let bits = self.pwr_write(PowerDevice::PowerStat, 0)?;
        Ok(PowerStatus(bits))
    }

    pub(crate) fn execute(&mut self) -> Result<(), XtensaError> {
        let mut queue = std::mem::take(&mut self.state.queue);

        tracing::debug!("Executing {} commands", queue.len());

        // Drop the status readers when we're done.
        // We take now to avoid a possibly recursive call to clear before it's time.
        let _idxs = std::mem::take(&mut self.state.status_idxs);

        while !queue.is_empty() {
            match self.probe.write_register_batch(&queue) {
                Ok(result) => {
                    self.state.jtag_results.merge_from(result);
                    return Ok(());
                }
                Err(e) => {
                    let mut to_consume = e.results.len();
                    match e.error {
                        ProbeRsError::Xtensa(XtensaError::XdmError(Error::Xdm {
                            source: DebugRegisterError::Busy,
                            ..
                        })) => {
                            // The specific nexus register may need some longer delay. For now we just
                            // retry, but we should probably add some no-ops later.
                        }
                        ProbeRsError::Xtensa(XtensaError::XdmError(Error::ExecBusy)) => {
                            // The instruction is still executing. Retry the Debug Status read.
                            to_consume -= 1;
                        }
                        ProbeRsError::Xtensa(XtensaError::XdmError(Error::ExecExeception)) => {
                            // Clear exception to allow executing further instructions.
                            self.clear_exception_state()?;
                            // TODO: in the future, we might want to bubble up the exception cause.
                            // We might also want to store this error for each result that has not
                            // yet been read.
                            return Err(XtensaError::XdmError(Error::ExecExeception));
                        }

                        ProbeRsError::Probe(error) => return Err(error.into()),
                        ProbeRsError::Xtensa(error) => return Err(error),
                        other => panic!("Unexpected error: {other}"),
                    }

                    // queue up the remaining commands when we retry
                    queue.consume(to_consume);
                    self.state.jtag_results.merge_from(e.results);
                }
            }
        }

        Ok(())
    }

    pub(crate) fn read_deferred_result(
        &mut self,
        index: DeferredResultIndex,
    ) -> Result<CommandResult, XtensaError> {
        match self.state.jtag_results.take(index) {
            Ok(result) => Ok(result),
            Err(index) => {
                self.execute()?;
                // We can lose data if `execute` fails.
                self.state
                    .jtag_results
                    .take(index)
                    .map_err(|_| XtensaError::BatchedResultNotAvailable)
            }
        }
    }

    fn do_nexus_op(&mut self, nar: u8, ndr: u32, transform: TransformFn) -> DeferredResultIndex {
        let nar = self.state.queue.schedule(JtagWriteCommand {
            address: TapInstruction::Nar.code(),
            data: nar.to_le_bytes().to_vec(),
            len: TapInstruction::Nar.bits(),
            transform: |write, capture| {
                let capture = capture.load_le::<u8>();
                let nar = write.data[0] >> 1;
                let write = write.data[0] & 1 == 1;

                // eww...?
                Err(ProbeRsError::Xtensa(XtensaError::XdmError(Error::Xdm {
                    narsel: nar,
                    access: if write { "writing" } else { "reading" },
                    source: match capture & 0b00000011 {
                        0 => return Ok(CommandResult::None),
                        1 => DebugRegisterError::Error,
                        2 => DebugRegisterError::Busy,
                        _ => DebugRegisterError::Unexpected(capture),
                    },
                })))
            },
        });

        // We save the nar reader because we want to capture the previous status.
        self.state.status_idxs.push(nar);

        self.state.queue.schedule(ShiftDrCommand {
            data: ndr.to_le_bytes().to_vec(),
            len: TapInstruction::Ndr.bits(),
            transform,
        })
    }

    /// Perform an access to a register
    fn schedule_dbg_read_and_transform(
        &mut self,
        address: u8,
        transform: TransformFn,
    ) -> DeferredResultIndex {
        let regdata = address << 1;

        self.do_nexus_op(regdata, 0, transform)
    }

    /// Perform an access to a register
    fn schedule_dbg_read(&mut self, address: u8) -> DeferredResultIndex {
        self.schedule_dbg_read_and_transform(address, transform_u32)
    }

    /// Perform an access to a register
    fn schedule_dbg_write(&mut self, address: u8, value: u32) -> DeferredResultIndex {
        let regdata = (address << 1) | 1;

        self.do_nexus_op(regdata, value, transform_noop)
    }

    fn pwr_write(&mut self, dev: PowerDevice, value: u8) -> Result<u8, XtensaError> {
        let instr = TapInstruction::from(dev);

        let capture = self
            .probe
            .write_register(instr.code(), &[value], instr.bits())?;

        let res = capture.load_le::<u8>();
        tracing::trace!("pwr_write response: {:?}", res);

        Ok(res)
    }

    pub(super) fn read_idcode(&mut self) -> Result<u32, XtensaError> {
        let instr = TapInstruction::Idcode;

        let capture = self
            .probe
            .write_register(instr.code(), &[0, 0, 0, 0], instr.bits())?;

        let res = capture.load_le::<u32>();

        tracing::debug!("idcode response: {:x?}", res);

        Ok(res)
    }

    pub(super) fn schedule_read_nexus_register<R: NexusRegister>(&mut self) -> DeferredResultIndex {
        tracing::debug!("Reading from {}", R::NAME);
        self.schedule_dbg_read(R::ADDRESS)
    }

    fn read_nexus_register<R: NexusRegister>(&mut self) -> Result<R, XtensaError> {
        let bits_reader = self.schedule_read_nexus_register::<R>();

        let bits = self.read_deferred_result(bits_reader)?.into_u32();
        let reg = R::from_bits(bits)?;
        tracing::trace!("Read: {:?}", reg);
        Ok(reg)
    }

    pub(crate) fn schedule_write_nexus_register<R: NexusRegister>(&mut self, register: R) {
        tracing::debug!("Writing {}: {:08x}", R::NAME, register.bits());
        self.schedule_dbg_write(R::ADDRESS, register.bits());
    }

    pub(crate) fn write_nexus_register<R: NexusRegister>(
        &mut self,
        register: R,
    ) -> Result<(), XtensaError> {
        self.schedule_write_nexus_register(register);
        self.execute()
    }

    pub(super) fn status(&mut self) -> Result<DebugStatus, XtensaError> {
        self.read_nexus_register::<DebugStatus>()
    }

    pub(super) fn schedule_wait_for_exec_done(&mut self) {
        let status_reader = self
            .schedule_dbg_read_and_transform(DebugStatus::ADDRESS, transform_instruction_status);

        self.state.status_idxs.push(status_reader);
    }

    /// Instructs Core to enter Core Stopped state instead of vectoring on a Debug Exception/Interrupt.
    pub(super) fn halt(&mut self) -> Result<(), XtensaError> {
        self.schedule_halt();
        self.execute()
    }

    /// Instructs Core to enter Core Stopped state instead of vectoring on a Debug Exception/Interrupt.
    pub(super) fn schedule_halt(&mut self) {
        self.schedule_write_nexus_register(DebugControlSet({
            let mut control = DebugControlBits(0);

            control.set_enable_ocd(true);
            control.set_debug_interrupt(true);

            control
        }));
        self.schedule_write_nexus_register({
            let mut status = DebugStatus(0);

            status.set_debug_pend_break(true);
            status.set_debug_int_break(true);
            status.set_exec_overrun(true);
            status.set_exec_exception(true);

            status
        });
    }

    pub(super) fn leave_ocd_mode(&mut self) -> Result<(), XtensaError> {
        // clear all clearable status bits
        self.write_nexus_register({
            let mut clear_status = DebugStatus(0);

            clear_status.set_exec_done(true);
            clear_status.set_exec_exception(true);
            clear_status.set_exec_overrun(true);
            clear_status.set_core_wrote_ddr(true);
            clear_status.set_core_read_ddr(true);
            clear_status.set_host_wrote_ddr(true);
            clear_status.set_host_read_ddr(true);
            clear_status.set_debug_pend_break(true);
            clear_status.set_debug_pend_host(true);
            clear_status.set_debug_pend_trax(true);
            clear_status.set_debug_int_break(true);
            clear_status.set_debug_int_host(true);
            clear_status.set_debug_int_trax(true);
            clear_status.set_run_stall_toggle(true);

            clear_status
        })?;

        self.write_nexus_register(DebugControlClear({
            let mut control = DebugControlBits(0);

            control.set_enable_ocd(true);
            control.set_break_in_en(true);
            control.set_break_out_en(true);

            control
        }))?;

        Ok(())
    }

    pub(super) fn resume(&mut self) -> Result<(), XtensaError> {
        tracing::debug!("resuming...");
        // Clear pending interrupts first that would re-enter us into the Stopped state
        self.schedule_write_nexus_register({
            let mut clear_status = DebugStatus(0);

            clear_status.set_debug_pend_host(true);
            clear_status.set_debug_pend_break(true);

            clear_status
        });

        self.schedule_execute_instruction(Instruction::Rfdo(0));
        match self.execute() {
            Ok(_) => Ok(()),
            // Core may just have resumed into a `waiti`
            Err(XtensaError::XdmError(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub(super) fn schedule_write_instruction(&mut self, instruction: Instruction) {
        tracing::debug!("Preparing instruction: {:?}", instruction);
        self.state.last_instruction = Some(instruction);

        match instruction.encode() {
            InstructionEncoding::Narrow(inst) => {
                self.schedule_write_nexus_register(DebugInstructionRegister(inst));
            }
        }
    }

    pub(super) fn schedule_execute_instruction(&mut self, instruction: Instruction) {
        tracing::debug!("Executing instruction: {:?}", instruction);
        self.state.last_instruction = Some(instruction);

        match instruction.encode() {
            InstructionEncoding::Narrow(inst) => {
                self.schedule_write_nexus_register(DebugInstructionAndExecRegister(inst));
            }
        }

        self.schedule_wait_for_last_instruction();
    }

    pub(super) fn schedule_read_ddr(&mut self) -> DeferredResultIndex {
        self.schedule_read_nexus_register::<DebugDataRegister>()
    }

    pub(super) fn schedule_read_ddr_and_execute(&mut self) -> DeferredResultIndex {
        let reader = self.schedule_read_nexus_register::<DebugDataAndExecRegister>();
        self.schedule_wait_for_last_instruction();

        reader
    }

    pub(super) fn schedule_write_ddr(&mut self, ddr: u32) {
        self.schedule_write_nexus_register(DebugDataRegister(ddr))
    }

    pub(super) fn schedule_write_ddr_and_execute(&mut self, ddr: u32) {
        if let Some(instruction) = self.state.last_instruction {
            tracing::debug!("Executing instruction via DDREXEC write: {:?}", instruction);
        } else {
            tracing::warn!("Writing DDREXEC without instruction");
        }

        self.schedule_write_nexus_register(DebugDataAndExecRegister(ddr));
        self.schedule_wait_for_last_instruction();
    }

    fn schedule_wait_for_last_instruction(&mut self) {
        // Assume some instructions complete practically instantly and don't waste bandwidth
        // checking their results.
        if let Some(last_instruction) = self.state.last_instruction {
            let wait = !matches!(
                last_instruction,
                Instruction::Rsr(_, _)
                    | Instruction::Wsr(_, _)
                    | Instruction::Lddr32P(_)
                    | Instruction::Sddr32P(_)
            );

            if wait {
                self.schedule_wait_for_exec_done();
            }
        }
    }

    pub fn reset_and_halt(&mut self) -> Result<(), XtensaError> {
        self.execute()?;
        self.pwr_write(PowerDevice::PowerControl, {
            let mut pwr_control = PowerControl(0);

            pwr_control.set_jtag_debug_use(true);
            pwr_control.set_debug_wakeup(true);
            pwr_control.set_mem_wakeup(true);
            pwr_control.set_core_wakeup(true);
            pwr_control.set_core_reset(true);

            pwr_control.0
        })?;
        self.halt()?;

        self.pwr_write(PowerDevice::PowerControl, {
            let mut pwr_control = PowerControl(0);

            pwr_control.set_jtag_debug_use(true);
            pwr_control.set_debug_wakeup(true);
            pwr_control.set_mem_wakeup(true);
            pwr_control.set_core_wakeup(true);

            pwr_control.0
        })?;

        Ok(())
    }

    fn clear_exception_state(&mut self) -> Result<(), XtensaError> {
        self.write_nexus_register({
            let mut status = DebugStatus(0);

            status.set_exec_exception(true);
            status.set_exec_done(true);
            status.set_exec_overrun(true);

            status
        })
    }
}

type TransformFn = fn(&ShiftDrCommand, &BitSlice) -> Result<CommandResult, ProbeRsError>;

fn transform_u32(
    _command: &ShiftDrCommand,
    capture: &BitSlice,
) -> Result<CommandResult, ProbeRsError> {
    Ok(CommandResult::U32(capture.load_le::<u32>()))
}

fn transform_noop(
    _command: &ShiftDrCommand,
    _capture: &BitSlice,
) -> Result<CommandResult, ProbeRsError> {
    Ok(CommandResult::None)
}

fn transform_instruction_status(
    _command: &ShiftDrCommand,
    capture: &BitSlice,
) -> Result<CommandResult, ProbeRsError> {
    let status = DebugStatus(capture.load_le::<u32>());

    if status.exec_overrun() {
        return Err(ProbeRsError::Xtensa(XtensaError::XdmError(
            Error::ExecOverrun,
        )));
    }
    if status.exec_exception() {
        return Err(ProbeRsError::Xtensa(XtensaError::XdmError(
            Error::ExecExeception,
        )));
    }
    if status.exec_busy() {
        return Err(ProbeRsError::Xtensa(XtensaError::XdmError(Error::ExecBusy)));
    }
    if status.exec_done() {
        return Ok(CommandResult::None);
    }

    Err(ProbeRsError::Xtensa(XtensaError::XdmError(
        Error::InstructionIgnored,
    )))
}

bitfield::bitfield! {
    #[derive(Copy, Clone)]
    pub struct PowerControl(u8);
    impl Debug;

    pub core_wakeup,    set_core_wakeup:    0;
    pub mem_wakeup,     set_mem_wakeup:     1;
    pub debug_wakeup,   set_debug_wakeup:   2;
    pub core_reset,     set_core_reset:     4;
    pub debug_reset,    set_debug_reset:    6;
    pub jtag_debug_use, set_jtag_debug_use: 7;
}

bitfield::bitfield! {
    #[derive(Copy, Clone)]
    pub struct PowerStatus(u8);
    impl Debug;

    pub core_domain_on,    _: 0;
    pub mem_domain_on,     _: 1;
    pub debug_domain_on,   _: 2;
    pub core_still_needed, _: 3;
    /// Clears bit when written as 1
    pub core_was_reset,    set_core_was_reset: 4;
    /// Clears bit when written as 1
    pub debug_was_reset,   set_debug_was_reset: 6;
}

bitfield::bitfield! {
    #[derive(Copy, Clone)]
    pub struct DebugStatus(u32);
    impl Debug;

    // Cleared by writing 1
    pub exec_done,         set_exec_done: 0;
    // Cleared by writing 1
    pub exec_exception,    set_exec_exception: 1;
    pub exec_busy,         _: 2;
    // Cleared by writing 1
    pub exec_overrun,      set_exec_overrun: 3;
    pub stopped,           _: 4;
    // Cleared by writing 1
    pub core_wrote_ddr,    set_core_wrote_ddr: 10;
    // Cleared by writing 1
    pub core_read_ddr,     set_core_read_ddr: 11;
    // Cleared by writing 1
    pub host_wrote_ddr,    set_host_wrote_ddr: 14;
    // Cleared by writing 1
    pub host_read_ddr,     set_host_read_ddr: 15;
    // Cleared by writing 1
    pub debug_pend_break,  set_debug_pend_break: 16;
    // Cleared by writing 1
    pub debug_pend_host,   set_debug_pend_host: 17;
    // Cleared by writing 1
    pub debug_pend_trax,   set_debug_pend_trax: 18;
    // Cleared by writing 1
    pub debug_int_break,   set_debug_int_break: 20;
    // Cleared by writing 1
    pub debug_int_host,    set_debug_int_host: 21;
    // Cleared by writing 1
    pub debug_int_trax,    set_debug_int_trax: 22;
    // Cleared by writing 1
    pub run_stall_toggle,  set_run_stall_toggle: 23;
    pub run_stall_sample,  _: 24;
    pub break_out_ack_iti, _: 25;
    pub break_in_iti,      _: 26;
    pub dbgmod_power_on,   _: 31;
}

/// An abstraction over all registers that can be accessed via the NAR/NDR instruction pair.
pub(crate) trait NexusRegister: Sized + Copy + Debug {
    /// NAR register address
    const ADDRESS: u8;
    const NAME: &'static str;

    fn from_bits(bits: u32) -> Result<Self, XtensaError>;
    fn bits(&self) -> u32;
}

impl NexusRegister for DebugStatus {
    const ADDRESS: u8 = NARADR_DSR;
    const NAME: &'static str = "DebugStatus";

    fn from_bits(bits: u32) -> Result<Self, XtensaError> {
        Ok(Self(bits))
    }

    fn bits(&self) -> u32 {
        self.0
    }
}

/// Writes and executes DIR.
#[derive(Copy, Clone, Debug)]
struct OcdId(u32);

impl NexusRegister for OcdId {
    const ADDRESS: u8 = NARADR_OCDID;
    const NAME: &'static str = "OCDID";

    fn from_bits(bits: u32) -> Result<Self, XtensaError> {
        Ok(Self(bits))
    }

    fn bits(&self) -> u32 {
        self.0
    }
}

bitfield::bitfield! {
    #[derive(Copy, Clone)]
    pub struct DebugControlBits(u32);
    impl Debug;

    pub enable_ocd,          set_enable_ocd         : 0;
    // R/set
    pub debug_interrupt,     set_debug_interrupt    : 1;
    pub interrupt_all_conds, set_interrupt_all_conds: 2;

    pub break_in_en,         set_break_in_en        : 16;
    pub break_out_en,        set_break_out_en       : 17;

    pub debug_sw_active,     set_debug_sw_active    : 20;
    pub run_stall_in_en,     set_run_stall_in_en    : 21;
    pub debug_mode_out_en,   set_debug_mode_out_en  : 22;

    pub break_out_ito,       set_break_out_ito      : 24;
    pub break_in_ack_ito,    set_break_in_ack_ito   : 25;
}

#[derive(Copy, Clone, Debug)]
/// Bits written as 1 are set to 1 in hardware.
pub struct DebugControlSet(pub(crate) DebugControlBits);

impl NexusRegister for DebugControlSet {
    const ADDRESS: u8 = NARADR_DCRSET;
    const NAME: &'static str = "DebugControlSet";

    fn from_bits(bits: u32) -> Result<Self, XtensaError> {
        Ok(Self(DebugControlBits(bits)))
    }

    fn bits(&self) -> u32 {
        self.0.0
    }
}

#[derive(Copy, Clone, Debug)]
/// Bits written as 1 are set to 0 in hardware.
pub struct DebugControlClear(pub(crate) DebugControlBits);

impl NexusRegister for DebugControlClear {
    const ADDRESS: u8 = NARADR_DCRCLR;
    const NAME: &'static str = "DebugControlClear";

    fn from_bits(bits: u32) -> Result<Self, XtensaError> {
        Ok(Self(DebugControlBits(bits)))
    }

    fn bits(&self) -> u32 {
        self.0.0
    }
}

/// Writes DDR.
#[derive(Copy, Clone, Debug)]
struct DebugDataRegister(u32);

impl NexusRegister for DebugDataRegister {
    const ADDRESS: u8 = NARADR_DDR;
    const NAME: &'static str = "DDR";

    fn from_bits(bits: u32) -> Result<Self, XtensaError> {
        Ok(Self(bits))
    }

    fn bits(&self) -> u32 {
        self.0
    }
}

/// Writes DDR and executes DIR on write AND READ.
#[derive(Copy, Clone, Debug)]
struct DebugDataAndExecRegister(u32);

impl NexusRegister for DebugDataAndExecRegister {
    const ADDRESS: u8 = NARADR_DDREXEC;
    const NAME: &'static str = "DDREXEC";

    fn from_bits(bits: u32) -> Result<Self, XtensaError> {
        Ok(Self(bits))
    }

    fn bits(&self) -> u32 {
        self.0
    }
}

/// Writes DIR.
#[derive(Copy, Clone, Debug)]
struct DebugInstructionRegister(u32);

impl NexusRegister for DebugInstructionRegister {
    const ADDRESS: u8 = NARADR_DIR0;
    const NAME: &'static str = "DIR0";

    fn from_bits(bits: u32) -> Result<Self, XtensaError> {
        Ok(Self(bits))
    }

    fn bits(&self) -> u32 {
        self.0
    }
}

/// Writes and executes DIR.
#[derive(Copy, Clone, Debug)]
struct DebugInstructionAndExecRegister(u32);

impl NexusRegister for DebugInstructionAndExecRegister {
    const ADDRESS: u8 = NARADR_DIR0EXEC;
    const NAME: &'static str = "DIR0EXEC";

    fn from_bits(bits: u32) -> Result<Self, XtensaError> {
        Ok(Self(bits))
    }

    fn bits(&self) -> u32 {
        self.0
    }
}
