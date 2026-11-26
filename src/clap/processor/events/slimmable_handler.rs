// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! [`RtSwapHandler`] binding for the dedicated slimmable rebuild SPSC ring.

use crate::clap::plugin::retired_box::{RetiredBox, RetiredBoxSink};
use crate::clap::plugin::{NamClapShared, SlimmableRebuild};
use neural_amp_modeler_rs::common::spsc::{GcItem, GcSink, RtSwapHandler};
use neural_amp_modeler_rs::models::StaticModel;
use std::sync::atomic::Ordering;

/// [`RtSwapHandler`] binding the engine's structural-swap scheduler to the
/// dedicated slimmable payload family ([`SlimmableRebuild`]).
pub(crate) struct SlimmableSwapHandler<'p, 'a> {
    /// Active neural model (mono/primary chain), if any.
    pub(in crate::clap::processor) model_l: &'p mut Option<Box<StaticModel>>,
    /// Active neural model for the dual-mono R channel, if any.
    pub(in crate::clap::processor) model_r: &'p mut Option<Box<StaticModel>>,
    /// Generation of the active model (slimmable staleness anchor, read-only).
    pub(in crate::clap::processor) model_generation: &'p u64,
    /// Shared main<->RT state (stale-discard telemetry counter).
    pub(in crate::clap::processor) shared: &'a NamClapShared,
    /// Off-RT sink returning consumed `Box<SlimmableRebuild>` envelopes.
    pub(in crate::clap::processor) box_gc: RetiredBoxSink<'p>,
}

impl SlimmableSwapHandler<'_, '_> {
    #[inline]
    fn ack_if_still_pending(&self, request_id: u64) {
        if self.shared.cold.pending_slimmable_request() == request_id && request_id != 0 {
            self.shared.cold.complete_slimmable_request(request_id);
        }
    }
}

impl RtSwapHandler for SlimmableSwapHandler<'_, '_> {
    type Payload = SlimmableRebuild;

    #[inline(always)]
    fn is_structural(&self, _payload: &Self::Payload) -> bool {
        true
    }

    #[inline(always)]
    fn coalesce_key(&self, payload: &Self::Payload) -> Option<u64> {
        Some(payload.generation)
    }

    #[inline(always)]
    fn current_generation(&self) -> Option<u64> {
        Some(*self.model_generation)
    }

    #[inline(always)]
    fn generation_of(&self, payload: &Self::Payload) -> Option<u64> {
        Some(payload.generation)
    }

    #[cold]
    fn install(&mut self, mut payload: Box<Self::Payload>, gc: &mut GcSink<'_>) {
        let request_id = payload.request_id;
        let latest = self.shared.cold.latest_slimmable_request();
        let model = payload.model.take();
        let model_r = payload.model_r.take();
        self.box_gc.retire(RetiredBox::Slimmable(payload));
        if latest != 0 && request_id != latest {
            self.shared
                .cold
                .slimmable_stale_discarded_total
                .fetch_add(1, Ordering::Relaxed);
            if let Some(model) = model {
                gc.retire(GcItem::Model(model));
            }
            if let Some(model_r) = model_r {
                gc.retire(GcItem::Model(model_r));
            }
            return;
        }
        if self.model_l.is_none() {
            // A slimmable rebuild only makes sense over an active primary
            // model. Without one, return both built models through the GC
            // channel (never dropping on the audio thread) and leave the
            // DSP slots untouched.
            if let Some(model) = model {
                gc.retire(GcItem::Model(model));
            }
            if let Some(model_r) = model_r {
                gc.retire(GcItem::Model(model_r));
            }
            self.ack_if_still_pending(request_id);
            return;
        }
        if let Some(model) = model
            && let Some(old) = self.model_l.replace(model)
        {
            gc.retire(GcItem::Model(old));
        }
        if let Some(model_r) = model_r {
            if self.model_r.is_some() {
                if let Some(old) = self.model_r.replace(model_r) {
                    gc.retire(GcItem::Model(old));
                }
            } else {
                gc.retire(GcItem::Model(model_r));
            }
        }
        self.ack_if_still_pending(request_id);
    }

    #[cold]
    fn discard(&mut self, mut payload: Box<Self::Payload>, gc: &mut GcSink<'_>) {
        let generation = payload.generation;
        let request_id = payload.request_id;
        let latest = self.shared.cold.latest_slimmable_request();
        let model = payload.model.take();
        let model_r = payload.model_r.take();
        self.box_gc.retire(RetiredBox::Slimmable(payload));
        if generation != *self.model_generation || (latest != 0 && request_id != latest) {
            self.shared
                .cold
                .slimmable_stale_discarded_total
                .fetch_add(1, Ordering::Relaxed);
        }
        if let Some(model) = model {
            gc.retire(GcItem::Model(model));
        }
        if let Some(model_r) = model_r {
            gc.retire(GcItem::Model(model_r));
        }
        self.ack_if_still_pending(request_id);
    }
}
