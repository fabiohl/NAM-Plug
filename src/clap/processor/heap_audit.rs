// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! CLAP-specific global allocator registration for heap audit.
//!
//! The shared `CountingAllocator` and tracking infrastructure lives in
//! [`neural_amp_modeler_rs::common::alloc_audit`].

#[cfg(feature = "heap-audit")]
#[global_allocator]
static GLOBAL: neural_amp_modeler_rs::common::alloc_audit::CountingAllocator =
    neural_amp_modeler_rs::common::alloc_audit::CountingAllocator;
