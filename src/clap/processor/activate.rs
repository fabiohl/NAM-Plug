// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Processor activation subsystem (`activate`).
//!
//! Handles pre-allocation of RT-safe buffers and initialization of all DSP
//! sub-components (models, resamplers, cab-sim IRs, oversamplers, and smoothers).

pub(crate) use crate::clap::extensions::cabinet::{
    CabAssetKind, CabSlotDsp, build_cab_nam_from_shared,
};
use crate::clap::extensions::params::bypass_u32_to_bool;
use crate::clap::plugin::errors::{self, static_plugin_error};
pub(crate) use crate::clap::plugin::shared::{PendingRestartOs, StagedRestore, StagedSwap};
use crate::clap::plugin::{CommandConsumer, NamClapMainThread, NamClapShared};
use crate::clap::processor::dsp::dry_delay::{DRY_DELAY_MAX_EXTRA, DryDelayLine};
use crate::clap::processor::panic_to_error;
use crate::clap::processor::prewarm_policy;
use crate::clap::processor::rollback;
use crate::clap::processor::state::{
    BypassCrossfader, CMD_SWAP_TUNABLES, NamClapProcessor, SLIMMABLE_SWAP_TUNABLES,
};
use clack_common::process::PluginAudioConfiguration;
use clack_extensions::tail::HostTail;
use clack_plugin::host::HostAudioProcessorHandle;
use clack_plugin::prelude::PluginError;
use neural_amp_modeler_rs::common::spsc::RtSwapDrain;
use neural_amp_modeler_rs::dsp::adaptive::AdaptiveCompute;
pub(crate) use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimPair;
use neural_amp_modeler_rs::dsp::gate::{DynamicHysteresis, GateParams};
use neural_amp_modeler_rs::dsp::oversample::{OversampleEngine, OversampleFactor};
use neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF;
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::dsp::smoother::ParamSmoother;
use neural_amp_modeler_rs::math::common::AlignedVec;
use neural_amp_modeler_rs::math::dsp::gain_lut::get_gain_lut;
use neural_amp_modeler_rs::models::NamModel;
use std::sync::Arc;
use std::sync::atomic::Ordering;

mod helpers;
use helpers::*;

mod build;
