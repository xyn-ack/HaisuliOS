// SPDX-License-Identifier: GPL-2.0

//! Starting the other CPUs.
//!
//! The boot CPU wakes every other CPU that the ACPI tables list ([`super::acpi::cpus`]) with the
//! INIT / start-up IPI sequence. A CPU wakes up in 16-bit real mode at a page below 1 MiB, so the
//! boot CPU first copies a *trampoline* there: it switches to protected mode and then to long
//! mode with the boot page tables (the kernel half that every address space shares), and calls
//! [`crate::ap_main`] on a stack of its own.
//!
//! The CPUs start one at a time: the trampoline's parameters (stack, CPU index) are shared, and
//! the boot CPU waits until each one has reached Rust code before it starts the next. A CPU that
//! takes too long is given up: it and the boot CPU race for one atomic [`Start`] state, and if
//! it comes late after all, it halts at once. Then no more CPUs are started, so that the
//! parameters a latecomer reads are never anybody else's.

use super::percpu::MAX_CPUS;
use super::x86::{segment_descriptor, Cr0, Cr4, Efer, Msr, SegmentAccess, SegmentFlag};
use super::{apic, boot, multiboot};
use core::arch::global_asm;
use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};

/// Where the trampoline runs: a page of conventional memory below 1 MiB, which nothing else
/// uses once the boot loader is done (the allocator never owns memory below 1 MiB, and the
/// boot loader's data that the kernel keeps is elsewhere: checked by [`start_cpus`]).
const TRAMPOLINE: u64 = 0x8000;
/// Bytes of each CPU's boot stack, on which it later idles.
const STACK_SIZE: usize = 16 * 1024;
/// The largest segment limit (20 bits), in 4 KiB units: the whole 4 GiB.
const FLAT_LIMIT: u32 = 0xF_FFFF;
/// The trampoline's temporary GDT: flat 32-bit code, flat data and 64-bit code, at these
/// selectors (`ap_main` loads the real GDT).
const TRAMPOLINE_CODE32: u16 = 0x08;
const TRAMPOLINE_DATA: u16 = 0x10;
const TRAMPOLINE_CODE64: u16 = 0x18;
const CODE_ACCESS: u8 = SegmentAccess::bits(&[
    SegmentAccess::Present,
    SegmentAccess::CodeOrData,
    SegmentAccess::Executable,
    SegmentAccess::ReadWrite,
]);
const DATA_ACCESS: u8 = SegmentAccess::bits(&[
    SegmentAccess::Present,
    SegmentAccess::CodeOrData,
    SegmentAccess::ReadWrite,
]);
const CODE32_SEGMENT: u64 = segment_descriptor(
    CODE_ACCESS,
    SegmentFlag::bits(&[SegmentFlag::Granularity4K, SegmentFlag::Size32]),
    FLAT_LIMIT,
);
const DATA_SEGMENT: u64 = segment_descriptor(
    DATA_ACCESS,
    SegmentFlag::bits(&[SegmentFlag::Granularity4K, SegmentFlag::Size32]),
    FLAT_LIMIT,
);
const CODE64_SEGMENT: u64 = segment_descriptor(
    CODE_ACCESS,
    SegmentFlag::bits(&[SegmentFlag::Granularity4K, SegmentFlag::LongMode]),
    FLAT_LIMIT,
);
const _: () = assert!(
    TRAMPOLINE_CODE32 == 8 && TRAMPOLINE_DATA == 16 && TRAMPOLINE_CODE64 == 24,
    "the selectors are the entries of `ap_gdt`, in order, 8 bytes each"
);
/// Opcodes of the far jumps between the modes: `ljmp ptr16:32`, with the operand-size prefix
/// that it needs in 16-bit code.
const OPERAND_SIZE_PREFIX: u8 = 0x66;
const FAR_JUMP: u8 = 0xEA;

/// How long to wait for a CPU to reach Rust code (an emulated one on a busy host can be slow).
const START_TIMEOUT_US: u32 = 2_000_000;

