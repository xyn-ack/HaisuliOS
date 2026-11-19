// SPDX-License-Identifier: MIT

//! RAM file system.
//!
//! The files of the system, and the open file descriptions that file descriptors refer to.
//!
//! At boot the boot ramdisk (a ustar archive) is unpacked into a small RAM file system, which is
//! then writable, like Linux's initramfs: a table of nodes, files and directories named by their
//! full normalised paths, whose contents live in page frames from the frame allocator. A path is
//! also a directory when some node lies below it, as in a tar archive, so directories need no node
//! of their own until they are opened. A file can be created in any directory; there is no way to
//! remove one yet.
//!
//! An *open file description* (POSIX) is what `open` creates: a node, an offset and an access
//! mode. Tasks refer to descriptions by index from their file descriptors (see
//! [`crate::task`]), and `dup`, `dup2` and `task_fork` make several descriptors share one. That is
//! what makes `2>&1` work: both descriptors write at the same offset. Description [`CONSOLE`] is
//! the terminal; reads and writes on it go to the console server instead.
//!
//! Paths are normalised before use: components are separated by any number of `/`, `.` is
//! dropped and `..` goes up one level. There is no current directory: a path starts at the root,
//! unless it is relative to an open directory (`openat`).

use crate::arch::phys;
use crate::sync::Lock;
use crate::vmm::AddressSpace;
use crate::{pmm, vmm};
use haisuli_abi::errno::*;
use haisuli_abi::fcntl::*;

/// Most nodes (files and directories, including the root) the file system holds.
const MAX_NODES: usize = 64;
/// Most pages of one file.
const FILE_PAGES: usize = 32;
const PAGE_SIZE: u64 = 4096;
/// Most bytes in one file.
const MAX_FILE_SIZE: u64 = FILE_PAGES as u64 * PAGE_SIZE;
/// Most bytes in one file, as a buffer size (see [`read_file`]).
pub(crate) const MAX_FILE: usize = MAX_FILE_SIZE as usize;
/// Most open file descriptions in the system, including [`CONSOLE`].
const MAX_OPEN: usize = 64;
/// Bytes copied between user memory and a file at a time.
const CHUNK: usize = 256;

/// The open file description of the terminal: every task starts with its
/// standard input, output and error on it.
pub(crate) const CONSOLE: usize = 0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Free,
    File,
    Dir,
}

/// A normalised path: components joined by single `/`, without a leading or
/// trailing one. The root directory is the empty path.
#[derive(Clone, Copy)]
struct Path {
    bytes: [u8; PATH_MAX],
    len: usize,
}

impl Path {
    const ROOT: Path = Path {
        bytes: [0; PATH_MAX],
        len: 0,
    };

    /// Normalises `raw` (see the module documentation).
    fn parse(raw: &[u8]) -> Result<Path, i64> {
        Path::parse_from(Path::ROOT, raw)
    }

    /// Normalises `raw`, starting at directory `base` unless it is absolute.
    fn parse_from(base: Path, raw: &[u8]) -> Result<Path, i64> {
        let mut path = if raw.starts_with(b"/") {
            Path::ROOT
        } else {
            base
        };
        for component in raw.split(|&b| b == b'/') {
            match component {
                b"" | b"." => {}
                b".." => path.pop(),
                name => path.push(name)?,
            }
        }
        Ok(path)
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    fn is_root(&self) -> bool {
        self.len == 0
    }

    fn push(&mut self, name: &[u8]) -> Result<(), i64> {
        let sep = usize::from(!self.is_root());
        let end = self.len + sep + name.len();
        if end > PATH_MAX {
            return Err(-ENAMETOOLONG);
        }
        if sep == 1 {
            self.bytes[self.len] = b'/';
        }
        self.bytes[self.len + sep..end].copy_from_slice(name);
        self.len = end;
        Ok(())
    }

    fn pop(&mut self) {
        self.len = self
            .as_bytes()
            .iter()
            .rposition(|&b| b == b'/')
            .unwrap_or(0);
    }

    /// The directory containing this path (the root for the root).
    fn parent(&self) -> Path {
        let mut parent = *self;
        parent.pop();
        parent
    }
}

/// If `path` lies below directory `dir`, the name of the entry of `dir` it is
/// in, and whether `path` is deeper down than that entry.
fn child_of<'p>(dir: &[u8], path: &'p [u8]) -> Option<(&'p [u8], bool)> {
    let rest = if dir.is_empty() {
        path
    } else {
        path.strip_prefix(dir)?.strip_prefix(b"/")?
    };
    if rest.is_empty() {
        return None;
    }
    Some(match rest.iter().position(|&b| b == b'/') {
        Some(slash) => (&rest[..slash], true),
        None => (rest, false),
    })
}

