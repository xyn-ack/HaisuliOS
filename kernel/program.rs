// SPDX-License-Identifier: GPL-2.0

//! Programs.
//!
//! Builds the address space that starts a program: its image, its stack and its arguments.
//!
//! Every task starts this way: the boot modules, the programs that the task manager starts from
//! the file system (`task_spawn`) and the ones a task switches to (`task_exec`). The arguments
//! come in an *argument block* ([`haisuli_abi::exec`]); the program finds them on its stack in the
//! System V x86_64 layout, as on Linux, so a libc ported later finds them where it expects them.

use crate::arch::{UserContext, UserRegs};
use crate::{elf, vmm};
use haisuli_abi::errno::{E2BIG, EINVAL, ENOENT, ENOEXEC, ENOMEM};
use haisuli_abi::exec::{HEADER, STRINGS_MAX};
use haisuli_abi::mman::{PROT_READ, PROT_WRITE};

/// Pages of a program's stack.
const STACK_PAGES: u64 = 8;
const PAGE_SIZE: u64 = 4096;
/// `AT_NULL`: the type of the auxiliary vector's last entry.
const AT_NULL: u64 = 0;
/// Words of the initial stack besides the `argv` and `envp` pointers: `argc`, the null pointers
/// ending `argv` and `envp`, and the `AT_NULL` entry (type and value).
const FIXED_WORDS: usize = 5;

/// Why a program could not be started: an error number and what went wrong.
pub(crate) type Error = (i64, &'static str);

/// A parsed argument block: the program's path, arguments and environment.
pub(crate) struct Args<'a> {
    path: &'a [u8],
    argc: usize,
    envc: usize,
    /// The arguments and then the environment strings, each NUL-terminated.
    strings: &'a [u8],
}

impl<'a> Args<'a> {
    /// Parses an argument block; every string must be NUL-terminated.
    pub(crate) fn parse(block: &'a [u8]) -> Result<Args<'a>, Error> {
        const BAD: Error = (-EINVAL, "malformed argument block");
        let word = |i: usize| {
            let mut w = [0u8; 8];
            w.copy_from_slice(block.get(i * 8..i * 8 + 8)?);
            Some(u64::from_ne_bytes(w) as usize)
        };
        let (Some(argc), Some(envc)) = (word(0), word(1)) else {
            return Err(BAD);
        };
        if argc.saturating_add(envc) > STRINGS_MAX {
            return Err((-E2BIG, "too many arguments"));
        }
        let rest = &block[HEADER..];
        let path_end = rest.iter().position(|&b| b == 0).ok_or(BAD)?;
        let strings = &rest[path_end + 1..];
        let terminated = strings.last().is_none_or(|&b| b == 0);
        if !terminated || strings.iter().filter(|&&b| b == 0).count() != argc + envc {
            return Err(BAD);
        }
        if path_end == 0 {
            return Err((-ENOENT, "empty path"));
        }
        Ok(Args {
            path: &rest[..path_end],
            argc,
            envc,
            strings,
        })
    }

    /// The path of the program.
    pub(crate) fn path(&self) -> &'a [u8] {
        self.path
    }

    /// The program's name: the last component of its path.
    pub(crate) fn name(&self) -> &'a [u8] {
        self.path.rsplit(|&b| b == b'/').next().unwrap_or(self.path)
    }
}

/// Builds a new address space running `image` with `args`.
///
/// Returns its page tables and the registers to enter user mode with. Nothing
/// is left behind on error.
pub(crate) fn load(image: &[u8], args: &Args<'_>) -> Result<(u64, UserContext), Error> {
    let pml4 = vmm::new_address_space().ok_or((-ENOMEM, "out of memory"))?;
    let setup = || -> Result<UserContext, Error> {
        let entry = elf::load(pml4, image).map_err(|why| (-ENOEXEC, why))?;
        let stack = STACK_PAGES * PAGE_SIZE;
        let flags = vmm::user_flags(PROT_READ | PROT_WRITE);
        vmm::map_init(pml4, vmm::USER_STACK_TOP - stack, stack, &[], flags)
            .map_err(|_| (-ENOMEM, "out of memory"))?;
        Ok(UserContext::new_program(entry, push_args(pml4, args)?))
    };
    match setup() {
        Ok(frame) => Ok((pml4, frame)),
        Err(e) => {
            vmm::destroy_user_space(pml4);
            Err(e)
        }
    }
}

/// Writes the initial stack (see [`haisuli_abi::exec`]) and returns the stack pointer.
fn push_args(pml4: u64, args: &Args<'_>) -> Result<u64, Error> {
    const NO_ROOM: Error = (-E2BIG, "arguments do not fit on the stack");
    let top = vmm::USER_STACK_TOP;
    let strings = (top - args.strings.len() as u64) & !7;
    let words = FIXED_WORDS + args.argc + args.envc;
    // 16-byte aligned, as the ABI wants the stack pointer at the entry point.
    let sp = (strings - words as u64 * 8) & !15;
    if top - sp > STACK_PAGES * PAGE_SIZE {
        return Err(NO_ROOM);
    }
    if !vmm::copy_to_user(pml4, strings, args.strings) {
        return Err(NO_ROOM);
    }
    // argc, argv[], NULL, envp[], NULL, AT_NULL.
    let mut at = sp;
    let mut put = |word: u64| {
        let ok = vmm::copy_to_user(pml4, at, &word.to_ne_bytes());
        at += 8;
        ok
    };
    let mut ok = put(args.argc as u64);
    let mut offset = 0;
    for (i, s) in args.strings.split_inclusive(|&b| b == 0).enumerate() {
        if i == args.argc {
            ok &= put(0);
        }
        ok &= put(strings + offset);
        offset += s.len() as u64;
    }
    if args.envc == 0 {
        ok &= put(0); // `argv` ended where the strings did.
    }
    for word in [0, AT_NULL, 0] {
        ok &= put(word);
    }
    if ok {
        Ok(sp)
    } else {
        Err(NO_ROOM)
    }
}
