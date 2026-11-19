// SPDX-License-Identifier: MIT

//! Command line words.
//!
//! Splits a command line into words and redirections, expanding the shell parameters `$?`, `$!`
//! and `$$`.
//!
//! Words are separated by whitespace. A word that starts with a single quote runs to the next
//! single quote and is taken literally: it may contain spaces, may be empty (`''`) and is not
//! expanded, so `trap 'echo $?' INT` expands `$?` when the trap runs, not when it is set. A
//! trailing `&` (on its own or at the end of the last word) asks for a background job.
//!
//! An unquoted word that starts with a redirection operator is a [`Redirect`]: `[n]>`, `[n]>>`,
//! `[n]<` or `[n]>&m` / `[n]<&m`, with the target either attached (`>file`, `2>&1`) or in the next
//! word (`> file`). Redirections may appear anywhere among the words. (An operator inside a word,
//! as in `echo a>b`, is not recognised.)

use core::fmt::Write;

/// Most words one command line may have.
pub(crate) const MAX_WORDS: usize = 32;
/// Most redirections one command may have.
pub(crate) const MAX_REDIRECTS: usize = 8;

/// The values of the shell parameters.
pub(crate) struct Params {
    /// `$?`: the status of the last command.
    pub(crate) status: i32,
    /// `$!`: the pid of the last background job.
    pub(crate) last_bg: u64,
    /// `$$`: the shell's pid.
    pub(crate) pid: u64,
}

/// Why a command line could not be split.
pub(crate) enum Error {
    /// More than [`MAX_WORDS`] words or [`MAX_REDIRECTS`] redirections.
    TooMany,
    /// A single quote without its closing quote.
    Unterminated,
    /// A redirection operator at the end of the line.
    MissingTarget,
}

/// What a redirection does with its file descriptor.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RedirectKind {
    /// `>`: write to a file, created or emptied.
    Output,
    /// `>>`: append to a file, created if missing.
    Append,
    /// `<`: read from a file.
    Input,
    /// `>&` or `<&`: become a copy of another descriptor (`-` closes it).
    Duplicate,
}

/// One redirection: `fd`, what to do with it, and the file or descriptor.
#[derive(Clone, Copy)]
pub(crate) struct Redirect<'b> {
    pub(crate) fd: u64,
    pub(crate) kind: RedirectKind,
    pub(crate) target: &'b str,
}

