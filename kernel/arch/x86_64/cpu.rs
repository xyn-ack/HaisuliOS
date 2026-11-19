// SPDX-License-Identifier: MIT

//! CPU instructions.
//!
//! Privileged x86_64 instructions, each behind a safe interface whose restrictions make it safe.

use super::x86::Msr;
use core::arch::asm;
use core::sync::atomic::AtomicU64;
use haisuli_abi::uart16550;

// ---- Port I/O ---------------------------------------------------------------

/// The legacy devices the kernel talks to.
///
/// None of them can access memory (no DMA), so reading and writing their
/// registers cannot break memory safety. Access is limited to each device's
/// own register range: the register functions panic for any other `reg`.
#[derive(Clone, Copy)]
pub(crate) enum Device {
    /// 16550 UART, 8 registers.
    Com1,
    /// 8259 interrupt controllers: command + data register each.
    Pic1,
    Pic2,
    /// POST diagnostic port, written only to wait a little.
    PostCode,
    /// 8254 programmable interval timer: three counters and the mode register. Only counter 2
    /// is used, to measure time while the kernel starts the other CPUs.
    Pit,
    /// System control port B: the gate of PIT counter 2 (bit 0) and its output (bit 5).
    SystemControl,
    /// QEMU's isa-debug-exit device (no-op on real hardware).
    DebugExit,
}

impl Device {
    /// The device's I/O ports: the first, and how many.
    pub(crate) const fn ports(self) -> (u16, u16) {
        match self {
            // The standard PC I/O ports of each device.
            Device::Com1 => (0x3F8, uart16550::REGISTERS as u16),
            Device::Pic1 => (0x20, 2),
            Device::Pic2 => (0xA0, 2),
            Device::PostCode => (0x80, 1),
            Device::Pit => (0x40, 4),
            Device::SystemControl => (0x61, 1),
            // QEMU: `-device isa-debug-exit,iobase=0xf4,iosize=0x04`.
            Device::DebugExit => (0xF4, 4),
        }
    }

    fn port(self, reg: u16) -> u16 {
        let (base, len) = self.ports();
        assert!(reg < len, "register outside the device's port range");
        base + reg
    }

    pub(crate) fn read8(self, reg: u16) -> u8 {
        let port = self.port(reg);
        let v: u8;
        // SAFETY: `port` belongs to a device from the list above, whose
        // registers cannot affect memory.
        unsafe {
            asm!("in al, dx", out("al") v, in("dx") port, options(nomem, nostack, preserves_flags))
        };
        v
    }

    pub(crate) fn write8(self, reg: u16, v: u8) {
        let port = self.port(reg);
        // SAFETY: As in `read8`.
        unsafe {
            asm!("out dx, al", in("dx") port, in("al") v, options(nomem, nostack, preserves_flags))
        };
    }

    pub(crate) fn write32(self, reg: u16, v: u32) {
        let port = self.port(reg);
        // SAFETY: As in `read8`.
        unsafe {
            asm!(
                "out dx, eax",
                in("dx") port,
                in("eax") v,
                options(nomem, nostack, preserves_flags),
            )
        };
    }
}

// ---- Interrupts and halting -------------------------------------------------

/// Stop this CPU for good.
pub(crate) fn halt() -> ! {
    loop {
        // SAFETY: Disabling interrupts and halting touches no memory.
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) };
    }
}

/// Enable interrupts, sleep until one has been handled, disable them again.
/// `sti` takes effect after the next instruction, so an interrupt cannot
/// slip in between and be missed by `hlt`.
pub(crate) fn wait_for_interrupt() {
    // SAFETY: Interrupt handlers save and restore the complete register
    // state. No `nomem`: handlers change kernel state, so the compiler must
    // not cache memory across this point.
    unsafe { asm!("sti", "hlt", "cli", options(nostack)) };
}

// ---- Control registers -------------------------------------------------------