/// A file or directory.
#[derive(Clone, Copy)]
struct Node {
    kind: Kind,
    path: Path,
    /// Bytes in the file.
    size: u64,
    /// The page frames holding the file, 0 for none yet (reads as zeros).
    pages: [u64; FILE_PAGES],
}

impl Node {
    const FREE: Node = Node {
        kind: Kind::Free,
        path: Path::ROOT,
        size: 0,
        pages: [0; FILE_PAGES],
    };

    /// Copies the file's bytes at `off` into `dst` (which must not reach past the end).
    fn read_at(&self, off: u64, dst: &mut [u8]) {
        let mut done = 0;
        while done < dst.len() {
            let at = off + done as u64;
            let (page, in_page) = ((at / PAGE_SIZE) as usize, at % PAGE_SIZE);
            let n = ((PAGE_SIZE - in_page) as usize).min(dst.len() - done);
            match self.pages[page] {
                0 => dst[done..done + n].fill(0),
                frame => phys::read(frame + in_page, &mut dst[done..done + n]),
            }
            done += n;
        }
    }

    /// Writes `src` into the file at `off`, growing it as needed.
    fn write_at(&mut self, off: u64, src: &[u8]) -> Result<(), i64> {
        let end = off + src.len() as u64;
        if end > MAX_FILE_SIZE {
            return Err(-EFBIG);
        }
        let mut done = 0;
        while done < src.len() {
            let at = off + done as u64;
            let (page, in_page) = ((at / PAGE_SIZE) as usize, at % PAGE_SIZE);
            let n = ((PAGE_SIZE - in_page) as usize).min(src.len() - done);
            if self.pages[page] == 0 {
                self.pages[page] = pmm::alloc_zeroed().ok_or(-ENOSPC)?;
            }
            phys::write(self.pages[page] + in_page, &src[done..done + n]);
            done += n;
        }
        self.size = self.size.max(end);
        Ok(())
    }

    /// Empties the file and gives its pages back.
    fn truncate(&mut self) {
        for frame in self.pages.iter_mut().filter(|f| **f != 0) {
            pmm::free_frame(*frame);
            *frame = 0;
        }
        self.size = 0;
    }
}

/// An open file description.
#[derive(Clone, Copy)]
struct Open {
    /// File descriptors referring to it; 0 means the slot is free.
    refs: u32,
    node: usize,
    offset: u64,
    read: bool,
    write: bool,
    append: bool,
}

impl Open {
    const FREE: Open = Open {
        refs: 0,
        node: 0,
        offset: 0,
        read: false,
        write: false,
        append: false,
    };
}

struct Fs {
    nodes: [Node; MAX_NODES],
    open: [Open; MAX_OPEN],
}

static FS: Lock<Fs> = Lock::new(Fs {
    nodes: [Node::FREE; MAX_NODES],
    open: {
        let mut open = [Open::FREE; MAX_OPEN];
        // The terminal: never freed.
        open[CONSOLE] = Open {
            refs: 1,
            read: true,
            write: true,
            ..Open::FREE
        };
        open
    },
});

impl Fs {
    fn find(&self, path: &Path) -> Option<usize> {
        self.nodes
            .iter()
            .position(|n| n.kind != Kind::Free && n.path.as_bytes() == path.as_bytes())
    }

    /// What `path` is: a node's kind, or a directory if nodes lie below it.
    fn kind_of(&self, path: &Path) -> Option<Kind> {
        if path.is_root() {
            return Some(Kind::Dir);
        }
        if let Some(i) = self.find(path) {
            return Some(self.nodes[i].kind);
        }
        self.nodes
            .iter()
            .any(|n| n.kind != Kind::Free && child_of(path.as_bytes(), n.path.as_bytes()).is_some())
            .then_some(Kind::Dir)
    }

    fn create(&mut self, path: &Path, kind: Kind) -> Result<usize, i64> {
        let i = self
            .nodes
            .iter()
            .position(|n| n.kind == Kind::Free)
            .ok_or(-ENOSPC)?;
        self.nodes[i] = Node {
            kind,
            path: *path,
            ..Node::FREE
        };
        Ok(i)
    }

