// SPDX-License-Identifier: GPL-2.0

//! x86_64 architecture definitions.
//!
//! Names for the numbers the CPU itself defines: flag and control register bits, model-specific
//! registers, privilege levels, exception vectors, page-table entries and the descriptors of the
//! GDT and IDT.
//!
//! This is pure data: nothing here touches the hardware. Flag enums have a `bits` function that
//! combines a list of flags into the register value, e.g. `RFlag::bits(&[RFlag::Interrupt])`.
//! References are to the Intel SDM, volume 3.

/// Declares an enum of flags (bits or bit fields of one register) with a `bits` function that
/// combines several of them into the register value.
macro_rules! flags {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident: $ty:ty {
            $($(#[$variant_meta:meta])* $variant:ident = $value:expr,)*
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq)]
        #[repr($ty)]
        $vis enum $name {
            $($(#[$variant_meta])* $variant = $value,)*
        }

        impl $name {
            /// The register value with exactly `flags` set.
            $vis const fn bits(flags: &[$name]) -> $ty {
                let mut bits = 0;
                let mut i = 0;
                while i < flags.len() {
                    bits |= flags[i] as $ty;
                    i += 1;
                }
                bits
            }
        }
    };
}
pub(crate) use flags;

// ---- Privilege levels ------------------------------------------------------------

/// A privilege level ("ring"); HaisuliOS uses only rings 0 and 3.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum PrivilegeLevel {
    /// Ring 0: the kernel.
    Kernel = 0,
    /// Ring 3: user mode.
    User = 3,
}

impl PrivilegeLevel {
    /// The bits of a segment selector that hold a privilege level (its RPL).
    pub(crate) const SELECTOR_MASK: u64 = 3;

    /// The level in the low bits of `selector`; for the code segment saved on
    /// an interrupt, the level the CPU was interrupted at.
    pub(crate) fn of_selector(selector: u64) -> PrivilegeLevel {
        if selector & Self::SELECTOR_MASK == PrivilegeLevel::User as u64 {
            PrivilegeLevel::User
        } else {
            PrivilegeLevel::Kernel
        }
    }
}

// ---- Flags and control registers ---------------------------------------------------

flags! {
    /// RFLAGS bits (SDM 2.3).
    pub(crate) enum RFlag: u64 {
        /// CF: carry.
        Carry = 1 << 0,
        /// Bit 1, which is reserved and always set.
        Reserved1 = 1 << 1,
        /// PF: parity.
        Parity = 1 << 2,
        /// AF: auxiliary carry ("adjust").
        Adjust = 1 << 4,
        /// ZF: zero.
        Zero = 1 << 6,
        /// SF: sign.
        Sign = 1 << 7,
        /// TF: single-step trap.
        Trap = 1 << 8,
        /// IF: maskable interrupts enabled.
        Interrupt = 1 << 9,
        /// DF: string instructions count down.
        Direction = 1 << 10,
        /// OF: overflow.
        Overflow = 1 << 11,
    }
}

flags! {
    /// CR0 bits (SDM 2.5).
    pub(crate) enum Cr0: u32 {
        /// PE: protected mode.
        ProtectionEnable = 1 << 0,
        /// PG: paging.
        Paging = 1 << 31,
    }
}

flags! {
    /// CR4 bits (SDM 2.5).
    pub(crate) enum Cr4: u32 {
        /// PAE: 64-bit page-table entries; required for long mode.
        PhysicalAddressExtension = 1 << 5,
    }
}

/// Model-specific registers the kernel uses (SDM 2.2.1 and vol. 4).
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum Msr {
    /// IA32_APIC_BASE: the local APIC's physical address and global enable bit.
    ApicBase = 0x1B,
    /// IA32_EFER: extended features (see [`Efer`]).
    Efer = 0xC000_0080,
    /// IA32_STAR: the segment selectors of `syscall` / `sysret`.
    Star = 0xC000_0081,
    /// IA32_LSTAR: the 64-bit `syscall` entry point.
    LStar = 0xC000_0082,
    /// IA32_FMASK: the RFLAGS bits that `syscall` clears.
    SfMask = 0xC000_0084,
    /// IA32_GS_BASE: the GS segment base. In the kernel, this CPU's [`super::percpu`] block.
    GsBase = 0xC000_0101,
    /// IA32_KERNEL_GS_BASE: what `swapgs` exchanges with [`Msr::GsBase`] (the user GS base,
    /// while in the kernel).
    KernelGsBase = 0xC000_0102,
}

flags! {
    /// IA32_EFER bits.
    pub(crate) enum Efer: u32 {
        /// SCE: the `syscall` / `sysret` instructions.
        Syscall = 1 << 0,
        /// LME: long mode (active once paging is on).
        LongMode = 1 << 8,
        /// NXE: the no-execute bit of page-table entries.
        NoExecute = 1 << 11,
    }
}

// ---- Exceptions --------------------------------------------------------------------

/// The CPU's exception vectors (SDM 6.3.1); vectors below 32 are reserved for them.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Exception {
    /// #DE.
    DivideError = 0,
    /// #DB.
    Debug = 1,
    /// NMI.
    NonMaskableInterrupt = 2,
    /// #BP.
    Breakpoint = 3,
    /// #OF.
    Overflow = 4,
    /// #BR.
    BoundRangeExceeded = 5,
    /// #UD.
    InvalidOpcode = 6,
    /// #NM.
    DeviceNotAvailable = 7,
    /// #DF, which gets its own stack (see [`super::gdt::DOUBLE_FAULT_IST`]).
    DoubleFault = 8,
    /// Vector 9, on CPUs before the 486.
    CoprocessorSegmentOverrun = 9,
    /// #TS.
    InvalidTss = 10,
    /// #NP.
    SegmentNotPresent = 11,
    /// #SS.
    StackSegmentFault = 12,
    /// #GP.
    GeneralProtection = 13,
    /// #PF; CR2 holds the faulting address.
    PageFault = 14,
    /// #MF.
    X87FloatingPoint = 16,
    /// #AC.
    AlignmentCheck = 17,
    /// #MC.
    MachineCheck = 18,
    /// #XM.
    SimdFloatingPoint = 19,
    /// #VE.
    Virtualization = 20,
    /// #CP.
    ControlProtection = 21,
    /// #HV.
    HypervisorInjection = 28,
    /// #VC.
    VmmCommunication = 29,
    /// #SX.
    Security = 30,
}

