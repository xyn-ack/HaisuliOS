// SPDX-License-Identifier: MIT

//! Virtual memory manager.
//!
//! Manages the user half of every address space, and all access to user memory.
//!
//! User-space layout (every task):
//!
//! ```text
//! 0x0000_0080_0000_0000  USER_BASE       program image
//! 0x0000_0100_0000_0000  MMAP_BASE       anonymous mmap() region
//! 0x0000_7FFF_FFFF_F000  USER_STACK_TOP  (stack grows down)
//! ```
//!
//! The page tables are the architecture's ([`PageTables`]): this module walks them generically
//! (a root table and the levels below it), and the architecture encodes the entries and puts the
//! kernel's half into every new address space. User space is mapped with 4 KiB pages only.
//!
//! Page tables and user pages are allocator frames, read and written through [`crate::arch::phys`].
//! User memory is only ever reached through the page tables of its address space (never by
//! dereferencing a user pointer), which also checks that it is mapped, user-accessible and, for
//! writes, writable.
//!
//! A driver's address space may also map a device's registers ([`map_device`]). Such pages are
//! not memory: the kernel never reads or writes them for the task (they are not user memory to
//! [`translate`]), a `task_fork` copy does not get them, and they are not freed with the space.

use crate::arch::{phys, PageTables, Paging, Prot};
use crate::pmm::{self, PAGE_SIZE};
use crate::sync::Lock;
use haisuli_abi::mman::{PROT_EXEC, PROT_WRITE};

/// Serialises the changes to user page tables and the walks that copy them.
///
/// The threads of a task share its page tables: two of them mapping memory at once could both
/// find a table missing and both install one, and a `task_fork` could copy tables that another
/// thread changes. Reading them (translating user addresses) needs no lock: entries are only
/// ever added while an address space is in use, each with one atomic write, and an address space
/// is only destroyed once nothing uses it.
static MAP_LOCK: Lock<()> = Lock::new(());

/// The bits of an address below its page: the offset in the page.
const PAGE_OFFSET: u64 = PAGE_SIZE - 1;
/// Entries per page table, at every level.
const TABLE_ENTRIES: usize = 1 << Paging::INDEX_BITS;
/// Bytes per page-table entry.
const ENTRY_SIZE: u64 = size_of::<u64>() as u64;
/// The level of the tables whose entries map pages.
const PAGE_LEVEL: usize = Paging::LEVELS - 1;
const _: () = assert!(
    1 << Paging::PAGE_SHIFT == PAGE_SIZE,
    "the architecture's pages"
);

pub(crate) const USER_BASE: u64 = 0x0000_0080_0000_0000;
pub(crate) const MMAP_BASE: u64 = 0x0000_0100_0000_0000;
pub(crate) const USER_STACK_TOP: u64 = haisuli_abi::USER_STACK_TOP;
pub(crate) const USER_LIMIT: u64 = haisuli_abi::USER_LIMIT;

/// An address space: the physical address of its root page table.
///
/// Only this module makes them, so every one (but [`AddressSpace::NONE`]) is a root that
/// [`new_address_space`] prepared, or the kernel's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct AddressSpace(u64);

impl AddressSpace {
    /// No address space: what an unused task record holds. Never switched to or walked.
    pub(crate) const NONE: AddressSpace = AddressSpace(0);

    /// The address space with no user pages, for a CPU that runs no thread.
    pub(crate) fn kernel() -> AddressSpace {
        AddressSpace(Paging::kernel_root())
    }
}

/// The protection of a user mapping with the `mmap` protection `prot` (readable either way;
/// never executable unless asked).
pub(crate) fn user_prot(prot: u64) -> Prot {
    Prot {
        user: true,
        write: prot & PROT_WRITE != 0,
        exec: prot & PROT_EXEC != 0,
    }
}

fn entry(table: u64, i: usize) -> u64 {
    phys::read_u64(table + ENTRY_SIZE * i as u64)
}

fn set_entry(table: u64, i: usize, v: u64) {
    phys::write_u64(table + ENTRY_SIZE * i as u64, v)
}

/// The lowest address bit that the tables at `level` translate.
const fn shift(level: usize) -> u32 {
    Paging::PAGE_SHIFT + Paging::INDEX_BITS * (PAGE_LEVEL - level) as u32
}

/// The index of `virt` in a table at `level`.
fn idx(virt: u64, level: usize) -> usize {
    ((virt >> shift(level)) as usize) & (TABLE_ENTRIES - 1)
}

/// Makes `space` this CPU's address space (unless it is already).
pub(crate) fn switch(space: AddressSpace) {
    Paging::activate(space.0);
}

/// Create an address space that shares the kernel mapping and has no user pages.
pub(crate) fn new_address_space() -> Option<AddressSpace> {
    let root = pmm::alloc_zeroed()?;
    Paging::init_root(root);
    Some(AddressSpace(root))
}

