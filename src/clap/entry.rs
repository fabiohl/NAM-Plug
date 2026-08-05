// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Custom CLAP entry point that exposes the plugin factory and the preset discovery factory.

use crate::clap::descriptor::nam_descriptor;
use crate::clap::factory::preset_discovery::NamPresetDiscoveryFactory;
use crate::clap::plugin::NamClapPlugin;
use clack_extensions::preset_discovery::prelude::*;
use clack_plugin::entry::DefaultPluginFactory;
use clack_plugin::entry::prelude::*;
use clack_plugin::factory::plugin::PluginFactoryWrapper;
use std::ffi::CStr;

/// Custom CLAP entry that exposes both the NAM plugin factory and the preset discovery factory.
///
/// This allows hosts to discover both the plugin itself and the model preset browser
/// through a single entry point.
pub struct NamEntry {
    plugin_factory: PluginFactoryWrapper<NamPluginFactory>,
    preset_discovery_factory: PresetDiscoveryFactoryWrapper<NamPresetDiscoveryFactory>,
}

impl Entry for NamEntry {
    fn new(_plugin_path: Option<&CStr>) -> Result<Self, EntryLoadError> {
        Ok(Self {
            plugin_factory: PluginFactoryWrapper::new(NamPluginFactory {
                descriptor: nam_descriptor(),
            }),
            preset_discovery_factory: PresetDiscoveryFactoryWrapper::new(NamPresetDiscoveryFactory),
        })
    }

    fn declare_factories<'a>(&'a self, builder: &mut EntryFactories<'a>) {
        builder.register_factory(&self.plugin_factory);
        builder.register_factory(&self.preset_discovery_factory);
    }
}

/// Internal factory handling instantiation of the NAM CLAP plugin descriptor and instances.
struct NamPluginFactory {
    descriptor: PluginDescriptor,
}

impl PluginFactoryImpl for NamPluginFactory {
    fn plugin_count(&self) -> u32 {
        // NAM-Plug exposes exactly one plugin definition per binary entry point
        1
    }

    fn plugin_descriptor(&self, index: u32) -> Option<&PluginDescriptor> {
        match index {
            0 => Some(&self.descriptor),
            _ => None,
        }
    }

    fn create_plugin<'a>(
        &'a self,
        host_info: clack_plugin::host::HostInfo<'a>,
        plugin_id: &CStr,
    ) -> Option<PluginInstance<'a>> {
        // Step 1: Validate requested plugin ID against current descriptor
        if plugin_id == self.descriptor.id().unwrap_or_default() {
            // Step 2: Instaniate plugin with shared state constructor and main-thread context
            Some(PluginInstance::new::<NamClapPlugin>(
                host_info,
                &self.descriptor,
                NamClapPlugin::new_shared,
                NamClapPlugin::new_main_thread,
            ))
        } else {
            None
        }
    }
}
