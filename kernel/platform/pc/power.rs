// SPDX-License-Identifier: GPL-2.0

//! Powering the machine off.
//!
//! Asks QEMU's `isa-debug-exit` device to end the emulation; `code` becomes QEMU's exit status
//! (shifted: the host sees `code << 1 | 1`). On other machines the write does nothing, and the
//! caller halts.

use crate::arch::cpu::Device;

/// Powers the machine off, as far as it can (see the module documentation).
pub(crate) fn off(code: u32) {
    Device::DebugExit.write32(0, code);
}
