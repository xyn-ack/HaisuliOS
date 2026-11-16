// SPDX-License-Identifier: GPL-2.0

//! The device tree.
//!
//! Reads the flattened device tree (DTB) that the boot loader passes: its structure block is a
//! sequence of tokens (a node begins, a property, a node ends), whose names are in the strings
//! block. [`Fdt::nodes`] walks it and hands out every node with the properties that the platform
//! reads, and the `#address-cells` / `#size-cells` of its parent, which say how to read its
//! `reg`.
//!
//! Everything is checked against the blob's bounds: a malformed tree ends the walk early, it
//! does not make the kernel read outside the blob.

/// The DTB header's magic number (big-endian, as every number in the blob).
const MAGIC: u32 = 0xD00D_FEED;

/// Structure block tokens.
const BEGIN_NODE: u32 = 1;
const END_NODE: u32 = 2;
const PROP: u32 = 3;
const NOP: u32 = 4;
const END: u32 = 9;

/// The deepest nesting the walk follows (QEMU's `virt` needs 5).
const MAX_DEPTH: usize = 8;

/// A device tree blob.
#[derive(Clone, Copy)]
pub(crate) struct Fdt {
    blob: &'static [u8],
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn be64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// The NUL-terminated string at `at`.
fn cstr(b: &[u8], at: usize) -> Option<&[u8]> {
    let rest = b.get(at..)?;
    Some(&rest[..rest.iter().position(|&c| c == 0)?])
}

/// A number of `cells` 32-bit cells (1 or 2) at `at` in `b`.
pub(crate) fn cells(b: &[u8], at: usize, cells: u32) -> Option<u64> {
    match cells {
        1 => be32(b, at).map(u64::from),
        2 => be64(b, at),
        _ => None,
    }
}

/// The total size a DTB header at the start of `header` (at least 8 bytes) gives, if it is one.
pub(crate) fn total_size(header: &[u8]) -> Option<usize> {
    (be32(header, 0)? == MAGIC).then_some(be32(header, 4)? as usize)
}

/// A node, with the properties the platform reads (empty if it has none of them).
#[derive(Clone, Copy, Default)]
pub(crate) struct Node {
    /// Its depth: 0 for the root.
    pub(crate) depth: usize,
    /// Its name (`memory@40000000`), and its parent's.
    pub(crate) name: &'static [u8],
    pub(crate) parent: &'static [u8],
    /// How its parent's children write addresses and sizes in `reg` (in cells).
    pub(crate) address_cells: u32,
    pub(crate) size_cells: u32,
    /// Its own `#address-cells` and `#size-cells`, for its children.
    own_address_cells: u32,
    own_size_cells: u32,
    pub(crate) compatible: &'static [u8],
    pub(crate) device_type: &'static [u8],
    pub(crate) reg: &'static [u8],
    pub(crate) interrupts: &'static [u8],
    pub(crate) method: &'static [u8],
    pub(crate) initrd_start: &'static [u8],
    pub(crate) initrd_end: &'static [u8],
}

impl Node {
    /// Whether its `compatible` list has `name`.
    pub(crate) fn is_compatible(&self, name: &[u8]) -> bool {
        self.compatible.split(|&c| c == 0).any(|c| c == name)
    }

    /// The `n`-th (address, size) pair of its `reg`.
    pub(crate) fn reg(&self, n: usize) -> Option<(u64, u64)> {
        let (a, s) = (self.address_cells, self.size_cells);
        let at = n * 4 * (a + s) as usize;
        Some((
            cells(self.reg, at, a)?,
            cells(self.reg, at + 4 * a as usize, s)?,
        ))
    }
}

impl Fdt {
    /// The device tree in `blob`, if it is one.
    pub(crate) fn new(blob: &'static [u8]) -> Option<Fdt> {
        (total_size(blob)? <= blob.len()).then_some(Fdt { blob })
    }

    /// The reserved memory that the tree lists (`/memreserve/`): (address, size) pairs.
    pub(crate) fn reserved(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        let start = be32(self.blob, 16).unwrap_or(0) as usize;
        (0..)
            .map(move |i| {
                let at = start + 16 * i;
                Some((be64(self.blob, at)?, be64(self.blob, at + 8)?))
            })
            .take_while(|entry| matches!(entry, Some((_, size)) if *size != 0))
            .flatten()
    }

    /// Calls `visit` with every node, once its properties have been read (children first).
    pub(crate) fn nodes(&self, mut visit: impl FnMut(&Node)) {
        let b = self.blob;
        let (Some(structs), Some(strings)) = (be32(b, 8), be32(b, 12)) else {
            return;
        };
        let strings = strings as usize;
        let mut at = structs as usize;
        let mut stack = [Node::default(); MAX_DEPTH];
        let mut depth = 0; // Nodes open.
        while let Some(token) = be32(b, at) {
            at += 4;
            match token {
                BEGIN_NODE => {
                    let Some(name) = cstr(b, at) else { return };
                    at = (at + name.len() + 1).next_multiple_of(4);
                    if depth == MAX_DEPTH {
                        return;
                    }
                    let parent = depth.checked_sub(1).map(|p| stack[p]);
                    stack[depth] = Node {
                        depth,
                        name,
                        parent: parent.map_or(&[][..], |p| p.name),
                        // The defaults, unless the parent says otherwise.
                        address_cells: parent.map_or(2, |p| p.own_address_cells),
                        size_cells: parent.map_or(1, |p| p.own_size_cells),
                        own_address_cells: 2,
                        own_size_cells: 1,
                        ..Node::default()
                    };
                    depth += 1;
                }
                END_NODE => {
                    let Some(top) = depth.checked_sub(1) else {
                        return;
                    };
                    visit(&stack[top]);
                    depth = top;
                }
                PROP => {
                    let (Some(len), Some(name_at)) = (be32(b, at), be32(b, at + 4)) else {
                        return;
                    };
                    let (len, value_at) = (len as usize, at + 8);
                    let (Some(value), Some(name)) = (
                        b.get(value_at..value_at + len),
                        cstr(b, strings + name_at as usize),
                    ) else {
                        return;
                    };
                    at = (value_at + len).next_multiple_of(4);
                    let Some(top) = depth.checked_sub(1) else {
                        return;
                    };
                    let node = &mut stack[top];
                    match name {
                        b"#address-cells" => {
                            node.own_address_cells = cells(value, 0, 1).unwrap_or(2) as u32;
                        }
                        b"#size-cells" => {
                            node.own_size_cells = cells(value, 0, 1).unwrap_or(1) as u32;
                        }
                        b"compatible" => node.compatible = value,
                        b"device_type" => node.device_type = value,
                        b"reg" => node.reg = value,
                        b"interrupts" => node.interrupts = value,
                        b"method" => node.method = value,
                        b"linux,initrd-start" => node.initrd_start = value,
                        b"linux,initrd-end" => node.initrd_end = value,
                        _ => {}
                    }
                }
                NOP => {}
                END => return,
                _ => return,
            }
        }
    }
}
