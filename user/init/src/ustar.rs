// SPDX-License-Identifier: GPL-2.0

//! Tar archive reader.
//!
//! Read-only access to a ustar (POSIX tar) archive held in memory.
//!
//! Paths are compared after normalisation: no leading `/` or `./`, no trailing `/` (the root
//! directory is the empty path).

const BLOCK: usize = 512;

pub(crate) struct Archive<'a> {
    data: &'a [u8],
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    File,
    Dir,
    Other,
}

pub(crate) struct Entry<'a> {
    path: [u8; 256],
    path_len: usize,
    kind: Kind,
    pub data: &'a [u8],
}

impl<'a> Entry<'a> {
    pub(crate) fn path(&self) -> &str {
        // Only entries with UTF-8 paths are produced (see `Iter::next`).
        normalize(core::str::from_utf8(&self.path[..self.path_len]).unwrap_or(""))
    }
    pub(crate) fn is_file(&self) -> bool {
        self.kind == Kind::File
    }
}

pub(crate) fn normalize(mut p: &str) -> &str {
    loop {
        if let Some(rest) = p.strip_prefix('/').or_else(|| p.strip_prefix("./")) {
            p = rest;
        } else if p == "." {
            return "";
        } else {
            return p.trim_end_matches('/');
        }
    }
}

/// If `path` is directly inside `dir` (both normalised), its name.
pub(crate) fn child_of<'p>(dir: &str, path: &'p str) -> Option<&'p str> {
    let rest = if dir.is_empty() {
        path
    } else {
        path.strip_prefix(dir)?.strip_prefix('/')?
    };
    (!rest.is_empty() && !rest.contains('/')).then_some(rest)
}

/// NUL-terminated string field of a header.
fn field(h: &[u8], off: usize, len: usize) -> &[u8] {
    let f = &h[off..off + len];
    &f[..f.iter().position(|&b| b == 0).unwrap_or(len)]
}

fn octal(f: &[u8]) -> Option<usize> {
    let mut v: usize = 0;
    for &b in f.iter().skip_while(|&&b| b == b' ') {
        match b {
            b'0'..=b'7' => v = v.checked_mul(8)?.checked_add((b - b'0') as usize)?,
            b' ' | 0 => break,
            _ => return None,
        }
    }
    Some(v)
}

impl<'a> Archive<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Archive { data }
    }

    pub(crate) fn entries(&self) -> Iter<'a> {
        Iter {
            data: self.data,
            off: 0,
        }
    }

    pub(crate) fn find(&self, path: &str) -> Option<Entry<'a>> {
        let path = normalize(path);
        if path.is_empty() {
            // The root directory always exists, even without an entry.
            return Some(Entry {
                path: [0; 256],
                path_len: 0,
                kind: Kind::Dir,
                data: &[],
            });
        }
        self.entries().find(|e| e.path() == path)
    }
}

pub(crate) struct Iter<'a> {
    data: &'a [u8],
    off: usize,
}

impl<'a> Iterator for Iter<'a> {
    type Item = Entry<'a>;

    fn next(&mut self) -> Option<Entry<'a>> {
        loop {
            let h = self.data.get(self.off..self.off + BLOCK)?;
            if h.iter().all(|&b| b == 0) {
                return None; // The end-of-archive marker.
            }
            let size = octal(field(h, 124, 12))?;
            let start = self.off + BLOCK;
            let data = self.data.get(start..start.checked_add(size)?)?;
            self.off = start + size.div_ceil(BLOCK) * BLOCK;

            // Ustar splits long paths into prefix + "/" + name.
            let (name, prefix) = (field(h, 0, 100), field(h, 345, 155));
            let mut path = [0u8; 256];
            let mut len = 0;
            if &h[257..262] == b"ustar" && !prefix.is_empty() {
                path[..prefix.len()].copy_from_slice(prefix);
                path[prefix.len()] = b'/';
                len = prefix.len() + 1;
            }
            path[len..len + name.len()].copy_from_slice(name);
            len += name.len();
            if core::str::from_utf8(&path[..len]).is_err() {
                continue;
            }
            let kind = match h[156] {
                b'0' | 0 => Kind::File,
                b'5' => Kind::Dir,
                _ => Kind::Other,
            };
            return Some(Entry {
                path,
                path_len: len,
                kind,
                data,
            });
        }
    }
}
