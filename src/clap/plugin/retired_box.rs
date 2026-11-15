// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Off-RT deallocation channel for consumed command and slimmable `Box` shells.
//!
//! `RtSwapHandler::install`/`discard` receive owned `Box<Payload>` from the
//! engine's `RtSwapDrain`. Unboxing or dropping a `Box` directly on the audio
//! thread invokes `GlobalAlloc::dealloc`; instead, the handlers move the inner
//! payload out and push the inert `Box` shell here so deallocation runs on the
//! main thread.

use super::shared::{ClapParamPayload, SlimmableRebuild};
use rtrb::{Consumer, Producer};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};

/// Capacity of the primary RT → Main SPSC ring for retired `Box` shells.
pub(crate) const RETIRED_BOX_QUEUE_CAPACITY: usize = 1024;

/// Heap envelope moved off the audio thread so `GlobalAlloc::dealloc` runs off-RT.
pub(crate) enum RetiredBox {
    /// Inert `Box<ClapParamPayload>` whose inner resources were already installed or retired.
    Command(Box<ClapParamPayload>),
    /// Inert `Box<SlimmableRebuild>` whose `StaticModel` was already taken.
    Slimmable(Box<SlimmableRebuild>),
}

/// Lock-free atomic overflow storage used if both the SPSC ring and the RT
/// parking lot fill during an unbounded burst without main-thread housekeeping.
pub(crate) struct RetiredBoxOverflow {
    cmds: Box<[AtomicPtr<ClapParamPayload>; 256]>,
    cmd_cursor: AtomicUsize,
    slims: Box<[AtomicPtr<SlimmableRebuild>; 32]>,
    slim_cursor: AtomicUsize,
}

impl RetiredBoxOverflow {
    /// Allocates empty atomic overflow tables off-RT (`ColdShared::new`).
    pub(crate) fn new() -> Self {
        Self {
            cmds: Box::new(std::array::from_fn(|_| AtomicPtr::new(ptr::null_mut()))),
            cmd_cursor: AtomicUsize::new(0),
            slims: Box::new(std::array::from_fn(|_| AtomicPtr::new(ptr::null_mut()))),
            slim_cursor: AtomicUsize::new(0),
        }
    }

    /// Stashes `item` into an empty atomic slot without deallocating on the RT thread.
    /// If every overflow slot is occupied, intentionally leaks rather than freeing on RT.
    pub(crate) fn push(&self, item: RetiredBox) {
        match item {
            RetiredBox::Command(b) => {
                Self::store_ptr(&self.cmds[..], &self.cmd_cursor, Box::into_raw(b))
            }
            RetiredBox::Slimmable(b) => {
                Self::store_ptr(&self.slims[..], &self.slim_cursor, Box::into_raw(b))
            }
        }
    }

    fn store_ptr<T>(slots: &[AtomicPtr<T>], cursor: &AtomicUsize, raw: *mut T) {
        let len = slots.len();
        let start = cursor.fetch_add(1, Ordering::Relaxed) % len;
        for offset in 0..len {
            let slot = &slots[(start + offset) % len];
            if slot
                .compare_exchange(ptr::null_mut(), raw, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
        }
        // Exhausted overflow slots: intentionally leak rather than freeing on RT.
    }

    /// Reclaims and drops all queued boxes off-RT.
    pub(crate) fn drain(&self) -> usize {
        Self::drain_slice(&self.cmds[..]) + Self::drain_slice(&self.slims[..])
    }

    fn drain_slice<T>(slots: &[AtomicPtr<T>]) -> usize {
        let mut drained = 0;
        for slot in slots {
            let raw = slot.swap(ptr::null_mut(), Ordering::Acquire);
            if !raw.is_null() {
                // SAFETY: `raw` was produced via `Box::into_raw` on a matching `Box<T>`
                // and published with `Release` in `store_ptr`; `swap(null)` transfers
                // sole ownership here for off-RT deallocation.
                drop(unsafe { Box::from_raw(raw) });
                drained += 1;
            }
        }
        drained
    }
}

impl Drop for RetiredBoxOverflow {
    fn drop(&mut self) {
        let _ = self.drain();
    }
}

/// Borrowed RT sink given to split-view swap handlers alongside `GcSink`.
pub(crate) struct RetiredBoxSink<'p> {
    /// Primary SPSC producer for retired `Box` shells.
    pub(crate) producer: &'p mut Producer<RetiredBox>,
    /// Fixed 16-slot RT parking lot for transient SPSC backpressure.
    pub(crate) parking_lot: &'p mut [Option<RetiredBox>; 16],
    /// Dirty flag avoiding full scans of `parking_lot` in steady state.
    pub(crate) parking_lot_dirty: &'p AtomicBool,
    /// Atomic overflow table if both SPSC and parking lot saturate.
    pub(crate) overflow: &'p RetiredBoxOverflow,
}

impl RetiredBoxSink<'_> {
    /// Retires an inert `Box` shell through SPSC → RT parking lot → atomic overflow.
    #[inline]
    pub(crate) fn retire(&mut self, item: RetiredBox) {
        match self.producer.push(item) {
            Ok(()) => {}
            Err(rtrb::PushError::Full(item)) => {
                for slot in self.parking_lot.iter_mut() {
                    if slot.is_none() {
                        *slot = Some(item);
                        self.parking_lot_dirty.store(true, Ordering::Release);
                        return;
                    }
                }
                self.overflow.push(item);
            }
        }
    }
}

/// Flushes RT-parked retired boxes back to the SPSC ring (zero allocation/drop).
#[inline]
pub(crate) fn drain_box_parking_lot(
    tx: &mut Producer<RetiredBox>,
    parking_lot: &mut [Option<RetiredBox>; 16],
    dirty: &AtomicBool,
) {
    if !dirty.load(Ordering::Acquire) {
        return;
    }
    let mut any_remaining = false;
    for slot in parking_lot.iter_mut() {
        let Some(item) = slot.take() else { continue };
        if let Err(rtrb::PushError::Full(returned)) = tx.push(item) {
            *slot = Some(returned);
            any_remaining = true;
            break;
        }
    }
    if !any_remaining {
        dirty.store(false, Ordering::Release);
    }
}

/// Off-RT drain of the retired-box SPSC consumer, parking lot, and overflow array.
pub(crate) fn drain_retired_boxes(
    rx: &mut Consumer<RetiredBox>,
    overflow: &RetiredBoxOverflow,
    parking_lot: &mut [Option<RetiredBox>; 16],
) -> usize {
    let mut drained = 0;
    while let Ok(item) = rx.pop() {
        drop(item);
        drained += 1;
    }
    for slot in parking_lot.iter_mut() {
        if let Some(item) = slot.take() {
            drop(item);
            drained += 1;
        }
    }
    drained + overflow.drain()
}
