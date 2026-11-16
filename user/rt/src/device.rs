// SPDX-License-Identifier: GPL-2.0

//! Devices, for drivers.
//!
//! A privileged program gets a device's registers granted by the kernel ([`grant`]), and then
//! reads and writes them through [`Registers`], whatever the machine keeps them in: the I/O port
//! space (the PC's devices) or memory (every arm64 device). So a driver does not depend on the
//! architecture, only on the device: the console server drives a 16550 the same way in either.
//!
//! ```ignore
//! let (info, regs) = device::grant(abi::device::CONSOLE)?;
//! sys::irq_bind(info.irq)?;
//! let status = regs.read8(LINE_STATUS);
//! ```
//!
//! Access is limited to the granted registers: an offset outside them panics. What makes the
//! accesses safe is the kernel's side of the grant: it only grants devices that cannot access
//! memory themselves (no DMA), and maps memory-mapped registers where no Rust object lives, for
//! the rest of the program.

#[cfg(target_arch = "x86_64")]
use crate::abi::device::SPACE_PORTS;
use crate::abi::device::{DeviceInfo, SPACE_MMIO};
use crate::abi::errno::ENODEV;
use crate::sys::{self, Errno, Result};

/// Where granted registers are.
#[derive(Clone, Copy, Debug)]
enum Space {
    /// I/O ports from this one on.
    #[cfg(target_arch = "x86_64")]
    Ports(u16),
    /// Memory-mapped, from this address on.
    Mmio(u64),
}

/// The registers of a device granted to the program.
#[derive(Clone, Copy, Debug)]
pub struct Registers {
    space: Space,
    /// Ports, or bytes.
    len: u64,
}

/// Describes device `device` (one of the ids in [`crate::abi::device`]).
pub fn info(device: u64) -> Result<DeviceInfo> {
    sys::device_info(device)
}

/// Gets the registers of device `device` granted (privileged programs only), with its
/// description.
///
/// Fails with `EPERM` for an unprivileged program, and with `ENODEV` if the machine has no such
/// device (or keeps its registers where this architecture cannot reach them).
pub fn grant(device: u64) -> Result<(DeviceInfo, Registers)> {
    let info = sys::device_info(device)?;
    let space = match info.space {
        #[cfg(target_arch = "x86_64")]
        SPACE_PORTS => {
            if info.base.saturating_add(info.len) > u16::MAX as u64 + 1 {
                return Err(Errno(ENODEV));
            }
            Space::Ports(sys::device_grant(device)? as u16)
        }
        SPACE_MMIO => Space::Mmio(sys::device_grant(device)?),
        // Including I/O ports where the architecture has none.
        _ => return Err(Errno(ENODEV)),
    };
    Ok((
        info,
        Registers {
            space,
            len: info.len,
        },
    ))
}

impl Registers {
    /// Checks that a `size`-byte access at `offset` lies within the registers.
    ///
    /// # Panics
    ///
    /// Panics if it does not, or if it is not aligned to its size.
    fn check(&self, offset: u64, size: u64) {
        assert!(
            offset.is_multiple_of(size)
                && offset.checked_add(size).is_some_and(|end| end <= self.len),
            "register access outside the device: offset {:#x}",
            offset
        );
    }

    /// Reads the 8-bit register at `offset`.
    ///
    /// # Panics
    ///
    /// Panics if `offset` is outside the device's registers.
    pub fn read8(&self, offset: u64) -> u8 {
        self.check(offset, 1);
        match self.space {
            #[cfg(target_arch = "x86_64")]
            // SAFETY: A port of the granted device (checked above), which cannot access memory.
            Space::Ports(base) => unsafe { crate::arch::ports::read8(base + offset as u16) },
            // SAFETY: The kernel mapped the registers at `base` for the rest of the program, and
            // no Rust object lives there; `offset` is within them (checked above).
            Space::Mmio(base) => unsafe { core::ptr::read_volatile((base + offset) as *const u8) },
        }
    }

    /// Writes the 8-bit register at `offset`.
    ///
    /// # Panics
    ///
    /// Panics if `offset` is outside the device's registers.
    pub fn write8(&self, offset: u64, value: u8) {
        self.check(offset, 1);
        match self.space {
            #[cfg(target_arch = "x86_64")]
            // SAFETY: As in `read8`.
            Space::Ports(base) => unsafe {
                crate::arch::ports::write8(base + offset as u16, value)
            },
            // SAFETY: As in `read8`.
            Space::Mmio(base) => unsafe {
                core::ptr::write_volatile((base + offset) as *mut u8, value)
            },
        }
    }

    /// Reads the 32-bit register at `offset` (a multiple of 4).
    ///
    /// # Panics
    ///
    /// Panics if `offset` is outside the device's registers, or not a multiple of 4.
    pub fn read32(&self, offset: u64) -> u32 {
        self.check(offset, 4);
        match self.space {
            #[cfg(target_arch = "x86_64")]
            // SAFETY: As in `read8`.
            Space::Ports(base) => unsafe { crate::arch::ports::read32(base + offset as u16) },
            // SAFETY: As in `read8`; the address is aligned (checked above, and the mapping is
            // page aligned).
            Space::Mmio(base) => unsafe { core::ptr::read_volatile((base + offset) as *const u32) },
        }
    }

    /// Writes the 32-bit register at `offset` (a multiple of 4).
    ///
    /// # Panics
    ///
    /// Panics if `offset` is outside the device's registers, or not a multiple of 4.
    pub fn write32(&self, offset: u64, value: u32) {
        self.check(offset, 4);
        match self.space {
            #[cfg(target_arch = "x86_64")]
            // SAFETY: As in `read8`.
            Space::Ports(base) => unsafe {
                crate::arch::ports::write32(base + offset as u16, value)
            },
            // SAFETY: As in `read32`.
            Space::Mmio(base) => unsafe {
                core::ptr::write_volatile((base + offset) as *mut u32, value)
            },
        }
    }
}
