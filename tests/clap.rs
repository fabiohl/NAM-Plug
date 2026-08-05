// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! CLAP Plugin Integration Test Harness Entrypoint (`NAM-Plug`).
//!
//! This module serves as the primary entrypoint for `NAM-Plug` integration tests.
//! When the `heap-audit` feature is enabled, it registers [`CountingAllocator`] as
//! the global allocator to verify zero heap allocations on the audio thread during
//! `process()` execution.
//!
//! # Submodules
//! - [`artifact_validator`]: Dynamic `.so` binary discovery and SHA256 hashing.
//! - [`clap_cross_machine`]: Cross-platform float determinism and sample rate conversion.
//! - [`clap_lifecycle_test`]: Plugin activation, audio config renegotiation, and lifecycle FSM.
//! - [`clap_multi_instance`]: Multi-instance concurrency and SPSC queue isolation.
//! - [`clap_parity_multi_sr`]: Multi-sample-rate output parity validation against `nam_rs` reference.
//! - [`clap_state_migration`]: State persistence (`clap.state-context`) and preset restoring.
//! - [`tail_semantics`]: CLAP tail extension (`clap_plugin_tail`) and silence flushing.

mod common;

use common::alloc_audit::CountingAllocator;

#[cfg_attr(not(feature = "heap-audit"), global_allocator)]
#[allow(dead_code, clippy::allow_attributes)]
static GLOBAL: CountingAllocator = CountingAllocator;

#[path = "clap/artifact_validator.rs"]
mod artifact_validator;
#[path = "clap/clap_cross_machine.rs"]
mod clap_cross_machine;
#[path = "clap/clap_lifecycle_test.rs"]
mod clap_lifecycle_test;
#[path = "clap/clap_multi_instance.rs"]
mod clap_multi_instance;
#[path = "clap/clap_parity_multi_sr.rs"]
mod clap_parity_multi_sr;
#[path = "clap/clap_state_migration.rs"]
mod clap_state_migration;
#[path = "clap/tail_semantics.rs"]
mod tail_semantics;
