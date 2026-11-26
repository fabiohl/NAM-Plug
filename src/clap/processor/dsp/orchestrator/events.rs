// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;

impl<'a> NamClapProcessor<'a> {
    /// Parses the host input events of one callback into the scheduled-event
    /// list, shared verbatim by the wet pipeline (`process_dsp_audio`) and
    /// the two-phase reset containment leg
    /// ([`Self::process_dry_contained_block`]) so both legs apply the exact
    /// same event set — zero event loss in either path.
    #[inline(always)]
    pub(super) fn schedule_input_events(&mut self, input_events: &InputEvents) {
        let mut sorted = true;
        let mut last_time = 0usize;
        {
            let events = &mut self.scheduled_events;
            events.clear();

            for event in input_events {
                if events.len() >= MAX_SCHEDULED_EVENTS {
                    core::hint::cold_path();
                    // Explicit saturation: the flag is the observable signal
                    // (logged by emit_pending_logs off-RT). Never suppress it.
                    self.rt_status.set_flag(
                        neural_amp_modeler_rs::common::spsc::RT_STATUS_SPSC_DRAIN_TRUNCATED,
                    );
                    debug_assert!(
                        false,
                        "Event flood > {MAX_SCHEDULED_EVENTS} in one block; truncating"
                    );
                    break;
                }
                let time = event.header().time();
                if let Some(param_event) = event.as_event::<ParamValueEvent>() {
                    let Some(clap_id) = param_event.param_id() else {
                        core::hint::cold_path();
                        continue;
                    };
                    let t = time as usize;
                    sorted &= t >= last_time;
                    last_time = t;
                    events.push(ScheduledEvent {
                        time: t,
                        param_id: clap_id.get(),
                        value: param_event.value() as f32,
                        is_mod: false,
                    });
                } else if let Some(mod_event) = event.as_event::<ParamModEvent>() {
                    let Some(clap_id) = mod_event.param_id() else {
                        core::hint::cold_path();
                        continue;
                    };
                    let t = time as usize;
                    sorted &= t >= last_time;
                    last_time = t;
                    events.push(ScheduledEvent {
                        time: t,
                        param_id: clap_id.get(),
                        value: mod_event.amount() as f32,
                        is_mod: true,
                    });
                } else {
                    // Unknown/unsupported event type for this plugin.
                    core::hint::cold_path();
                }
            }
        }
        self.scheduled_events_sorted = sorted;
    }
}