    /// The node for `path`, which must exist; an implicit directory gets one.
    fn node_of(&mut self, path: &Path, kind: Kind) -> Result<usize, i64> {
        match self.find(path) {
            Some(i) => Ok(i),
            None => self.create(path, kind),
        }
    }

    /// Whether the name `name` of directory `dir` comes from a node before `pos`
    /// (so that a directory lists each entry once).
    fn listed_before(&self, dir: &[u8], name: &[u8], pos: usize) -> bool {
        self.nodes[..pos].iter().any(|n| {
            n.kind != Kind::Free && child_of(dir, n.path.as_bytes()).is_some_and(|(c, _)| c == name)
        })
    }
}

/// Unpacks the boot ramdisk (a ustar archive) into the file system.
///
/// Returns the number of files. Entries that are neither files nor
/// directories, or that do not fit, are skipped.
pub(crate) fn init(archive: &[u8]) -> usize {
    let mut fs = FS.lock();
    // The root directory.
    let _ = fs.create(&Path::ROOT, Kind::Dir);
    let mut files = 0;
    for entry in ustar::entries(archive) {
        let Ok(path) = Path::parse(entry.path()) else {
            continue;
        };
        if path.is_root() || fs.find(&path).is_some() {
            continue;
        }
        let Ok(i) = fs.create(&path, entry.kind) else {
            break;
        };
        if entry.kind == Kind::File && fs.nodes[i].write_at(0, entry.data).is_ok() {
            files += 1;
        }
    }
    files
}

/// Opens `path` with `O_*` `flags` and returns a new open file description.
///
/// A relative `path` starts at the directory that open file description `dir` refers to
/// (`-ENOTDIR` if it is not a directory), or at the root without one.
///
/// As in POSIX: `O_CREAT` creates a missing file in an existing directory,
/// `O_EXCL` fails if it exists, `O_TRUNC` empties it, `O_APPEND` makes every
/// write go to its end and `O_DIRECTORY` insists on a directory. A directory
/// can only be opened for reading (with `getdents64`).
pub(crate) fn open(dir: Option<usize>, raw_path: &[u8], flags: u64) -> Result<usize, i64> {
    let (read, write) = match flags & O_ACCMODE {
        O_RDONLY => (true, false),
        O_WRONLY => (false, true),
        O_RDWR => (true, true),
        _ => return Err(-EINVAL),
    };
    let mut fs = FS.lock();
    let base = match dir {
        None => Path::ROOT,
        // The terminal's description has no node of its own.
        Some(ofd) if ofd == CONSOLE => return Err(-ENOTDIR),
        Some(ofd) => {
            let node = &fs.nodes[fs.open[ofd].node];
            if node.kind != Kind::Dir {
                return Err(-ENOTDIR);
            }
            node.path
        }
    };
    let path = Path::parse_from(base, raw_path)?;
    let slot = (0..MAX_OPEN)
        .find(|&i| i != CONSOLE && fs.open[i].refs == 0)
        .ok_or(-ENFILE)?;
    let node = match fs.kind_of(&path) {
        Some(Kind::Dir) => {
            if write || flags & O_CREAT != 0 {
                return Err(-EISDIR);
            }
            fs.node_of(&path, Kind::Dir)?
        }
        Some(_) => {
            if flags & O_DIRECTORY != 0 {
                return Err(-ENOTDIR);
            }
            if flags & (O_CREAT | O_EXCL) == O_CREAT | O_EXCL {
                return Err(-EEXIST);
            }
            let i = fs.node_of(&path, Kind::File)?;
            if write && flags & O_TRUNC != 0 {
                fs.nodes[i].truncate();
            }
            i
        }
        None => {
            if flags & O_CREAT == 0 {
                return Err(-ENOENT);
            }
            match fs.kind_of(&path.parent()) {
                Some(Kind::Dir) => fs.create(&path, Kind::File)?,
                Some(_) => return Err(-ENOTDIR),
                None => return Err(-ENOENT),
            }
        }
    };
    fs.open[slot] = Open {
        refs: 1,
        node,
        offset: 0,
        read,
        write,
        append: flags & O_APPEND != 0,
    };
    Ok(slot)
}

