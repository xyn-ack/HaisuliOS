// SPDX-License-Identifier: GPL-2.0

//! Process groups and sessions.
//!
//! POSIX process groups and sessions, process names, and the process listing used by `ps`.
//!
//! Every boot module leads its own session and process group; a forked child joins its
//! parent's. A job control shell puts each job in a process group of its own (`setpgid`), which
//! `kill` and `wait4` can address as a whole, and which the console server makes the terminal's
//! foreground process group. A pid stays reserved while it is in use as a process group or
//! session id (see `Table::free_slot`).

use super::{current_pml4, table, Name, State, NAME_LEN};
use crate::vmm;
use haisuli_abi::errno::{EFAULT, EINVAL, EPERM, ESRCH};
use haisuli_abi::{ProcInfo, PR_GET_NAME, PR_SET_NAME};

/// Moves process `pid` (0: the caller) into process group `pgid` (0: `pid`; the `setpgid`
/// system call).
///
/// The target must be the caller or one of its children in the same session, and must not
/// lead a session. The group must be new (`pgid` = `pid`) or exist in the caller's session.
pub(crate) fn setpgid(pid: i32, pgid: i32) -> i64 {
    if pid < 0 || pgid < 0 {
        return -EINVAL;
    }
    let mut t = table();
    let (me, my_sid) = (t.cur().pid, t.cur().sid);
    let pid = if pid == 0 { me } else { pid as u64 };
    let Some(slot) = t.live_slot(pid).filter(|&s| {
        let p = &t.procs[s];
        p.pid == me || p.ppid == me
    }) else {
        return -ESRCH;
    };
    let target = &t.procs[slot];
    if target.sid != my_sid || target.sid == target.pid {
        return -EPERM;
    }
    let pgid = if pgid == 0 { pid } else { pgid as u64 };
    if pgid != pid
        && !t
            .procs
            .iter()
            .any(|p| p.exists() && p.pgid == pgid && p.sid == my_sid)
    {
        return -EPERM;
    }
    t.procs[slot].pgid = pgid;
    0
}

/// Returns the process group id of `pid` (0: the caller; the `getpgid` system call).
pub(crate) fn getpgid(pid: i32) -> i64 {
    attribute(pid, |p| p.pgid)
}

/// Returns the caller's process group id (the `getpgrp` system call).
pub(crate) fn getpgrp() -> i64 {
    table().cur().pgid as i64
}

/// Returns the session id of `pid` (0: the caller; the `getsid` system call).
pub(crate) fn getsid(pid: i32) -> i64 {
    attribute(pid, |p| p.sid)
}

/// An attribute of process `pid` (0: the caller), which may be a zombie.
fn attribute(pid: i32, get: impl Fn(&super::Process) -> u64) -> i64 {
    if pid < 0 {
        return -ESRCH;
    }
    let t = table();
    if pid == 0 {
        return get(t.cur()) as i64;
    }
    match t.slot(pid as u64) {
        Some(slot) => get(&t.procs[slot]) as i64,
        None => -ESRCH,
    }
}

/// Makes the caller the leader of a new session and process group (the `setsid` system call).
///
/// Fails with `EPERM` if a process group with the caller's pid exists already (for example
/// because the caller leads one). Returns the new session id.
pub(crate) fn setsid() -> i64 {
    let mut t = table();
    let me = t.cur().pid;
    if t.procs.iter().any(|p| p.exists() && p.pgid == me) {
        return -EPERM;
    }
    let p = t.cur_mut();
    p.sid = me;
    p.pgid = me;
    me as i64
}

/// Describes the process with the lowest pid >= `pid` in `buf` (the `proc_info` system call).
///
/// Writes a [`ProcInfo`] record and returns that process's pid, or fails with `ESRCH` if there
/// is none.
pub(crate) fn proc_info(pid: u64, buf: u64) -> i64 {
    let info = {
        let t = table();
        let Some(p) = t
            .procs
            .iter()
            .filter(|p| p.exists() && p.pid >= pid.max(1))
            .min_by_key(|p| p.pid)
        else {
            return -ESRCH;
        };
        let state = match p.state {
            State::Zombie => b'Z',
            _ if p.stopped => b'T',
            State::Ready => b'R',
            _ => b'S',
        };
        ProcInfo {
            pid: p.pid,
            ppid: p.ppid,
            pgid: p.pgid,
            sid: p.sid,
            state,
            name: p.name.0,
        }
    };
    if !vmm::copy_to_user(current_pml4(), buf, &info.to_bytes()) {
        return -EFAULT;
    }
    info.pid as i64
}

/// Process attributes (the `prctl` system call); only the name.
///
/// `PR_SET_NAME` takes a NUL-terminated string, of which it keeps at most 15 bytes;
/// `PR_GET_NAME` writes the name, NUL-terminated, into a 16-byte buffer.
pub(crate) fn prctl(option: u64, arg: u64) -> i64 {
    let pml4 = current_pml4();
    match option {
        PR_SET_NAME => {
            let mut name = [0u8; NAME_LEN];
            for (i, c) in name.iter_mut().take(NAME_LEN - 1).enumerate() {
                let mut b = [0u8];
                if !vmm::copy_from_user(pml4, arg + i as u64, &mut b) {
                    return -EFAULT;
                }
                if b[0] == 0 {
                    break;
                }
                *c = b[0];
            }
            table().cur_mut().name = Name(name);
            0
        }
        PR_GET_NAME => {
            let name = table().cur().name.0;
            if vmm::copy_to_user(pml4, arg, &name) {
                0
            } else {
                -EFAULT
            }
        }
        _ => -EINVAL,
    }
}