/// Maps one user page.
///
/// # Panics
///
/// Panics if `frame` is not an allocator frame: user space must never see
/// kernel memory.
fn map_user(space: AddressSpace, virt: u64, frame: u64, prot: Prot) -> Result<(), ()> {
    // User space must never see kernel memory: only allocator frames.
    assert!(
        phys::is_managed(frame),
        "mapping a non-allocator frame {:#x} into user space",
        frame
    );
    set_user_entry(space, virt, Paging::page_entry(frame, prot))
}

/// Installs the page entry `page` for user address `virt` (page aligned), making the tables on
/// the way as needed; fails if out of memory, or if a page is mapped there already.
fn set_user_entry(space: AddressSpace, virt: u64, page: u64) -> Result<(), ()> {
    if !(USER_BASE..USER_LIMIT).contains(&virt) || virt & PAGE_OFFSET != 0 {
        return Err(());
    }
    let mut t = space.0;
    for lvl in 0..PAGE_LEVEL {
        let e = entry(t, idx(virt, lvl));
        t = if !Paging::is_present(e) {
            let f = pmm::alloc_zeroed().ok_or(())?;
            set_entry(t, idx(virt, lvl), Paging::table_entry(f));
            f
        } else {
            Paging::target(e)
        };
    }
    if Paging::is_present(entry(t, idx(virt, PAGE_LEVEL))) {
        return Err(());
    }
    set_entry(t, idx(virt, PAGE_LEVEL), page);
    Ok(())
}

/// Allocates, fills and maps fresh user pages covering `[virt, virt + size)`.
///
/// `data` is placed at `virt` (which need not be page aligned); the rest is
/// zero. Fails if out of memory or if a page is already mapped; pages mapped
/// before the failure stay mapped (they are freed with the address space).
pub(crate) fn map_init(
    space: AddressSpace,
    virt: u64,
    size: u64,
    data: &[u8],
    prot: Prot,
) -> Result<(), ()> {
    let _serialised = MAP_LOCK.lock();
    let end = virt.checked_add(size).ok_or(())?;
    if (data.len() as u64) > size {
        return Err(());
    }
    let data_end = virt + data.len() as u64;
    let mut page = virt & !PAGE_OFFSET;
    while page < end {
        let f = pmm::alloc_zeroed().ok_or(())?;
        // Part of `data` that falls into this page.
        let lo = page.max(virt);
        let hi = (page + PAGE_SIZE).min(data_end);
        if lo < hi {
            phys::write(
                f + (lo - page),
                &data[(lo - virt) as usize..(hi - virt) as usize],
            );
        }
        if map_user(space, page, f, prot).is_err() {
            pmm::free_frame(f);
            return Err(());
        }
        page += PAGE_SIZE;
    }
    Ok(())
}

/// Maps the device registers at physical `regs` (page aligned), `pages` pages of them, at
/// `virt` in `space`, for the task's own use.
///
/// Only for a device that the machine describes (see [`crate::device`]): its registers are not
/// memory, so they are neither copied by [`clone_user_space`] nor freed by
/// [`destroy_user_space`]. Fails if out of memory for page tables or if a page is already
/// mapped; pages mapped before the failure stay mapped.
pub(crate) fn map_device(space: AddressSpace, virt: u64, regs: u64, pages: u64) -> Result<(), ()> {
    let _serialised = MAP_LOCK.lock();
    let prot = Prot {
        user: true,
        write: true,
        exec: false,
    };
    for i in 0..pages {
        let entry = Paging::device_entry(regs + i * PAGE_SIZE, prot);
        set_user_entry(space, virt + i * PAGE_SIZE, entry)?;
    }
    Ok(())
}

/// Walks the page tables for a user address and returns its physical address and protection.
///
/// Device registers ([`map_device`]) are not user memory: they are not found.
pub(crate) fn translate(space: AddressSpace, virt: u64) -> Option<(u64, Prot)> {
    if !(USER_BASE..USER_LIMIT).contains(&virt) {
        return None; // The kernel's half is not walked here.
    }
    let mut t = space.0;
    for lvl in 0..PAGE_LEVEL {
        let e = entry(t, idx(virt, lvl));
        if !Paging::is_present(e) {
            return None;
        }
        t = Paging::target(e);
    }
    let e = entry(t, idx(virt, PAGE_LEVEL));
    if !Paging::is_present(e) || Paging::is_device(e) {
        return None;
    }
    Some((
        Paging::target(e) | (virt & PAGE_OFFSET),
        Paging::page_prot(e),
    ))
}

/// Returns whether `[ptr, ptr + len)` is entirely mapped user memory (writable if `write`).
pub(crate) fn user_range_ok(space: AddressSpace, ptr: u64, len: u64, write: bool) -> bool {
    if len == 0 {
        return true;
    }
    let end = match ptr.checked_add(len) {
        Some(e) => e,
        None => return false,
    };
    if ptr < USER_BASE || end > USER_LIMIT {
        return false;
    }
    let mut a = ptr & !PAGE_OFFSET;
    while a < end {
        match translate(space, a) {
            Some((_, prot)) if prot.user && (!write || prot.write) => {}
            _ => return false,
        }
        a += PAGE_SIZE;
    }
    true
}