/// The boot stacks of the other CPUs (the boot CPU has the one from `boot`).
#[repr(C, align(16))]
struct Stack([AtomicU64; STACK_SIZE / 8]);
static STACKS: [Stack; MAX_CPUS] =
    [const { Stack([const { AtomicU64::new(0) }; STACK_SIZE / 8]) }; MAX_CPUS];

/// Where the CPU being started is (one of [`Start`]).
static STARTED: AtomicU8 = AtomicU8::new(Start::Waiting as u8);

/// The progress of the CPU being started.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Start {
    /// Not in Rust code yet.
    Waiting,
    /// In Rust code: it is the next CPU.
    Arrived,
    /// Given up by the boot CPU: if it arrives after all, it halts.
    Abandoned,
}

/// Moves the CPU being started from `Waiting` to `to`; whoever does so first decides.
fn settle(to: Start) -> bool {
    STARTED
        .compare_exchange(
            Start::Waiting as u8,
            to as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
}

fn arrived() -> bool {
    STARTED.load(Ordering::Acquire) == Start::Arrived as u8
}

extern "C" {
    static ap_trampoline_start: u8;
    static ap_trampoline_end: u8;
    static ap_cr3: u8;
    static ap_stack: u8;
    static ap_entry: u8;
    static ap_cpu: u8;
}

// The trampoline: position dependent, assembled here and run at `TRAMPOLINE`, so every address
// in it is `label - ap_trampoline_start + TRAMPOLINE`. The parameters at its end are filled in
// by `start_cpus`. The temporary GDT has 32-bit code, data and 64-bit code (see
// `TRAMPOLINE_CODE32`); `ap_main` loads the real one.
global_asm!(
    r#"
.pushsection .rodata.ap_trampoline, "a"
.code16
.global ap_trampoline_start
ap_trampoline_start:
    cli
    cld
    xorw %ax, %ax
    movw %ax, %ds
    lgdtl (ap_gdtr - ap_trampoline_start + {base})
    movl %cr0, %eax
    orl  ${cr0_pe}, %eax
    movl %eax, %cr0
    # ljmp code32:ap_protected (with a 32-bit offset).
    .byte {operand_size}, {far_jump}
    .long ap_protected - ap_trampoline_start + {base}
    .word {code32}
.code32
ap_protected:
    movw ${data}, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movl %cr4, %eax
    orl  ${cr4_pae}, %eax
    movl %eax, %cr4
    movl (ap_cr3 - ap_trampoline_start + {base}), %eax
    movl %eax, %cr3
    movl ${msr_efer}, %ecx
    rdmsr
    orl  ${efer_lme_nxe}, %eax
    wrmsr
    movl %cr0, %eax
    orl  ${cr0_pg_pe}, %eax
    movl %eax, %cr0
    # ljmp code64:ap_long.
    .byte {far_jump}
    .long ap_long - ap_trampoline_start + {base}
    .word {code64}
.code64
ap_long:
    movw ${data}, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    xorl %eax, %eax
    movw %ax, %fs
    movw %ax, %gs
    movq (ap_stack - ap_trampoline_start + {base}), %rsp
    movq (ap_cpu - ap_trampoline_start + {base}), %rdi
    movq (ap_entry - ap_trampoline_start + {base}), %rax
    xorl %ebp, %ebp
    call *%rax
    ud2
.balign 8
ap_gdt:
    .quad 0
    .quad {code32_segment}
    .quad {data_segment}
    .quad {code64_segment}
ap_gdtr:
    .word ap_gdtr - ap_gdt - 1
    .long ap_gdt - ap_trampoline_start + {base}
.balign 8
.global ap_cr3
ap_cr3: .quad 0
.global ap_stack
ap_stack: .quad 0
.global ap_entry
ap_entry: .quad 0
.global ap_cpu
ap_cpu: .quad 0
.global ap_trampoline_end
ap_trampoline_end:
.popsection
"#,
    base = const TRAMPOLINE,
    cr0_pe = const Cr0::ProtectionEnable as u32,
    cr4_pae = const Cr4::PhysicalAddressExtension as u32,
    msr_efer = const Msr::Efer as u32,
    efer_lme_nxe = const Efer::bits(&[Efer::LongMode, Efer::NoExecute]),
    cr0_pg_pe = const Cr0::bits(&[Cr0::Paging, Cr0::ProtectionEnable]),
    operand_size = const OPERAND_SIZE_PREFIX,
    far_jump = const FAR_JUMP,
    code32 = const TRAMPOLINE_CODE32,
    data = const TRAMPOLINE_DATA,
    code64 = const TRAMPOLINE_CODE64,
    code32_segment = const CODE32_SEGMENT,
    data_segment = const DATA_SEGMENT,
    code64_segment = const CODE64_SEGMENT,
    options(att_syntax)
);

/// The first Rust code of a CPU that the trampoline started.
extern "C" fn ap_start(cpu: u64) -> ! {
    if !settle(Start::Arrived) {
        super::cpu::halt(); // Too late: the boot CPU went on without it.
    }
    super::percpu::init(cpu as usize); // Before anything that takes a lock.
    crate::ap_main(cpu as usize)
}

/// The trampoline's bytes, and the offset of `label` in them.
fn trampoline() -> &'static [u8] {
    let start = &raw const ap_trampoline_start;
    let end = &raw const ap_trampoline_end as usize;
    // SAFETY: Both labels are in the same section of the kernel image, with the code and data
    // of the trampoline between them; the kernel never writes that (read-only) section.
    unsafe { core::slice::from_raw_parts(start, end - start as usize) }
}

