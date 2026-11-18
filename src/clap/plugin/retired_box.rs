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
    /// Monotonic count of undrained pointers overwritten in the overflow slots.
    overwritten_total: AtomicUsize,
}

impl RetiredBoxOverflow {
    /// Allocates empty atomic overflow tables off-RT (`ColdShared::new`).
    pub(crate) fn new() -> Self {
        Self {
            cmds: Box::new(std::array::from_fn(|_| AtomicPtr::new(ptr::null_mut()))),
            cmd_cursor: AtomicUsize::new(0),
            slims: Box::new(std::array::from_fn(|_| AtomicPtr::new(ptr::null_mut()))),
            slim_cursor: AtomicUsize::new(0),
            overwritten_total: AtomicUsize::new(0),
        }
    }

    /// Stashes `item` into the next atomic overflow slot in O(1) without deallocating
    /// on the RT thread. If the slot already holds an undrained pointer, it is overwritten
    /// (controlled leak) and [`Self::overwritten_total`] is incremented rather than freeing
    /// on RT or scanning the table linearly.
    pub(crate) fn push(&self, item: RetiredBox) {
        let overwritten = match item {
            RetiredBox::Command(b) => {
                Self::store_ptr(&self.cmds[..], &self.cmd_cursor, Box::into_raw(b))
            }
            RetiredBox::Slimmable(b) => {
                Self::store_ptr(&self.slims[..], &self.slim_cursor, Box::into_raw(b))
            }
        };
        if overwritten {
            self.overwritten_total.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Returns total undrained pointers overwritten in the overflow table.
    #[inline]
    pub(crate) fn overwritten_total(&self) -> usize {
        self.overwritten_total.load(Ordering::Relaxed)
    }

    fn store_ptr<T>(slots: &[AtomicPtr<T>], cursor: &AtomicUsize, raw: *mut T) -> bool {
        let len = slots.len();
        // Relaxed safe: single RT producer updates cursor; happens-before with off-RT drain
        // is established by the AcqRel swap on the slot itself.
        let idx = cursor.fetch_add(1, Ordering::Relaxed) % len;
        // O(1) bounded RT write: an undrained prior pointer is intentionally leaked rather
        // than freeing on the audio thread or incurring an O(N) CAS scan.
        let old = slots[idx].swap(raw, Ordering::AcqRel);
        !old.is_null()
    }

    /// Reclaims and drops all queued boxes off-RT.
    pub(crate) fn drain(&self) -> usize {
        Self::drain_slice(&self.cmds[..]) + Self::drain_slice(&self.slims[..])
    }

    fn drain_slice<T>(slots: &[AtomicPtr<T>]) -> usize {
        let mut drained = 0;
        for slot in slots {
            let raw = slot.swap(ptr::null_mut(), Ordering::AcqRel);
            if !raw.is_null() {
                // SAFETY: `raw` was produced via `Box::into_raw` on a matching `Box<T>`
                // and published with `AcqRel` in `store_ptr`; `swap(null, AcqRel)` transfers
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_overwrite_within_slot_capacity() {
        let overflow = RetiredBoxOverflow::new();
        for _ in 0..256 {
            overflow.push(RetiredBox::Command(Box::new(ClapParamPayload::CabBypass(
                false,
            ))));
        }
        for i in 0..32 {
            overflow.push(RetiredBox::Slimmable(Box::new(SlimmableRebuild {
                generation: i as u64,
                model: None,
            })));
        }

        assert_eq!(overflow.overwritten_total(), 0);
        assert_eq!(overflow.drain(), 256 + 32);
        assert_eq!(overflow.drain(), 0);
        assert_eq!(overflow.overwritten_total(), 0);
    }

    #[test]
    fn overflow_counts_command_and_slimmable_overwrites_without_drain() {
        let overflow = RetiredBoxOverflow::new();
        let cmd_pushes = 263usize;
        for _ in 0..cmd_pushes {
            overflow.push(RetiredBox::Command(Box::new(ClapParamPayload::CabBypass(
                true,
            ))));
        }
        assert_eq!(overflow.overwritten_total(), cmd_pushes - 256);

        let slim_pushes = 37usize;
        for i in 0..slim_pushes {
            overflow.push(RetiredBox::Slimmable(Box::new(SlimmableRebuild {
                generation: i as u64,
                model: None,
            })));
        }

        let expected_overwrites = (cmd_pushes - 256) + (slim_pushes - 32);
        assert_eq!(overflow.overwritten_total(), expected_overwrites);
        assert_eq!(overflow.drain(), 256 + 32);
        assert_eq!(overflow.drain(), 0);
        assert_eq!(overflow.overwritten_total(), expected_overwrites);
    }
}