impl Exception {
    /// Number of vectors reserved for exceptions; interrupts come after them.
    pub(crate) const VECTORS: usize = 32;

    /// The exception with vector `vector`, if it is one.
    pub(crate) fn from_vector(vector: u64) -> Option<Exception> {
        use Exception::*;
        Some(match vector {
            0 => DivideError,
            1 => Debug,
            2 => NonMaskableInterrupt,
            3 => Breakpoint,
            4 => Overflow,
            5 => BoundRangeExceeded,
            6 => InvalidOpcode,
            7 => DeviceNotAvailable,
            8 => DoubleFault,
            9 => CoprocessorSegmentOverrun,
            10 => InvalidTss,
            11 => SegmentNotPresent,
            12 => StackSegmentFault,
            13 => GeneralProtection,
            14 => PageFault,
            16 => X87FloatingPoint,
            17 => AlignmentCheck,
            18 => MachineCheck,
            19 => SimdFloatingPoint,
            20 => Virtualization,
            21 => ControlProtection,
            28 => HypervisorInjection,
            29 => VmmCommunication,
            30 => Security,
            _ => return None,
        })
    }

    /// A human-readable name.
    pub(crate) fn name(self) -> &'static str {
        use Exception::*;
        match self {
            DivideError => "divide error",
            Debug => "debug",
            NonMaskableInterrupt => "non-maskable interrupt",
            Breakpoint => "breakpoint",
            Overflow => "overflow",
            BoundRangeExceeded => "bound range exceeded",
            InvalidOpcode => "invalid opcode",
            DeviceNotAvailable => "device not available",
            DoubleFault => "double fault",
            CoprocessorSegmentOverrun => "coprocessor segment overrun",
            InvalidTss => "invalid TSS",
            SegmentNotPresent => "segment not present",
            StackSegmentFault => "stack-segment fault",
            GeneralProtection => "general protection fault",
            PageFault => "page fault",
            X87FloatingPoint => "x87 floating-point error",
            AlignmentCheck => "alignment check",
            MachineCheck => "machine check",
            SimdFloatingPoint => "SIMD floating-point error",
            Virtualization => "virtualization exception",
            ControlProtection => "control protection exception",
            HypervisorInjection => "hypervisor injection exception",
            VmmCommunication => "VMM communication exception",
            Security => "security exception",
        }
    }
}

