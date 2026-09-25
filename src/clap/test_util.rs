// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Shared test helpers for CLAP integration tests and benchmarks.
//!
//! Consolidates ~700 lines of boilerplate extracted from `processor_test.rs`
//! and reusable by `inference_bench.rs`.

#![allow(missing_docs)]

use crate::clap::NamClapPlugin;
use clack_extensions::state::PluginState;
use clack_host::prelude::*;
use neural_amp_modeler_rs::common::params::ProcessingParams;
use neural_amp_modeler_rs::dsp::pipeline::test_util::infra::{TrackingGuard, get_alloc_count};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

// ── Test host mocks ──

pub struct TestHostShared {
    /// Set to `true` when `request_restart()` is called by the plugin.
    /// Used by tests to verify CLAP restart-on-latency-change policy.
    pub restart_was_called: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
impl<'a> SharedHandler<'a> for TestHostShared {
    fn request_restart(&self) {
        self.restart_was_called
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    fn request_process(&self) {}
    fn request_callback(&self) {}
}

pub struct TestHost;
impl HostHandlers for TestHost {
    type Shared<'a> = TestHostShared;
    type MainThread<'a> = ();
    type AudioProcessor<'a> = ();
}

// ── Plugin bootstrap ──

/// Creates a fully initialized CLAP plugin entry, host info, and instance.
/// Returns all three pieces as a tuple — the caller must keep them alive
/// together (destructure with `let (_entry, _host, mut instance) = ...`).
pub fn make_test_plugin() -> (PluginEntry, HostInfo, PluginInstance<TestHost>) {
    let entry =
        PluginEntry::load_from_clack::<clack_plugin::entry::SinglePluginEntry<NamClapPlugin>>(
            c"/test",
        )
        .expect("Failed to load PluginEntry");

    let host_info = HostInfo::new("Test", "Test", "Test", "0.1.0").unwrap();

    let restart_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let instance = PluginInstance::<TestHost>::new(
        {
            let restart_flag = std::sync::Arc::clone(&restart_flag);
            move |_| TestHostShared {
                restart_was_called: restart_flag,
            }
        },
        |_| (),
        &entry,
        c"br.eti.fabiolima.nam-plug",
        &host_info,
    )
    .expect("Failed to instantiate plugin");

    (entry, host_info, instance)
}

/// Creates a fully initialized CLAP plugin entry, host info, and instance
/// by loading the plugin dynamically from a shared library (`.so`) at `so_path`.
///
/// Uses [`PluginEntry::load`] under the hood, which is inherently unsafe
/// because it executes code from an external dynamic library.
pub fn make_test_plugin_dynamic(
    so_path: &std::path::Path,
) -> (PluginEntry, HostInfo, PluginInstance<TestHost>) {
    let entry = unsafe { PluginEntry::load(so_path) }.expect("Failed to load PluginEntry from .so");

    let host_info = HostInfo::new("Test", "Test", "Test", "0.1.0").unwrap();

    let instance = PluginInstance::<TestHost>::new(
        |_| TestHostShared {
            restart_was_called: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        },
        |_| (),
        &entry,
        c"br.eti.fabiolima.nam-plug",
        &host_info,
    )
    .expect("Failed to instantiate plugin from .so");

    (entry, host_info, instance)
}

// ── Default ProcessingParams ──

/// Creates `ProcessingParams` with test-friendly defaults (all zeros/off)
/// and an optional model path.
///
/// When `model_path` names an existing file, the SHA-256 digest is computed and
/// stored in `model_hash` so the state satisfies the mandatory-hash policy.
pub fn make_default_params(model_path: Option<PathBuf>) -> ProcessingParams {
    let model_hash = model_path.as_deref().and_then(|p| {
        if p.exists() {
            crate::clap::extensions::state_transaction::compute_file_hash(p).ok()
        } else {
            None
        }
    });
    let mut params = ProcessingParams::default();
    params.model_path = model_path;
    params.model_hash = model_hash;
    params
}

/// Computes the SHA-256 hex digest of `path` with the same streaming hasher the
/// restore path uses. Returns `None` when the file cannot be hashed.
pub fn asset_hash(path: &std::path::Path) -> Option<String> {
    crate::clap::extensions::state_transaction::compute_file_hash(path).ok()
}

// ── Local fixture resolution (NAM-Plug owned; not engine registry paths) ──

/// Resolves a test model under NAM-Plug's own fixture tree.
///
/// Search order:
/// 1. `NAM_FIXTURES_DIR/{name}` when set
/// 2. `{CARGO_MANIFEST_DIR}/tests/fixtures/models/{name}`
pub fn model_path(name: &str) -> PathBuf {
    if let Ok(dir) = std::env::var("NAM_FIXTURES_DIR") {
        let p = PathBuf::from(dir).join(name);
        if p.exists() {
            return p;
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/models")
        .join(name)
}

/// Writes a minimal invalid `.nam` that exists on disk but fails model build.
pub fn write_invalid_model_fixture(path: &std::path::Path) {
    std::fs::write(
        path,
        br#"{"version":"0.5.0","architecture":"WaveNet","config":{},"weights":[]}"#,
    )
    .expect("write invalid model fixture");
}

/// Monotonic counter so parallel tests never collide on the same temp file.
static TMP_FILE_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Returns a unique temporary file path derived from `name`.
pub fn tmp_path(name: &str) -> PathBuf {
    let n = TMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("nam_plug_{}_{n}_{name}", std::process::id()))
}

/// Writes a copy of `src` (a `.nam` model) with the top-level `"sample_rate"`
/// field set to `sample_rate`, returning the temporary file path.
pub fn write_model_with_rate(src: &std::path::Path, sample_rate: u32) -> PathBuf {
    let content = std::fs::read_to_string(src).expect("read model fixture");
    let mut model: serde_json::Value = serde_json::from_str(&content).expect("valid model JSON");
    model["sample_rate"] = serde_json::json!(sample_rate as f64);
    let path = tmp_path(&format!("model_{sample_rate}.nam"));
    std::fs::write(&path, serde_json::to_vec(&model).expect("serialize model"))
        .expect("write injected model");
    path
}

// ── Shared pointer extraction ──

/// Extracts a raw pointer to `NamClapShared` from a `PluginInstance<TestHost>`.
///
/// The caller should dereference this with `unsafe { &*ptr }` and ensure
/// the `PluginInstance` outlives the dereferenced reference.
pub fn extract_shared(
    instance: &mut PluginInstance<TestHost>,
) -> *const crate::clap::plugin::NamClapShared {
    let raw_ptr = instance.plugin_handle().as_raw_ptr();
    unsafe {
        clack_plugin::extensions::wrapper::PluginWrapper::<NamClapPlugin>::handle(
            raw_ptr,
            |wrapper| Ok(wrapper.shared() as *const crate::clap::plugin::NamClapShared),
        )
    }
    .expect("Failed to get plugin wrapper")
}

// ── State loading ──

/// Gets the `PluginState` extension and loads `params` (serialized as JSON).
pub fn get_state_ext(
    instance: &mut PluginInstance<TestHost>,
) -> clack_extensions::state::PluginState {
    instance
        .plugin_handle()
        .get_extension::<PluginState>()
        .expect("State extension not found")
}

/// Serializes `params` to JSON and loads it into the plugin via `PluginState::load`.
pub fn load_plugin_state(instance: &mut PluginInstance<TestHost>, params: &ProcessingParams) {
    let state_ext = get_state_ext(instance);
    let state_bytes = serde_json::to_vec(params).unwrap();
    let handle = instance.plugin_handle();
    state_ext
        .load(&handle, &mut state_bytes.as_slice())
        .expect("Failed to load state");
}

// ── Logger verification helpers ──

/// Snapshots the global `LogBuffer` and asserts it contains a message
/// matching `expected`. Returns the full snapshot for further inspection.
pub fn assert_log_buffer_contains(expected: &str) {
    let buffer = neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::log_buffer()
        .expect("NamLogger::log_buffer() should return Some after plugin init");
    let snapshot = buffer.snapshot();
    let found = snapshot.iter().any(|r| r.message.contains(expected));
    if !found {
        let all_messages: Vec<_> = snapshot.iter().map(|r| &r.message).collect();
        panic!("LogBuffer does not contain '{expected}'.\nRecent log messages:\n{all_messages:#?}");
    }
}

/// Registers a test sink on the global `NamLogger` and returns reference holders.
///
/// Returns `(captured_messages, sink_arc)` where:
/// - `captured_messages` is a shared `Vec<(String, String)>` of (severity, message) pairs
/// - `sink_arc` must be kept alive for the `Weak` reference to remain valid
pub type TestSinkCapture = Arc<std::sync::Mutex<Vec<(String, String)>>>;
pub type TestSink = Arc<neural_amp_modeler_rs::common::diagnostics::logger::HostLogFn>;

pub fn register_test_sink() -> (TestSinkCapture, TestSink) {
    let captured: Arc<std::sync::Mutex<Vec<(String, String)>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let cap = Arc::clone(&captured);
    let sink: Arc<neural_amp_modeler_rs::common::diagnostics::logger::HostLogFn> =
        Arc::new(move |severity: &str, msg: &str| {
            if let Ok(mut v) = cap.lock() {
                v.push((severity.to_string(), msg.to_string()));
            }
        });
    let logger = neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::global()
        .expect("NamLogger::global() should be Some after plugin init");
    logger.register_sink(&sink);
    (captured, sink)
}

/// Runs `f` with allocation tracking enabled and asserts no allocations occurred.
/// `label` identifies the test context in the failure message.
///
/// With the `heap-audit` feature, `AUDIT_ENABLED` is forced off for the counted
/// window: when it is set, the processor installs its own `TrackingGuard`
/// inside `process()`, and that guard zeroes this thread's counters at entry
/// and exit — making the measured delta read as zero regardless of what `f`
/// actually allocated. Forcing it off keeps the TLS diff authoritative (the
/// processor's own `RT_STATUS_HEAP_ALLOC` telemetry, activated with
/// `AUDIT_ENABLED = true`, remains the authoritative signal in the dedicated
/// heap-audit lane). Without the feature the counting allocator is not even
/// installed, so there is nothing to mask.
pub fn assert_zero_alloc<F: FnOnce()>(label: &str, f: F) {
    #[cfg(feature = "heap-audit")]
    struct RestoreAuditEnabled(bool);

    #[cfg(feature = "heap-audit")]
    impl Drop for RestoreAuditEnabled {
        fn drop(&mut self) {
            if self.0 {
                neural_amp_modeler_rs::common::alloc_audit::AUDIT_ENABLED
                    .store(true, Ordering::Relaxed);
            }
        }
    }

    // Restores the previous global audit state on scope exit (panic-safe), so
    // concurrent heap-audit telemetry keeps its intended `AUDIT_ENABLED`.
    #[cfg(feature = "heap-audit")]
    let _restore = {
        let was_enabled =
            neural_amp_modeler_rs::common::alloc_audit::AUDIT_ENABLED.load(Ordering::Relaxed);
        neural_amp_modeler_rs::common::alloc_audit::AUDIT_ENABLED.store(false, Ordering::Relaxed);
        RestoreAuditEnabled(was_enabled)
    };

    let _guard = TrackingGuard::new();
    let before = get_alloc_count();
    f();
    let after = get_alloc_count();
    assert_eq!(
        after - before,
        0,
        "{label}: allocations detected in hot-path"
    );
}

/// Runs one stereo processing block reusing the audio ports and the output
/// event buffer owned by `bufs`.
///
/// Unlike a per-call rebuild of ports/buffers (e.g. via
/// `process_block_harness`), no allocation can originate from the harness
/// scaffolding itself, so this runner is safe to invoke inside an
/// [`assert_zero_alloc`] window: any counted allocation is attributable to the
/// plugin or the host wrapper. The `AudioPorts` must have been "warmed" with at
/// least one call before the counted window opens (first view construction
/// allocates its internal channel state).
pub fn process_stereo_block_prealloc<H: HostHandlers>(
    started: &mut StartedPluginAudioProcessor<H>,
    bufs: &mut StereoTestBuffers,
    events: Option<&InputEvents<'_>>,
) {
    let mut input_channels = [bufs.in_l.as_mut_slice(), bufs.in_r.as_mut_slice()];
    let input_audio = bufs.input_ports.with_input_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_input_only(
            input_channels.iter_mut().map(InputChannel::constant),
        ),
    }]);
    let output_channels = [bufs.out_l.as_mut_slice(), bufs.out_r.as_mut_slice()];
    let mut output_audio = bufs.output_ports.with_output_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_output_only(output_channels.into_iter()),
    }]);
    let mut output_events = OutputEvents::from_buffer(&mut bufs.output_events_buffer);

    let _ = started.process(
        &input_audio,
        &mut output_audio,
        events.unwrap_or(&InputEvents::empty()),
        &mut output_events,
        None,
        None,
    );
}

