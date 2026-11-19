// SPDX-License-Identifier: MIT

//! The serial line's transmitter.
//!
//! The PC's COM1 UART (a 16550), write-only: the kernel's log goes out of it, and input belongs
//! to the user-space console server, which drives the rest of the UART itself.

use crate::arch::x86_64::cpu::Device::Com1;
use haisuli_abi::uart16550::{
    FifoControl, LineControl, LineStatus, ModemControl, Register, DIVISOR_38400,
};

fn write(reg: Register, value: u8) {
    Com1.write8(reg as u16, value);
}

/// Sets up COM1 for output: 38400 baud, 8N1, FIFOs on, no interrupts.
pub(crate) fn init() {
    write(Register::InterruptEnable, 0);
    write(Register::LineControl, LineControl::DivisorLatch as u8);
    let [low, high] = DIVISOR_38400.to_le_bytes();
    write(Register::DIVISOR_LOW, low);
    write(Register::DIVISOR_HIGH, high);
    write(Register::LineControl, LineControl::Word8 as u8); // Also ends DLAB.
    write(
        Register::FifoControl,
        FifoControl::bits(&[
            FifoControl::Enable,
            FifoControl::ClearReceive,
            FifoControl::ClearTransmit,
            FifoControl::Trigger14,
        ]),
    );
    write(
        Register::ModemControl,
        ModemControl::bits(&[
            ModemControl::DataTerminalReady,
            ModemControl::RequestToSend,
            ModemControl::Out2,
        ]),
    );
}

/// Sends one byte, waiting until the transmitter takes it.
pub(crate) fn write_byte(b: u8) {
    while Com1.read8(Register::LineStatus as u16) & LineStatus::TransmitterEmpty as u8 == 0 {}
    write(Register::Data, b);
}
