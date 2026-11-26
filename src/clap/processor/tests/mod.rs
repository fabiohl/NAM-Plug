// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

#[cfg(all(target_arch = "x86_64", feature = "heap-audit"))]
pub(crate) use crate::clap::processor::DRAIN_PROBE_NS;
#[cfg(target_arch = "x86_64")]
pub(crate) use crate::clap::processor::FtzDazGuard;
pub(crate) use crate::clap::processor::panic_to_error;

#[cfg(test)]
#[path = "../../processor_bypass_test.rs"]
pub(crate) mod processor_bypass_test;

#[cfg(test)]
#[path = "../../processor_dual_mono_test.rs"]
pub(crate) mod processor_dual_mono_test;

#[cfg(test)]
#[path = "../../processor_stress_test.rs"]
pub(crate) mod processor_stress_test;

#[cfg(test)]
#[path = "../../processor_streaming_cardinality_test.rs"]
pub(crate) mod processor_streaming_cardinality_test;

#[cfg(test)]
#[path = "../../processor_gui_test.rs"]
pub(crate) mod processor_gui_test;

#[cfg(test)]
#[path = "../../processor_state_test.rs"]
pub(crate) mod processor_state_test;

#[cfg(test)]
#[path = "../../processor_heap_audit_test.rs"]
pub(crate) mod processor_heap_audit_test;

#[cfg(test)]
#[path = "../../processor_clip_test.rs"]
pub(crate) mod processor_clip_test;

#[cfg(test)]
#[path = "../../processor_gc_stress_test.rs"]
pub(crate) mod processor_gc_stress_test;

#[cfg(test)]
#[path = "../../processor_calibration_test.rs"]
pub(crate) mod processor_calibration_test;

#[cfg(test)]
#[path = "../../processor_automation_test.rs"]
pub(crate) mod processor_automation_test;

#[cfg(test)]
#[path = "../../processor_deactivate_reactivate_test.rs"]
pub(crate) mod processor_deactivate_reactivate_test;

#[cfg(test)]
#[path = "../../processor_restart_test.rs"]
pub(crate) mod processor_restart_test;

#[cfg(test)]
#[path = "../../processor_params_integrity_test.rs"]
pub(crate) mod processor_params_integrity_test;

#[cfg(test)]
#[path = "../../processor_rt_robustness_test.rs"]
pub(crate) mod processor_rt_robustness_test;

#[cfg(test)]
#[path = "../../processor_events_test.rs"]
pub(crate) mod processor_events_test;

#[cfg(test)]
#[path = "../../processor_slimmable_generation_test.rs"]
pub(crate) mod processor_slimmable_generation_test;

#[cfg(test)]
#[path = "../../processor_command_budget_test.rs"]
pub(crate) mod processor_command_budget_test;

#[cfg(all(test, feature = "heap-audit"))]
#[path = "../../processor_drain_latency_test.rs"]
pub(crate) mod processor_drain_latency_test;

#[cfg(test)]
#[path = "../../processor_swap_handler_test.rs"]
pub(crate) mod processor_swap_handler_test;

#[cfg(test)]
#[path = "../../processor_recovered_replay_test.rs"]
pub(crate) mod processor_recovered_replay_test;

#[cfg(test)]
#[path = "../../processor_latency_policy_test.rs"]
pub(crate) mod processor_latency_policy_test;

#[cfg(test)]
#[path = "../../processor_dry_delay_test.rs"]
pub(crate) mod processor_dry_delay_test;

#[cfg(test)]
#[path = "../../processor_tail_rearm_test.rs"]
pub(crate) mod processor_tail_rearm_test;

#[cfg(test)]
#[path = "../../processor_cabsim_block_test.rs"]
pub(crate) mod processor_cabsim_block_test;

#[cfg(test)]
#[path = "../../processor_reset_test.rs"]
pub(crate) mod processor_reset_test;

#[cfg(test)]
#[path = "../../processor_diagnostics_logging_test.rs"]
pub(crate) mod processor_diagnostics_logging_test;

#[cfg(test)]
#[path = "../../processor_multi_instance_isolation_test.rs"]
pub(crate) mod processor_multi_instance_isolation_test;

#[cfg(test)]
#[path = "../../processor_temporal_validation_test.rs"]
pub(crate) mod processor_temporal_validation_test;

#[cfg(test)]
#[path = "../../processor_rt_thread_test.rs"]
pub(crate) mod processor_rt_thread_test;

#[cfg(test)]
#[path = "../../processor_ftz_guard_test.rs"]
pub(crate) mod processor_ftz_guard_test;

#[cfg(test)]
#[path = "../../processor_telemetry_test.rs"]
pub(crate) mod processor_telemetry_test;

#[cfg(test)]
#[path = "../../processor_poisoning_test.rs"]
pub(crate) mod processor_poisoning_test;

#[cfg(test)]
#[path = "../../processor_oversize_block_test.rs"]
pub(crate) mod processor_oversize_block_test;