/// Physical address of user address `va`, which `user_range_ok` accepted.
fn user_phys(space: AddressSpace, va: u64) -> u64 {
    translate(space, va)
        .expect("validated user page vanished")
        .0
}

/// Bytes from `va` to the end of its page.
fn page_rest(va: u64) -> u64 {
    PAGE_SIZE - (va & (PAGE_SIZE - 1))
}

/// Copy `src` to user memory at `dst` in address space `space`.
pub(crate) fn copy_to_user(space: AddressSpace, dst: u64, src: &[u8]) -> bool {
    if !user_range_ok(space, dst, src.len() as u64, true) {
        return false;
    }
    let mut done = 0;
    while done < src.len() {
        let va = dst + done as u64;
        let n = (page_rest(va) as usize).min(src.len() - done);
        phys::write(user_phys(space, va), &src[done..done + n]);
        done += n;
    }
    true
}

/// Fill `dst` from user memory at `src` in address space `space`.
pub(crate) fn copy_from_user(space: AddressSpace, src: u64, dst: &mut [u8]) -> bool {
    if !user_range_ok(space, src, dst.len() as u64, false) {
        return false;
    }
    let mut done = 0;
    while done < dst.len() {
        let va = src + done as u64;
        let n = (page_rest(va) as usize).min(dst.len() - done);
        phys::read(user_phys(space, va), &mut dst[done..done + n]);
        done += n;
    }
    true
}

/// Copy `len` bytes of user memory from one address space to another.
pub(crate) fn copy_user_to_user(
    dst_space: AddressSpace,
    dst: u64,
    src_space: AddressSpace,
    src: u64,
    len: u64,
) -> bool {
    if !user_range_ok(src_space, src, len, false) || !user_range_ok(dst_space, dst, len, true) {
        return false;
    }
    let mut done = 0;
    while done < len {
        let (s, d) = (src + done, dst + done);
        let n = page_rest(s).min(page_rest(d)).min(len - done);
        phys::copy(user_phys(dst_space, d), user_phys(src_space, s), n as usize);
        done += n;
    }
    true
}

/// What [`walk`] visits.
enum Visit {
    /// A mapped user page: its virtual address and page entry.
    Page(u64, u64),
    /// A page table below the root, once everything below it has been visited.
    Table(u64),
}

/// Visits the user part of `space`, depth first: every mapped page, and every table below the
/// root after what it points to (so that the tables can be freed as they are visited).
fn walk(space: AddressSpace, mut f: impl FnMut(Visit)) {
    for i in Paging::USER_ROOT_ENTRIES {
        let e = entry(space.0, i);
        if Paging::is_present(e) {
            walk_table(Paging::target(e), 1, (i as u64) << shift(0), &mut f);
        }
    }
}

/// [`walk`] below the root: the table at `table`, at `level`, which maps the addresses from
/// `base` on.
fn walk_table(table: u64, level: usize, base: u64, f: &mut impl FnMut(Visit)) {
    for i in 0..TABLE_ENTRIES {
        let e = entry(table, i);
        if !Paging::is_present(e) {
            continue;
        }
        let virt = base | (i as u64) << shift(level);
        if level == PAGE_LEVEL {
            f(Visit::Page(virt, e));
        } else {
            walk_table(Paging::target(e), level + 1, virt, f);
        }
    }
    f(Visit::Table(table));
}

/// Deep-copy all user pages (used by `task_fork`; no copy-on-write yet).
pub(crate) fn clone_user_space(src: AddressSpace) -> Option<AddressSpace> {
    let dst = new_address_space()?;
    let serialised = MAP_LOCK.lock();
    let mut ok = true;
    walk(src, |visit| {
        let Visit::Page(virt, e) = visit else {
            return;
        };
        // Device registers stay the driver's own: the copy (never privileged) gets none.
        if !ok || Paging::is_device(e) {
            return;
        }
        let Some(f) = pmm::alloc_frame() else {
            ok = false;
            return;
        };
        phys::copy(f, Paging::target(e), PAGE_SIZE as usize);
        // The same permissions; the CPU's record of use starts afresh for the copy.
        if map_user(dst, virt, f, Paging::page_prot(e)).is_err() {
            pmm::free_frame(f);
            ok = false;
        }
    });
    drop(serialised);
    if !ok {
        destroy_user_space(dst);
        return None;
    }
    Some(dst)
}

/// Frees every user page and page table, then the root itself.
///
/// Never call this on an address space that a CPU uses.
pub(crate) fn destroy_user_space(space: AddressSpace) {
    walk(space, |visit| match visit {
        // Device registers are not the allocator's.
        Visit::Page(_, e) if Paging::is_device(e) => {}
        Visit::Page(_, e) => pmm::free_frame(Paging::target(e)),
        Visit::Table(table) => pmm::free_frame(table),
    });
    pmm::free_frame(space.0);
}
