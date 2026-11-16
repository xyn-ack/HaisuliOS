// SPDX-License-Identifier: GPL-2.0

//! The PIT, as a stopwatch.
//!
//! The PC's 8254 programmable interval timer, used only to measure time while the kernel boots:
//! its counter 2 can be polled, so no interrupt is needed. The local APIC timer is calibrated
//! against it, and the delays of waking the other CPUs come from it.

use crate::arch::cpu::Device;

/// The PIT's input clock, in Hz.
const PIT_HZ: u64 = 1_193_182;
/// PIT registers: counter 2's data, and the mode register.
const PIT_COUNTER2: u16 = 2;
const PIT_MODE: u16 = 3;
/// Mode register: counter 2, low byte then high byte, mode 0 (interrupt on terminal count).
const PIT_COUNTER2_ONE_SHOT: u8 = 0b1011_0000;
/// System control port B: counter 2's gate, the speaker, and counter 2's output.
const GATE2: u8 = 1 << 0;
const SPEAKER: u8 = 1 << 1;
const OUT2: u8 = 1 << 5;
/// The longest wait counter 2 can time in one go (16 bits at `PIT_HZ`).
const PIT_MAX_US: u32 = 50_000;

/// Busy-waits for about `us` microseconds.
pub(crate) fn wait_us(mut us: u32) {
    while us > 0 {
        let step = us.min(PIT_MAX_US);
        pit_wait(step);
        us -= step;
    }
}

fn pit_wait(us: u32) {
    let count = (PIT_HZ * us as u64 / 1_000_000).clamp(1, 0xFFFF) as u16;
    let port_b = Device::SystemControl.read8(0) & !(GATE2 | SPEAKER);
    Device::SystemControl.write8(0, port_b);
    Device::Pit.write8(PIT_MODE, PIT_COUNTER2_ONE_SHOT);
    let [low, high] = count.to_le_bytes();
    Device::Pit.write8(PIT_COUNTER2, low);
    Device::Pit.write8(PIT_COUNTER2, high);
    Device::SystemControl.write8(0, port_b | GATE2);
    while Device::SystemControl.read8(0) & OUT2 == 0 {
        core::hint::spin_loop();
    }
    Device::SystemControl.write8(0, port_b);
}
