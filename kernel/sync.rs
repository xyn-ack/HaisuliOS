// SPDX-License-Identifier: GPL-2.0

//! Synchronisation primitives.
//!
//! A lock for shared mutable kernel state that panics on re-entrant use instead of aliasing.
//!
//! HaisuliOS runs on one CPU, and kernel code runs with interrupts disabled (they are only enabled
//! in user mode and in the scheduler's idle loop, where no lock is held). So a [`Lock`] can never
//! be contended; finding it held means re-entrant use, which would create two `&mut` to the same
//! data. Instead of undefined behaviour, that is a panic. Simple integer state uses atomics
//! instead, which need no unsafe code at all. SMP will turn this into a real spinlock with
//! interrupt masking.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

pub(crate) struct Lock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: Access to `value` is serialised by `locked` (see `lock`), so the
// lock may be shared like a mutex; `T: Send` because it is handed between
// whichever contexts take the lock.
unsafe impl<T: Send> Sync for Lock<T> {}

impl<T> Lock<T> {
    pub(crate) const fn new(value: T) -> Self {
        Lock {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    /// Gives exclusive access until the guard is dropped.
    ///
    /// # Panics
    ///
    /// Panics if the lock is already held (re-entrant use).
    #[track_caller]
    pub(crate) fn lock(&self) -> Guard<'_, T> {
        if self.locked.swap(true, Ordering::Acquire) {
            panic!("lock already held (re-entrant use)");
        }
        Guard { lock: self }
    }

    /// Returns the address of the protected value.
    ///
    /// This is for hardware that is told where a structure lives (e.g. the
    /// TSS). Obtaining it is harmless; the CPU's own accesses are outside
    /// Rust's aliasing model.
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
        // SAFETY: The guard exists only while `locked` is set by it, so no
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
        self.lock.locked.store(false, Ordering::Release);
    }
}