/// The faulting linear address of the last page fault (CR2).
pub(crate) fn fault_address() -> u64 {
    let v: u64;
    // SAFETY: Reading CR2 has no side effects.
    unsafe { asm!("mov {}, cr2", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/// Switches to the page tables at physical address `pml4` (CR3).
///
/// The tables themselves are maintained by [`crate::vmm`], which always
/// installs the shared kernel mapping.
///
/// # Panics
///
/// Panics unless [`super::phys::is_page_table_root`] accepts `pml4`.
pub(crate) fn set_page_table(pml4: u64) {
    assert!(
        super::phys::is_page_table_root(pml4),
        "not a page table: {:#x}",
        pml4
    );
    // SAFETY: `pml4` is the boot page table or an allocator-owned frame
    // prepared by `vmm`, both of which map the kernel identically.
    unsafe { asm!("mov cr3, {}", in(reg) pml4, options(nostack, preserves_flags)) };
}

/// The page tables this CPU uses (CR3).
pub(crate) fn page_table() -> u64 {
    let v: u64;
    // SAFETY: Reading CR3 has no side effects.
    unsafe { asm!("mov {}, cr3", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

// ---- Descriptor tables -------------------------------------------------------

#[repr(C, packed)]
struct DescriptorPointer {
    limit: u16,
    base: u64,
}

/// Loads the GDT, the kernel's segment selectors and the task register.
///
/// CS, DS, ES and SS are reloaded; FS and GS are left alone: loading a selector into GS would
/// reset the GS base, which points at this CPU's [`super::percpu`] block (and neither is used
/// as a segment in 64-bit mode). The table lives forever (`'static`).
pub(crate) fn load_gdt(gdt: &'static [AtomicU64], code: u16, data: u16, tss: u16) {
    let ptr = DescriptorPointer {
        limit: (size_of_val(gdt) - 1) as u16,
        base: gdt.as_ptr() as u64,
    };
    // SAFETY: The table outlives the CPU's use of it, and the selectors
    // name the kernel code/data and TSS descriptors the caller put in it.
    // CS is reloaded with a far return; the stack is unchanged.
    unsafe {
        asm!(
            "lgdt [{ptr}]",
            "push {code}",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "retfq",
            "2:",
            "mov ds, {data:x}",
            "mov es, {data:x}",
            "mov ss, {data:x}",
            "ltr {tss:x}",
            ptr = in(reg) &ptr,
            code = in(reg) code as u64,
            data = in(reg) data as u64,
            tss = in(reg) tss as u64,
            tmp = out(reg) _,
        );
    }
}

/// Loads the interrupt descriptor table (16-byte gates, two words each).
pub(crate) fn load_idt(idt: &'static [AtomicU64]) {
    let ptr = DescriptorPointer {
        limit: (size_of_val(idt) - 1) as u16,
        base: idt.as_ptr() as u64,
    };
    // SAFETY: The table lives forever; gates are filled in by `idt` with
    // the entry stubs of `arch::entry`.
    unsafe { asm!("lidt [{}]", in(reg) &ptr, options(readonly, nostack, preserves_flags)) };
}

// ---- Model-specific registers --------------------------------------------------

/// Reads a model-specific register.
pub(crate) fn rdmsr(msr: Msr) -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: Reading the MSRs in `Msr` has no side effects.
    unsafe {
        asm!(
            "rdmsr",
            in("ecx") msr as u32,
            out("eax") lo,
            out("edx") hi,
            options(nomem, nostack, preserves_flags),
        )
    };
    ((hi as u64) << 32) | lo as u64
}

/// Writes a model-specific register.
///
/// Only the hardware layer may: [`super::entry`] points the system call MSRs at its own entry
/// stub, [`super::percpu`] points the GS base at a CPU's own block and [`super::apic`] enables
/// the local APIC at the address where the boot page tables map it.
pub(super) fn wrmsr(msr: Msr, v: u64) {
    // SAFETY: See above; callers write values that describe the kernel's
    // own layout (GDT, entry points, per-CPU blocks, APIC mapping).
    unsafe {
        asm!("wrmsr", in("ecx") msr as u32, in("eax") v as u32, in("edx") (v >> 32) as u32,
             options(nomem, nostack, preserves_flags))
    };
}
