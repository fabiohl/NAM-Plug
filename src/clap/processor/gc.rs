// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Garbage collection: safe disposal of heap-allocated resources from the audio thread.
//! Must remain alloc/lock-free (array parking-lot + overflow buffer).

use super::NamClapProcessor;
use neural_amp_modeler_rs::common::spsc::GcItem;
use std::sync::atomic::Ordering;

impl<'a> NamClapProcessor<'a> {
    /// Attempts to drain items parked in the parking lot back to the SPSC GC channel.
    /// Called at the start of each process() block so that items parked during a previous
    /// swap (when the GC channel was full) are retried every audio cycle.
    /// Capacity of 16 is safe: 3 slots per swap (2 channels + 1 resampler), and draining
    /// happens on every block (~0.3–1.5ms @ 48kHz), yielding >96 slots/s throughput.
    ///
    /// F-PERF-24: fast path via the `parking_lot_dirty` Acquire latch (ported
    /// from NAM-Audio-Pipe `capture/setup.rs`): the 16-slot sweep is skipped
    /// with a single load when no GC item was ever parked since the last
    /// drain. Set with `Release` by every cascade retirement below.
    #[inline]
    pub(super) fn drain_parking_lot(&mut self) {
        if !self.parking_lot_dirty.load(Ordering::Acquire) {
            return;
        }
        let mut any_remaining = false;
        for slot in self.parking_lot.iter_mut() {
            let Some(item) = slot.take() else { continue };
            if let Err(rtrb::PushError::Full(returned)) = self.gc_tx.push(item) {
                *slot = Some(returned);
                any_remaining = true;
                break;
            }
        }
        if !any_remaining {
            self.parking_lot_dirty.store(false, Ordering::Release);
        }
    }

    /// Attempts to send an item for safe disposal (GC).
    /// If the main channel is full, falls back to the parking lot and then the overflow buffer.
    pub(super) fn push_to_gc(&mut self, item: GcItem) {
        self.parking_lot_dirty.store(true, Ordering::Release);
        neural_amp_modeler_rs::common::spsc::gc_cascade(
            Some(item),
            &mut self.gc_tx,
            &mut self.parking_lot,
            &self.gc_overflow,
            &self.rt_status,
        );
    }
}