/// A command line split into words and redirections.
pub(crate) struct Words<'b> {
    words: [&'b str; MAX_WORDS],
    count: usize,
    redirects: [Redirect<'b>; MAX_REDIRECTS],
    redirect_count: usize,
    /// Whether the line ended with `&`.
    pub(crate) background: bool,
}

impl<'b> Words<'b> {
    /// The words, without redirections or a trailing `&`.
    pub(crate) fn get(&self) -> &[&'b str] {
        &self.words[..self.count]
    }

    /// The redirections, in the order they appeared.
    pub(crate) fn redirects(&self) -> &[Redirect<'b>] {
        &self.redirects[..self.redirect_count]
    }
}

/// If `word` starts with a redirection operator: its descriptor, kind and
/// whatever follows the operator (the target, or "" if it is the next word).
fn redirection(word: &str) -> Option<(u64, RedirectKind, &str)> {
    let (fd, rest) = match word.as_bytes().first() {
        Some(d @ b'0'..=b'9') => (Some((d - b'0') as u64), &word[1..]),
        _ => (None, word),
    };
    // Longest operators first: `>>` and `>&` before `>`.
    const OPERATORS: [(&str, RedirectKind, u64); 5] = [
        (">>", RedirectKind::Append, 1),
        (">&", RedirectKind::Duplicate, 1),
        ("<&", RedirectKind::Duplicate, 0),
        (">", RedirectKind::Output, 1),
        ("<", RedirectKind::Input, 0),
    ];
    let (target, kind, default_fd) = OPERATORS
        .iter()
        .find_map(|&(op, kind, fd)| rest.strip_prefix(op).map(|t| (t, kind, fd)))?;
    Some((fd.unwrap_or(default_fd), kind, target))
}

/// Splits `line` into words, using `buf` for the expanded text.
pub(crate) fn split<'b>(
    line: &str,
    buf: &'b mut [u8],
    params: &Params,
) -> Result<Words<'b>, Error> {
    let mut spans = [(0, 0); MAX_WORDS];
    let mut quoted = [false; MAX_WORDS];
    let mut count = 0;
    let mut last_quoted = false;
    let len = {
        let mut out = Out {
            buf: &mut *buf,
            len: 0,
        };
        let mut rest = line;
        loop {
            rest = rest.trim_start_matches(|c: char| c.is_ascii_whitespace());
            if rest.is_empty() {
                break;
            }
            if count == MAX_WORDS {
                return Err(Error::TooMany);
            }
            let start = out.len;
            if let Some(quoted) = rest.strip_prefix('\'') {
                let end = quoted.find('\'').ok_or(Error::Unterminated)?;
                let _ = out.write_str(&quoted[..end]);
                rest = &quoted[end + 1..];
                last_quoted = true;
            } else {
                let end = rest
                    .find(|c: char| c.is_ascii_whitespace())
                    .unwrap_or(rest.len());
                expand(&rest[..end], &mut out, params);
                rest = &rest[end..];
                last_quoted = false;
            }
            spans[count] = (start, out.len);
            quoted[count] = last_quoted;
            count += 1;
        }
        out.len
    };
    let buf: &'b [u8] = buf;
    let text = core::str::from_utf8(&buf[..len]).unwrap_or("");
    let mut all = [""; MAX_WORDS];
    for (word, &(start, end)) in all.iter_mut().zip(&spans[..count]) {
        *word = text.get(start..end).unwrap_or("");
    }
    let mut words = Words {
        words: [""; MAX_WORDS],
        count: 0,
        redirects: [Redirect {
            fd: 0,
            kind: RedirectKind::Output,
            target: "",
        }; MAX_REDIRECTS],
        redirect_count: 0,
        background: false,
    };
    // An asynchronous list: `cmd &` or `cmd&` (but not a quoted '&').
    if count > 0 && !last_quoted {
        if all[count - 1] == "&" {
            words.background = true;
            count -= 1;
        } else if let Some(stripped) = all[count - 1].strip_suffix('&') {
            words.background = true;
            all[count - 1] = stripped;
        }
    }
    let mut i = 0;
    while i < count {
        let (word, is_quoted) = (all[i], quoted[i]);
        i += 1;
        let Some((fd, kind, mut target)) = redirection(word).filter(|_| !is_quoted) else {
            words.words[words.count] = word;
            words.count += 1;
            continue;
        };
        if target.is_empty() {
            // `> file`: the target is the next word (which may be quoted).
            if i == count {
                return Err(Error::MissingTarget);
            }
            target = all[i];
            i += 1;
        }
        if words.redirect_count == MAX_REDIRECTS {
            return Err(Error::TooMany);
        }
        words.redirects[words.redirect_count] = Redirect { fd, kind, target };
        words.redirect_count += 1;
    }
    Ok(words)
}

/// Appends `word` to `out` with the shell parameters substituted.
fn expand(word: &str, out: &mut Out<'_>, params: &Params) {
    let mut rest = word;
    while let Some(at) = rest.find('$') {
        let (before, from_dollar) = rest.split_at(at);
        let _ = out.write_str(before);
        let value = match from_dollar.as_bytes().get(1) {
            Some(b'?') => Some(params.status as i64),
            Some(b'!') => Some(params.last_bg as i64),
            Some(b'$') => Some(params.pid as i64),
            _ => None,
        };
        match value {
            Some(v) => {
                let _ = write!(out, "{}", v);
                rest = &from_dollar[2..];
            }
            None => {
                let _ = out.write_str("$");
                rest = &from_dollar[1..];
            }
        }
    }
    let _ = out.write_str(rest);
}

/// A fixed byte buffer that text can be written into, discarding overflow.
pub(crate) struct Out<'a> {
    pub(crate) buf: &'a mut [u8],
    pub(crate) len: usize,
}

impl Write for Out<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        // Whole strings only, so the text stays valid UTF-8.
        let end = self.len + s.len();
        if end > self.buf.len() {
            return Err(core::fmt::Error);
        }
        self.buf[self.len..end].copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}