// ---- Paging ------------------------------------------------------------------------

flags! {
    /// Page-table entry bits (SDM 4.5).
    pub(crate) enum PageFlag: u64 {
        /// P: the entry is in use.
        Present = 1 << 0,
        /// R/W: writes are allowed.
        Writable = 1 << 1,
        /// U/S: user mode may access it.
        User = 1 << 2,
        /// PWT: write-through caching.
        WriteThrough = 1 << 3,
        /// PCD: caching disabled (device registers).
        CacheDisable = 1 << 4,
        /// PS, in a page-directory entry: it maps a 2 MiB page.
        Huge = 1 << 7,
        /// An available bit (ignored by the CPU), which HaisuliOS sets in the entries that map
        /// device registers rather than memory.
        Device = 1 << 9,
        /// XD: instruction fetches fault (needs [`Efer::NoExecute`]).
        NoExecute = 1 << 63,
    }
}

// ---- Segment and gate descriptors ----------------------------------------------------

flags! {
    /// The access byte of a code or data segment descriptor (SDM 3.4.5).
    pub(crate) enum SegmentAccess: u8 {
        /// Code: readable. Data: writable.
        ///
        /// (Bit 0, "accessed", is the CPU's to set.)
        ReadWrite = 1 << 1,
        /// A code segment (otherwise data).
        Executable = 1 << 3,
        /// S: a code or data segment (otherwise a system segment).
        CodeOrData = 1 << 4,
        /// DPL 3: a user-mode segment.
        User = (PrivilegeLevel::User as u8) << 5,
        /// P: the descriptor is valid.
        Present = 1 << 7,
    }
}

flags! {
    /// The flags nibble of a segment descriptor (SDM 3.4.5).
    pub(crate) enum SegmentFlag: u8 {
        /// L: a 64-bit code segment.
        LongMode = 1 << 1,
        /// D/B: a 32-bit segment.
        Size32 = 1 << 2,
        /// G: the limit counts 4 KiB units.
        Granularity4K = 1 << 3,
    }
}

/// A code or data segment descriptor with base 0 (SDM 3.4.5).
///
/// `access` and `flags` come from [`SegmentAccess::bits`] and
/// [`SegmentFlag::bits`]; `limit` has 20 bits. In long mode the CPU ignores
/// base and limit for code and data, but not the access byte and flags.
pub(crate) const fn segment_descriptor(access: u8, flags: u8, limit: u32) -> u64 {
    let limit = limit as u64;
    (limit & 0xFFFF) | (access as u64) << 40 | ((limit >> 16) & 0xF) << 48 | (flags as u64) << 52
}

/// The type field of a system-segment descriptor (SDM 3.5).
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum SystemSegment {
    /// An available (not busy) 64-bit TSS.
    AvailableTss64 = 0x9,
}

/// A 16-byte system-segment descriptor (a TSS in HaisuliOS): two GDT entries.
pub(crate) const fn system_descriptor(kind: SystemSegment, base: u64, limit: u32) -> [u64; 2] {
    let limit = limit as u64;
    let access = SegmentAccess::Present as u64 | kind as u64;
    let low = (limit & 0xFFFF)
        | (base & 0xFF_FFFF) << 16
        | access << 40
        | ((limit >> 16) & 0xF) << 48
        | ((base >> 24) & 0xFF) << 56;
    [low, base >> 32]
}

/// The type of an IDT gate (SDM 6.14.1).
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum GateType {
    /// Interrupts stay disabled while the handler runs.
    Interrupt = 0xE,
}

/// A 16-byte IDT gate: enter `handler` through code segment `selector`, on
/// interrupt-stack-table stack `ist` (0 = none), callable with `int` from
/// privilege level `dpl`.
pub(crate) const fn gate_descriptor(
    handler: u64,
    selector: u64,
    ist: u8,
    kind: GateType,
    dpl: PrivilegeLevel,
) -> [u64; 2] {
    let access = SegmentAccess::Present as u64 | (dpl as u64) << 5 | kind as u64;
    let low = (handler & 0xFFFF)
        | selector << 16
        | (ist as u64) << 32
        | access << 40
        | ((handler >> 16) & 0xFFFF) << 48;
    [low, handler >> 32]
}
