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
        if let Err(err) = check_cpu_requirements() {
            log::error!("NAM-Plug: Plugin initialization rejected: {}", err);
            eprintln!("NAM-Plug: [FATAL] Plugin initialization rejected: {}", err);
            return Err(EntryLoadError);
        }

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

/// Validates whether the CPU supports all mandatory hardware extensions required by NAM-Plug.
///
/// Under the x86-64-v3 baseline, the mandatory extensions are:
/// - AVX2
/// - FMA
/// - BMI2
///
/// Accepts a feature probe closure `has_feature` to allow deterministic unit testing of error paths.
pub fn validate_cpu_features_with<F>(mut has_feature: F) -> Result<(), String>
where
    F: FnMut(&str) -> bool,
{
    #[cfg(target_arch = "x86_64")]
    {
        const REQUIRED_FEATURES: &[&str] = &["avx2", "fma", "bmi2"];
        let mut missing = Vec::new();
        for &feat in REQUIRED_FEATURES {
            if !has_feature(feat) {
                missing.push(feat);
            }
        }
        if !missing.is_empty() {
            return Err(format!(
                "CPU does not meet minimum x86-64-v3 requirements. Missing features: {}",
                missing.join(", ")
            ));
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = has_feature;
    }
    Ok(())
}

/// Checks runtime CPU features using architecture detection.
pub fn check_cpu_requirements() -> Result<(), String> {
    #[cfg(target_arch = "x86_64")]
    {
        validate_cpu_features_with(|feat| match feat {
            "avx2" => std::is_x86_feature_detected!("avx2"),
            "fma" => std::is_x86_feature_detected!("fma"),
            "bmi2" => std::is_x86_feature_detected!("bmi2"),
            _ => false,
        })
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        Ok(())
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
            // Step 2: Instantiate plugin with shared state constructor and main-thread context
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_cpu_features_all_present() {
        let result = validate_cpu_features_with(|_| true);
        assert!(result.is_ok(), "Expected Ok when all features are present");
    }

    #[test]
    fn test_validate_cpu_features_missing_avx2() {
        let result = validate_cpu_features_with(|f| f != "avx2");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("avx2"));
        assert!(!err.contains("fma"));
        assert!(!err.contains("bmi2"));
    }

    #[test]
    fn test_validate_cpu_features_missing_fma() {
        let result = validate_cpu_features_with(|f| f != "fma");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("fma"));
        assert!(!err.contains("avx2"));
        assert!(!err.contains("bmi2"));
    }

    #[test]
    fn test_validate_cpu_features_missing_bmi2() {
        let result = validate_cpu_features_with(|f| f != "bmi2");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("bmi2"));
        assert!(!err.contains("avx2"));
        assert!(!err.contains("fma"));
    }

    #[test]
    fn test_validate_cpu_features_missing_multiple() {
        let result = validate_cpu_features_with(|f| f != "avx2" && f != "bmi2");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("avx2"));
        assert!(err.contains("bmi2"));
        assert!(!err.contains("fma"));
    }

    #[test]
    fn test_check_cpu_requirements_on_host() {
        #[cfg(target_arch = "x86_64")]
        {
            let res = check_cpu_requirements();
            assert!(
                res.is_ok(),
                "Host environment expected to support required CPU baseline: {:?}",
                res
            );
        }
    }
}