/// Where `label` (a trampoline parameter) is in the copy at [`TRAMPOLINE`].
fn parameter(label: *const u8) -> *mut u64 {
    let offset = label as usize - &raw const ap_trampoline_start as usize;
    (TRAMPOLINE as usize + offset) as *mut u64
}

/// Writes a trampoline parameter into the copy at [`TRAMPOLINE`].
fn set_parameter(label: *const u8, value: u64) {
    // SAFETY: `label` is one of the 8-byte, 8-aligned parameters at the end of the trampoline,
    // so the address lies in the trampoline page that `start_cpus` owns (see `TRAMPOLINE`).
    unsafe { core::ptr::write_volatile(parameter(label), value) };
}

/// Starts the CPUs with APIC ids `apic_ids` (the boot CPU's among them is skipped).
///
/// Each started CPU gets the next index, from 1. Returns how many CPUs run, the boot CPU
/// included.
pub(crate) fn start_cpus(apic_ids: &[u32]) -> usize {
    let page = (TRAMPOLINE, TRAMPOLINE + 4096);
    assert!(
        multiboot::reserved_ranges().all(|(start, end)| end <= page.0 || page.1 <= start),
        "the boot loader's data is where the CPU trampoline goes"
    );
    let code = trampoline();
    assert!(code.len() <= 4096);
    // SAFETY: The trampoline page is conventional memory below 1 MiB that nothing else uses
    // (see `TRAMPOLINE`; checked above against the boot loader's data), and `code` is a
    // distinct part of the kernel image.
    unsafe {
        core::ptr::copy_nonoverlapping(code.as_ptr(), TRAMPOLINE as *mut u8, code.len());
    }
    set_parameter(&raw const ap_cr3, boot::kernel_pml4());
    set_parameter(&raw const ap_entry, ap_start as *const () as u64);
    let me = apic::id();
    let mut running = 1;
    for &id in apic_ids.iter().filter(|&&id| id != me) {
        if running == MAX_CPUS {
            break;
        }
        STARTED.store(Start::Waiting as u8, Ordering::Release);
        let stack = &STACKS[running].0;
        set_parameter(
            &raw const ap_stack,
            stack.as_ptr() as u64 + STACK_SIZE as u64,
        );
        set_parameter(&raw const ap_cpu, running as u64);
        apic::start_cpu(id, TRAMPOLINE, arrived);
        let mut waited = 0;
        while !arrived() && waited < START_TIMEOUT_US {
            apic::wait_us(1000);
            waited += 1000;
        }
        if arrived() || !settle(Start::Abandoned) {
            running += 1;
        } else {
            crate::println!("[kernel] CPU with APIC id {} did not start", id);
            break; // It may still come: then these parameters must stay its own.
        }
    }
    running
}
