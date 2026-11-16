// SPDX-License-Identifier: GPL-2.0

//! Devices for user-space drivers.
//!
//! The kernel drives no devices for user space; it only tells a driver about one and gives it the
//! device's registers. The machine describes its devices ([`Machine::device`]), by well-known ids
//! ([`haisuli_abi::device`]): what each one is, where its registers are and which interrupt line
//! it raises. A driver asks with `device_info`, gets the registers with `device_grant` (if it is
//! privileged) and binds the interrupt line with `irq_bind` (see [`crate::irq`]).
//!
//! Only the devices the machine lists can be granted, and only their own registers: a driver
//! never gets anything else of the machine, nor memory of the kernel or other tasks.

use crate::platform::{Board, Machine, Registers};
use crate::{task, vmm};
use haisuli_abi::device::{DeviceInfo, DEVICE_INFO_LEN, NO_IRQ, SPACE_PORTS};
use haisuli_abi::errno::{EFAULT, EINVAL, ENODEV, EPERM};
use haisuli_abi::PROC_NAME_LEN;

/// Writes the description of device `id` to `buf[..len]` (the `device_info` system call).
pub(crate) fn info(id: u64, buf: u64, len: u64) -> i64 {
    let Some(device) = Board::device(id) else {
        return -ENODEV;
    };
    if len < DEVICE_INFO_LEN as u64 {
        return -EINVAL;
    }
    let (space, base, count) = match device.registers {
        Registers::Ports { base, count } => (SPACE_PORTS, base, count),
    };
    let mut name = [0u8; PROC_NAME_LEN];
    let n = device.name.len().min(PROC_NAME_LEN - 1);
    name[..n].copy_from_slice(&device.name.as_bytes()[..n]);
    let info = DeviceInfo {
        kind: device.kind,
        space,
        base,
        len: count,
        irq: device.irq.map_or(NO_IRQ, |line| line as u64),
        name,
    };
    if vmm::copy_to_user(task::current_space(), buf, &info.to_bytes()) {
        0
    } else {
        -EFAULT
    }
}

/// Gives the calling, privileged task the registers of device `id` (the `device_grant` system
/// call): I/O ports become usable, and the first one is returned.
pub(crate) fn grant(id: u64) -> i64 {
    if !task::current_privileged() {
        return -EPERM;
    }
    let Some(device) = Board::device(id) else {
        return -ENODEV;
    };
    match device.registers {
        Registers::Ports { base, count } => match task::allow_ports(base..base + count) {
            Ok(()) => base as i64,
            Err(e) => e,
        },
    }
}
