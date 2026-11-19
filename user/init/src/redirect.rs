// SPDX-License-Identifier: MIT

//! Redirections.
//!
//! Points a command's file descriptors at files, or at other descriptors, for the duration of
//! the command.
//!
//! Redirections apply from left to right, so `2>&1 > file` sends errors to where the output went
//! *before* it was redirected. A command that runs in the shell itself (most built-ins) has its
//! descriptors saved first and restored afterwards, as POSIX shells do, so that built-ins such as
//! `trap` or `wait` still act on the shell. A job applies its redirections in the child process,
//! where nothing needs restoring.

use crate::words::{Redirect, RedirectKind};
use haisuli_rt::abi::errno::{self, EBADF};
use haisuli_rt::abi::fcntl::{O_APPEND, O_CREAT, O_RDONLY, O_TRUNC, O_WRONLY};
use haisuli_rt::eprintln;
use haisuli_rt::sys;

/// Saved copies of descriptors go here or above, clear of the descriptors
/// 0-9 that a redirection can name.
const SAVE_FROM: u64 = 10;

/// The descriptors a command's redirections replaced, to be put back.
pub(crate) struct Saved {
    /// `(fd, copy)`: the copy of what `fd` was, or `None` if it was closed.
    saved: [(u64, Option<u64>); crate::words::MAX_REDIRECTS],
    count: usize,
}

impl Saved {
    const fn none() -> Saved {
        Saved {
            saved: [(0, None); crate::words::MAX_REDIRECTS],
            count: 0,
        }
    }

    /// Saves `fd` unless it already is; fails if no descriptor is free.
    fn save(&mut self, fd: u64) -> sys::Result<()> {
        if self.saved[..self.count].iter().any(|&(s, _)| s == fd) {
            return Ok(());
        }
        let copy = match sys::dup_from(fd, SAVE_FROM) {
            Ok(copy) => Some(copy),
            Err(e) if e.0 == EBADF => None, // Closed: close it again afterwards.
            Err(e) => return Err(e),
        };
        self.saved[self.count] = (fd, copy);
        self.count += 1;
        Ok(())
    }

    /// Puts every saved descriptor back, the last one saved first.
    pub(crate) fn restore(self) {
        for &(fd, copy) in self.saved[..self.count].iter().rev() {
            match copy {
                Some(copy) => {
                    let _ = sys::dup2(copy, fd);
                    let _ = sys::close(copy);
                }
                None => {
                    let _ = sys::close(fd);
                }
            }
        }
    }
}

/// Applies `redirects`; with `save`, so that [`Saved::restore`] can undo them.
///
/// On failure, reports the error on standard error (after undoing what was
/// applied, so on the shell's own), and returns `Err`: the command must not
/// run, and its status is 1.
pub(crate) fn apply(redirects: &[Redirect<'_>], save: bool) -> Result<Saved, ()> {
    let mut saved = Saved::none();
    for r in redirects {
        let result = if save { saved.save(r.fd) } else { Ok(()) }.and_then(|()| redirect(r));
        if let Err(e) = result {
            saved.restore();
            eprintln!("init: {}: {}", r.target, errno::describe(e.0));
            return Err(());
        }
    }
    Ok(saved)
}

/// Performs one redirection.
fn redirect(r: &Redirect<'_>) -> sys::Result<()> {
    let flags = match r.kind {
        RedirectKind::Output => O_WRONLY | O_CREAT | O_TRUNC,
        RedirectKind::Append => O_WRONLY | O_CREAT | O_APPEND,
        RedirectKind::Input => O_RDONLY,
        RedirectKind::Duplicate => {
            // `n>&-` closes n; `n>&m` makes it a copy of m.
            if r.target == "-" {
                return sys::close(r.fd).or(Ok(()));
            }
            let target = match r.target.as_bytes() {
                [d @ b'0'..=b'9'] => (d - b'0') as u64,
                _ => return Err(sys::Errno(EBADF)),
            };
            return sys::dup2(target, r.fd).map(drop);
        }
    };
    let fd = sys::open(r.target, flags)?;
    if fd != r.fd {
        let moved = sys::dup2(fd, r.fd);
        let _ = sys::close(fd);
        moved?;
    }
    Ok(())
}