// ── Stereo audio buffers ──

/// Pre-allocated stereo audio test infrastructure.
/// Owns both the sample buffers and the CLAP `AudioPorts` / `EventBuffer`.
pub struct StereoTestBuffers {
    pub in_l: Vec<f32>,
    pub in_r: Vec<f32>,
    pub out_l: Vec<f32>,
    pub out_r: Vec<f32>,
    pub input_ports: AudioPorts,
    pub output_ports: AudioPorts,
    pub output_events_buffer: EventBuffer,
}

impl StereoTestBuffers {
    pub fn new(n: usize, in_l_val: f32, in_r_val: f32) -> Self {
        Self {
            in_l: vec![in_l_val; n],
            in_r: vec![in_r_val; n],
            out_l: vec![0.0f32; n],
            out_r: vec![0.0f32; n],
            input_ports: AudioPorts::with_capacity(2, 1),
            output_ports: AudioPorts::with_capacity(2, 1),
            output_events_buffer: EventBuffer::new(),
        }
    }
}

// ── Mono audio buffers ──

/// Pre-allocated mono audio test infrastructure.
pub struct MonoTestBuffers {
    pub in_buf: Vec<f32>,
    pub out_buf: Vec<f32>,
    pub input_ports: AudioPorts,
    pub output_ports: AudioPorts,
    pub output_events_buffer: EventBuffer,
}

impl MonoTestBuffers {
    pub fn new(n: usize, in_val: f32) -> Self {
        Self {
            in_buf: vec![in_val; n],
            out_buf: vec![0.0f32; n],
            input_ports: AudioPorts::with_capacity(1, 1),
            output_ports: AudioPorts::with_capacity(1, 1),
            output_events_buffer: EventBuffer::new(),
        }
    }
}
