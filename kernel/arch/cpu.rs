// SPDX-License-Identifier: GPL-2.0

//! CPU instructions.
//!
//! Privileged x86_64 instructions, each behind a safe interface whose restrictions make it safe.

use core::arch::asm;
use core::sync::atomic::AtomicU64;

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
    /// QEMU's isa-debug-exit device (no-op on real hardware).
    DebugExit,
}

impl Device {
    const fn ports(self) -> (u16, u16) {
        match self {
            Device::Com1 => (0x3F8, 8),
            Device::Pic1 => (0x20, 2),
            Device::Pic2 => (0xA0, 2),
            Device::PostCode => (0x80, 1),
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

/// Asks QEMU (`isa-debug-exit`) to power off.
///
/// This is a no-op on other machines.
pub(crate) fn qemu_exit(code: u32) {
    Device::DebugExit.write32(0, code);
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

// ---- Descriptor tables -------------------------------------------------------

#[repr(C, packed)]
struct DescriptorPointer {
    limit: u16,
    base: u64,
}

/// Loads the GDT, the kernel's segment selectors and the task register.
///
/// Every segment register is reloaded. The table lives forever (`'static`).
pub(crate) fn load_gdt(gdt: &'static [AtomicU64], code: u16, data: u16, tss: u16) {
    let ptr = DescriptorPointer {
        limit: (gdt.len() * 8 - 1) as u16,
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
            "xor {tmp:e}, {tmp:e}",
            "mov fs, {tmp:x}",
            "mov gs, {tmp:x}",
            "ltr {tss:x}",
            ptr = in(reg) &ptr,
            code = in(reg) code as u64,
            data = in(reg) data as u64,
            tss = in(reg) tss as u64,
            tmp = out(reg) _,
        );
    }
}

/// Load the interrupt descriptor table (256 gates of 16 bytes).
pub(crate) fn load_idt(idt: &'static [AtomicU64; 512]) {
    let ptr = DescriptorPointer {
        limit: (256 * 16 - 1) as u16,
        base: idt.as_ptr() as u64,
    };
    // SAFETY: The table lives forever; gates are filled in by `idt` with
    // the entry stubs of `arch::entry`.
    unsafe { asm!("lidt [{}]", in(reg) &ptr, options(readonly, nostack, preserves_flags)) };
}

// ---- Model-specific registers --------------------------------------------------

pub(crate) const MSR_EFER: u32 = 0xC000_0080;
pub(crate) const MSR_STAR: u32 = 0xC000_0081;
pub(crate) const MSR_LSTAR: u32 = 0xC000_0082;
pub(crate) const MSR_SFMASK: u32 = 0xC000_0084;

pub(crate) fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: Reading the MSRs above has no side effects.
    unsafe {
        asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") lo,
            out("edx") hi,
            options(nomem, nostack, preserves_flags),
        )
    };
    ((hi as u64) << 32) | lo as u64
}

/// Writes one of the system call MSRs.
///
/// Only these can be written, and only by [`super::entry`], which points them
/// at its own entry stub.
///
/// # Panics
///
/// Panics if `msr` is not one of the system call MSRs.
pub(super) fn wrmsr(msr: u32, v: u64) {
    assert!(matches!(msr, MSR_EFER | MSR_STAR | MSR_LSTAR | MSR_SFMASK));
    // SAFETY: See above; callers write values that describe the kernel's
    // GDT layout and syscall entry point.
    unsafe {
        asm!("wrmsr", in("ecx") msr, in("eax") v as u32, in("edx") (v >> 32) as u32,
             options(nomem, nostack, preserves_flags))
    };
}
