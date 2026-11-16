// SPDX-License-Identifier: GPL-2.0

//! Synchronisation primitives.
//!
//! A spinlock for shared mutable kernel state that panics on re-entrant use instead of
//! deadlocking.
//!
//! Kernel code runs with interrupts disabled (they are only enabled in user mode and while a CPU
//! idles, where no lock is held), so a [`Lock`] can only be contended by *other* CPUs: it waits
//! for them. Finding it held by the *same* CPU means re-entrant use, which would otherwise wait
//! forever (or, without the lock, create two `&mut` to the same data); that is a panic. Simple
//! integer state uses atomics instead, which need no unsafe code at all.
//!
//! It is a *ticket* lock: CPUs get the lock in the order they asked for it. A CPU that releases
//! and retakes a lock in a loop (a thread calling `sched_yield` over and over) would otherwise
//! keep winning it, and another CPU's timer interrupt could wait for it indefinitely.

use crate::arch::percpu;
use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

/// What [`Lock::owner`] holds while the lock is free.
const FREE: usize = 0;

pub(crate) struct Lock<T> {
    /// The next ticket to hand out.
    next: AtomicU32,
    /// The ticket whose holder may have the lock.
    serving: AtomicU32,
    /// The CPU holding the lock, plus one; [`FREE`] if nobody does. Only the holder writes its
    /// own number here, so a CPU that finds its own number holds the lock.
    owner: AtomicUsize,
    value: UnsafeCell<T>,
}

// SAFETY: Access to `value` is serialised by `owner` (see `lock`), so the
// lock may be shared like a mutex; `T: Send` because it is handed between
// whichever contexts (and CPUs) take the lock.
unsafe impl<T: Send> Sync for Lock<T> {}

impl<T> Lock<T> {
    pub(crate) const fn new(value: T) -> Self {
        Lock {
            next: AtomicU32::new(0),
            serving: AtomicU32::new(0),
            owner: AtomicUsize::new(FREE),
            value: UnsafeCell::new(value),
        }
    }

    /// Gives exclusive access until the guard is dropped, waiting while another CPU holds it.
    ///
    /// # Panics
    ///
    /// Panics if this CPU already holds the lock (re-entrant use).
    #[track_caller]
    pub(crate) fn lock(&self) -> Guard<'_, T> {
        let me = percpu::index() + 1;
        if self.owner.load(Ordering::Relaxed) == me {
            panic!("lock already held (re-entrant use)");
        }
        let ticket = self.next.fetch_add(1, Ordering::Relaxed);
        while self.serving.load(Ordering::Acquire) != ticket {
            spin_loop();
        }
        self.owner.store(me, Ordering::Relaxed);
        Guard { lock: self }
    }

    /// Like [`lock`](Self::lock), but gives up instead of waiting or panicking.
    ///
    /// For the panic handler, which must report even if the panicking code held the lock.
    pub(crate) fn try_lock(&self) -> Option<Guard<'_, T>> {
        let serving = self.serving.load(Ordering::Relaxed);
        self.next
            .compare_exchange(
                serving,
                serving.wrapping_add(1),
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .ok()?;
        self.owner.store(percpu::index() + 1, Ordering::Relaxed);
        Some(Guard { lock: self })
    }

    /// Returns the address of the protected value.
    ///
    /// This is for hardware that is told where a structure lives (e.g. the
    /// TSS). Obtaining it is harmless; the CPU's own accesses are outside
    /// Rust's aliasing model.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn addr(&self) -> u64 {
        self.value.get() as u64
    }
}

pub(crate) struct Guard<'a, T> {
    lock: &'a Lock<T>,
}

impl<T> Deref for Guard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: The guard exists only while its ticket is being served, so no
        // other reference to the value exists.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for Guard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: As in `deref`; `&mut self` makes this borrow unique.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for Guard<'_, T> {
    fn drop(&mut self) {
        self.lock.owner.store(FREE, Ordering::Relaxed);
        self.lock.serving.fetch_add(1, Ordering::Release);
    }
}