/// Copies the whole file at `raw_path` into `buf` and returns its size: how
/// the kernel reads a program to run.
///
/// A directory is `-EACCES`, as for `execve`.
pub(crate) fn read_file(raw_path: &[u8], buf: &mut [u8; MAX_FILE]) -> Result<usize, i64> {
    let path = Path::parse(raw_path)?;
    let fs = FS.lock();
    match fs.kind_of(&path) {
        None => Err(-ENOENT),
        Some(Kind::Dir) => Err(-EACCES),
        Some(_) => {
            let node = &fs.nodes[fs.find(&path).ok_or(-ENOENT)?];
            let size = node.size as usize;
            node.read_at(0, &mut buf[..size]);
            Ok(size)
        }
    }
}

/// Records another file descriptor referring to description `ofd`.
pub(crate) fn retain(ofd: usize) {
    if ofd != CONSOLE {
        FS.lock().open[ofd].refs += 1;
    }
}

/// Drops a file descriptor's reference to `ofd`; the last one frees it.
pub(crate) fn release(ofd: usize) {
    if ofd != CONSOLE {
        let mut fs = FS.lock();
        fs.open[ofd].refs = fs.open[ofd].refs.saturating_sub(1);
    }
}

/// Where a `read` or `write` on a description goes.
pub(crate) enum Transfer {
    /// To the terminal: the caller forwards it to the console server.
    Console,
    /// Done here: the number of bytes, or a negative errno.
    Done(i64),
}

/// Reads up to `len` bytes of `ofd` at its offset into user memory at `buf`.
///
/// `buf` must be writable user memory in `space` (the caller checks it).
pub(crate) fn read(ofd: usize, space: AddressSpace, buf: u64, len: u64) -> Transfer {
    if ofd == CONSOLE {
        return Transfer::Console;
    }
    let mut fs = FS.lock();
    let open = fs.open[ofd];
    if !open.read {
        return Transfer::Done(-EBADF);
    }
    let node = &fs.nodes[open.node];
    if node.kind == Kind::Dir {
        return Transfer::Done(-EISDIR);
    }
    let total = len.min(node.size.saturating_sub(open.offset)) as usize;
    let mut chunk = [0u8; CHUNK];
    let mut done = 0;
    while done < total {
        let n = CHUNK.min(total - done);
        node.read_at(open.offset + done as u64, &mut chunk[..n]);
        if !vmm::copy_to_user(space, buf + done as u64, &chunk[..n]) {
            break;
        }
        done += n;
    }
    fs.open[ofd].offset += done as u64;
    Transfer::Done(if done == 0 && total > 0 {
        -EFAULT
    } else {
        done as i64
    })
}

/// Writes `len` bytes of user memory at `buf` to `ofd` at its offset (at the
/// end of the file for `O_APPEND`).
///
/// Returns how much was written; a full file system or file ends the write
/// early, and fails it if nothing could be written.
pub(crate) fn write(ofd: usize, space: AddressSpace, buf: u64, len: u64) -> Transfer {
    if ofd == CONSOLE {
        return Transfer::Console;
    }
    let mut fs = FS.lock();
    let open = fs.open[ofd];
    if !open.write {
        return Transfer::Done(-EBADF);
    }
    let node = &mut fs.nodes[open.node];
    let start = if open.append { node.size } else { open.offset };
    let mut chunk = [0u8; CHUNK];
    let mut done = 0;
    let mut error = 0;
    while (done as u64) < len {
        // Up to the size limit: a write that reaches it is a short write.
        let room = MAX_FILE_SIZE.saturating_sub(start + done as u64);
        let n = CHUNK.min((len - done as u64).min(room) as usize);
        if n == 0 {
            error = -EFBIG;
            break;
        }
        if !vmm::copy_from_user(space, buf + done as u64, &mut chunk[..n]) {
            error = -EFAULT;
            break;
        }
        if let Err(e) = node.write_at(start + done as u64, &chunk[..n]) {
            error = e;
            break;
        }
        done += n;
    }
    fs.open[ofd].offset = start + done as u64;
    Transfer::Done(if done == 0 && error != 0 {
        error
    } else {
        done as i64
    })
}

/// Fills user memory at `buf[..len]` with directory entries of `ofd` (Linux
/// `struct linux_dirent64` records) and returns the bytes used (0 at the end).
///
/// The description's offset is the index of the next node to look at.
pub(crate) fn getdents(ofd: usize, space: AddressSpace, buf: u64, len: u64) -> i64 {
    if ofd == CONSOLE {
        return -ENOTDIR;
    }
    let mut fs = FS.lock();
    let open = fs.open[ofd];
    if fs.nodes[open.node].kind != Kind::Dir {
        return -ENOTDIR;
    }
    let dir = fs.nodes[open.node].path;
    let dir = dir.as_bytes();
    let mut pos = open.offset as usize;
    let mut written = 0;
    while pos < MAX_NODES {
        let node = &fs.nodes[pos];
        let entry = (node.kind != Kind::Free)
            .then(|| child_of(dir, node.path.as_bytes()))
            .flatten()
            .filter(|&(name, _)| !fs.listed_before(dir, name, pos));
        if let Some((name, deeper)) = entry {
            let is_dir = deeper || node.kind == Kind::Dir;
            let reclen = (DIRENT_HEADER + name.len() + 1).next_multiple_of(8);
            if written + reclen as u64 > len {
                if written == 0 {
                    return -EINVAL; // Not even one entry fits.
                }
                break;
            }
            let mut record = [0u8; (DIRENT_HEADER + PATH_MAX + 1).next_multiple_of(8)];
            record[0..8].copy_from_slice(&(pos as u64 + 1).to_ne_bytes()); // d_ino
            record[8..16].copy_from_slice(&(pos as u64 + 1).to_ne_bytes()); // d_off
            record[16..18].copy_from_slice(&(reclen as u16).to_ne_bytes());
            record[18] = if is_dir { DT_DIR } else { DT_REG };
            record[DIRENT_HEADER..DIRENT_HEADER + name.len()].copy_from_slice(name);
            if !vmm::copy_to_user(space, buf + written, &record[..reclen]) {
                return -EFAULT;
            }
            written += reclen as u64;
        }
        pos += 1;
    }
    fs.open[ofd].offset = pos as u64;
    written as i64
}

mod ustar {
    //! Reading the boot ramdisk, a ustar (POSIX tar) archive.

    use super::Kind;

    const BLOCK: usize = 512;

    pub(super) struct Entry<'a> {
        path: [u8; 256],
        path_len: usize,
        pub(super) kind: Kind,
        pub(super) data: &'a [u8],
    }

    impl Entry<'_> {
        pub(super) fn path(&self) -> &[u8] {
            &self.path[..self.path_len]
        }
    }

    /// The files and directories in `archive`, in archive order.
    pub(super) fn entries(archive: &[u8]) -> impl Iterator<Item = Entry<'_>> {
        let mut off = 0;
        core::iter::from_fn(move || loop {
            let header = archive.get(off..off + BLOCK)?;
            if header.iter().all(|&b| b == 0) {
                return None; // The end-of-archive marker.
            }
            let size = octal(field(header, 124, 12))?;
            let start = off + BLOCK;
            let data = archive.get(start..start.checked_add(size)?)?;
            off = start + size.div_ceil(BLOCK) * BLOCK;
            let kind = match header[156] {
                b'0' | 0 => Kind::File,
                b'5' => Kind::Dir,
                _ => continue,
            };
            // Ustar splits long paths into prefix + "/" + name.
            let (name, prefix) = (field(header, 0, 100), field(header, 345, 155));
            let mut path = [0u8; 256];
            let mut len = 0;
            if &header[257..262] == b"ustar" && !prefix.is_empty() {
                path[..prefix.len()].copy_from_slice(prefix);
                path[prefix.len()] = b'/';
                len = prefix.len() + 1;
            }
            path[len..len + name.len()].copy_from_slice(name);
            return Some(Entry {
                path,
                path_len: len + name.len(),
                kind,
                data,
            });
        })
    }

    /// A NUL-terminated string field of a header.
    fn field(header: &[u8], off: usize, len: usize) -> &[u8] {
        let f = &header[off..off + len];
        &f[..f.iter().position(|&b| b == 0).unwrap_or(len)]
    }

    fn octal(field: &[u8]) -> Option<usize> {
        let mut v: usize = 0;
        for &b in field.iter().skip_while(|&&b| b == b' ') {
            match b {
                b'0'..=b'7' => v = v.checked_mul(8)?.checked_add((b - b'0') as usize)?,
                b' ' | 0 => break,
                _ => return None,
            }
        }
        Some(v)
    }
}
