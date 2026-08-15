// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Thread-safe command channel with acknowledgment and coalescing.
//!
//! # Architecture
//!
//! The `CommandScheduler` provides a lossless, ordered channel between the
//! Main Thread and the Audio Thread, solving three problems identified in
//! CLAP-F004:
//!
//! 1. **Coalescing** — rapid parameter automation bursts are merged so the
//!    SPSC never saturates. 10 000 host events reduce to ≤ 9 internal pushes
//!    (one per parameter, only the latest value survives).
//! 2. **Acknowledgment** — every command batch receives a monotonic sequence
//!    number. The audio thread atomically reports the last fully-drained
//!    batch, giving the main thread non-blocking confirmation of delivery.
//! 3. **Ordering** — non-coalescable commands (model load, IR swap,
//!    oversampling engine hot-swap) flush any pending coalesced parameters
//!    *before* being enqueued, preserving the total causal order.

use super::shared::ClapParamPayload;
use neural_amp_modeler_rs::common::params::RtProcessingParams;
use rtrb::{Consumer, Producer};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Default capacity for the command SPSC ring buffer.
/// 256 was chosen to safely handle parameter automation bursts
/// while keeping memory footprint minimal (~8 KiB for pointers).
pub const CMD_QUEUE_CAPACITY: usize = 256;

const PARAM_COUNT: usize = 9;

/// Error returned when the SPSC ring buffer is full and the
/// command cannot be enqueued. The caller should retry or fall
/// back to atomic-based signalling.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PushError {
    /// The destination SPSC ring buffer has no free slots.
    Full,
}

#[derive(Debug)]
struct CoalesceBuffer {
    slots: [Option<f64>; PARAM_COUNT],
    dirty_mask: u16,
}

impl Default for CoalesceBuffer {
    fn default() -> Self {
        Self {
            slots: [None; PARAM_COUNT],
            dirty_mask: 0,
        }
    }
}

impl CoalesceBuffer {
    fn set(&mut self, param_id: u32, value: f64) {
        let idx = param_id as usize;
        if idx < PARAM_COUNT {
            self.slots[idx] = Some(value);
            self.dirty_mask |= 1u16 << idx;
        }
    }

    fn take_snapshot(&mut self) -> Option<RtProcessingParams> {
        if self.dirty_mask == 0 {
            return None;
        }
        let mask = self.dirty_mask;
        self.dirty_mask = 0;

        let mut params = RtProcessingParams::default();

        if mask & (1 << 0) != 0
            && let Some(v) = self.slots[0].take()
        {
            params.input_gain_db = v as f32;
        }
        if mask & (1 << 1) != 0
            && let Some(v) = self.slots[1].take()
        {
            params.output_gain_db = v as f32;
        }
        if mask & (1 << 2) != 0
            && let Some(v) = self.slots[2].take()
        {
            params.gate_threshold_db = v as f32;
        }
        if mask & (1 << 3) != 0
            && let Some(v) = self.slots[3].take()
        {
            params.bypass = v != 0.0;
        }
        if mask & (1 << 4) != 0
            && let Some(v) = self.slots[4].take()
        {
            params.adaptive_compute =
                neural_amp_modeler_rs::common::params::AdaptiveComputeMode::from_f32(v as f32);
        }
        if mask & (1 << 5) != 0
            && let Some(v) = self.slots[5].take()
        {
            params.slim_override =
                neural_amp_modeler_rs::dsp::adaptive::SlimOverride::from_f32(v as f32);
        }
        if mask & (1 << 6) != 0
            && let Some(v) = self.slots[6].take()
        {
            params.oversample =
                neural_amp_modeler_rs::dsp::oversample::OversampleFactor::from_f32(v as f32);
        }
        if mask & (1 << 7) != 0
            && let Some(v) = self.slots[7].take()
        {
            params.activation_precision =
                neural_amp_modeler_rs::common::params::ActivationPrecision::from_f32(v as f32);
        }

        Some(params)
    }
}

/// Main-thread side of the command scheduler.
///
/// Wraps an SPSC producer with coalescing logic and acknowledgment
/// tracking. Owned exclusively by [`NamClapMainThread`](super::main_thread::NamClapMainThread).
pub struct CommandProducer<'a> {
    tx: Producer<ClapParamPayload>,
    next_seq: &'a AtomicU64,
    last_ack: &'a AtomicU64,
    coalescing: CoalesceBuffer,
}

/// Audio-thread side of the command scheduler.
///
/// Wraps an SPSC consumer. Drains commands in `process_events()` and
/// updates the atomic acknowledgment counter so the main thread can
/// confirm delivery.
pub struct CommandConsumer<'a> {
    rx: Consumer<ClapParamPayload>,
    last_ack: &'a AtomicU64,
    /// Monotonic sequence number of the last command popped from the ring.
    ///
    /// The SPSC is FIFO and the producer assigns one sequence number per
    /// pushed item (no gaps), so the `k`-th popped command carries sequence
    /// `base + k`. This field tracks the exact sequence of the most recently
    /// popped command, enabling strict `ack_up_to` semantics.
    processed_seq: u64,
}

/// Channel endpoints extracted from [`CommandScheduler`] during
/// plugin initialisation.
pub struct CommandSchedulerChannels {
    /// Producer (main-thread → audio-thread).
    pub cmd_tx: Producer<ClapParamPayload>,
    /// Consumer (audio-thread side).
    pub cmd_rx: Consumer<ClapParamPayload>,
}

/// Shared portion of the command scheduler stored in [`ColdShared`](super::shared::ColdShared).
///
/// Holds the SPSC channel ends (behind `Mutex<Option<>>` to satisfy
/// the `PluginShared` extraction protocol) and two atomic u64 for
/// sequence-number-based acknowledgment.
pub struct CommandScheduler {
    /// Lock-protected SPSC producer (main-thread side).
    pub cmd_tx: Mutex<Option<Producer<ClapParamPayload>>>,
    /// Lock-protected SPSC consumer (audio-thread side).
    pub cmd_rx: Mutex<Option<Consumer<ClapParamPayload>>>,
    /// Monotonic sequence counter incremented by the main thread.
    pub cmd_next_seq: AtomicU64,
    /// Last sequence fully drained by the audio thread (ack).
    pub cmd_last_ack: AtomicU64,
}

impl CommandScheduler {
    /// Creates a new command scheduler with a ring buffer of
    /// [`CMD_QUEUE_CAPACITY`] slots.
    pub fn new() -> Self {
        let (tx, rx) = rtrb::RingBuffer::new(CMD_QUEUE_CAPACITY);
        Self {
            cmd_tx: Mutex::new(Some(tx)),
            cmd_rx: Mutex::new(Some(rx)),
            cmd_next_seq: AtomicU64::new(0),
            cmd_last_ack: AtomicU64::new(0),
        }
    }

    /// Extracts the SPSC channel ends for exclusive ownership by the
    /// main thread and audio thread respectively. Returns `None` if
    /// already extracted.
    pub fn extract_producer_consumer(&self) -> Option<CommandSchedulerChannels> {
        let tx = self
            .cmd_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()?;
        let rx = self
            .cmd_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()?;
        Some(CommandSchedulerChannels {
            cmd_tx: tx,
            cmd_rx: rx,
        })
    }

    /// Returns previously extracted channel ends to the cold storage
    /// (used during deactivate / rollback).
    pub fn restore_channels(&self, tx: Producer<ClapParamPayload>, rx: Consumer<ClapParamPayload>) {
        if let Ok(mut g) = self.cmd_tx.lock() {
            *g = Some(tx);
        }
        if let Ok(mut g) = self.cmd_rx.lock() {
            *g = Some(rx);
        }
    }
}

impl Default for CommandScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> CommandProducer<'a> {
    /// Creates a new producer wrapping the given SPSC endpoint and
    /// ack atomics.
    pub fn new(
        tx: Producer<ClapParamPayload>,
        next_seq: &'a AtomicU64,
        last_ack: &'a AtomicU64,
    ) -> Self {
        Self {
            tx,
            next_seq,
            last_ack,
            coalescing: CoalesceBuffer::default(),
        }
    }

    /// Queues a parameter snapshot for delivery to the audio thread.
    ///
    /// Coalescing: if parameters have already been pushed since the
    /// last drain, the new snapshot overwrites the previous one
    /// (only the latest value per parameter is retained). No SPSC
    /// push occurs until [`force_flush`](Self::force_flush) or
    /// [`push_command`](Self::push_command) is called.
    ///
    /// Returns `true` if this is a new batch (not coalesced into an
    /// existing pending batch). Callers should call `force_flush()`
    /// after a batch of `push_params` to actually deliver the data.
    pub fn push_params(&mut self, params: RtProcessingParams) -> bool {
        let had_pending = self.coalescing.dirty_mask != 0;

        self.coalescing.set(0, params.input_gain_db as f64);
        self.coalescing.set(1, params.output_gain_db as f64);
        self.coalescing.set(2, params.gate_threshold_db as f64);
        self.coalescing
            .set(3, if params.bypass { 1.0 } else { 0.0 });
        self.coalescing
            .set(4, params.adaptive_compute as u32 as f64);
        self.coalescing.set(5, params.slim_override as u32 as f64);
        self.coalescing.set(6, params.oversample as u32 as f64);
        self.coalescing
            .set(7, params.activation_precision as u32 as f64);

        !had_pending
    }

    /// Queues a non-coalescable command (model load, IR swap,
    /// oversampling engine hot-swap).
    ///
    /// Any pending coalesced parameters are flushed **before** the
    /// command is enqueued, preserving causal ordering.
    ///
    /// Returns the monotonic sequence number assigned to the command
    /// batch. On saturation the command is dropped (see
    /// [`try_push_command`](Self::try_push_command) for the fail-closed
    /// variant that returns the command back to the caller).
    pub fn push_command(&mut self, cmd: ClapParamPayload) -> Result<u64, PushError> {
        self.try_push_command(cmd).map_err(|(e, _)| e)
    }

    /// Queues a non-coalescable command, returning the command back to the
    /// caller on `Full` so it can be retained for retry (fail-closed).
    ///
    /// Any pending coalesced parameters are flushed first, preserving causal
    /// ordering. Sequence numbers are only consumed **after** a successful
    /// push, guaranteeing the FIFO item↔sequence mapping has no gaps.
    pub fn try_push_command(
        &mut self,
        cmd: ClapParamPayload,
    ) -> Result<u64, (PushError, ClapParamPayload)> {
        if let Err(e) = self.force_flush() {
            return Err((e, cmd));
        }
        match self.tx.push(cmd) {
            Ok(()) => {
                let seq = self.next_seq.fetch_add(1, Ordering::Relaxed) + 1;
                Ok(seq)
            }
            Err(rtrb::PushError::Full(value)) => Err((PushError::Full, value)),
        }
    }

    /// Immediately pushes any pending coalesced parameters to the
    /// SPSC channel. No-op if the coalescing buffer is empty.
    ///
    /// Returns `Ok(seq)` with the assigned sequence number if a push
    /// occurred, or `Ok(0)` if the buffer was empty. The sequence number
    /// is consumed only after a successful push so the FIFO mapping
    /// stays gapless.
    pub fn force_flush(&mut self) -> Result<u64, PushError> {
        if let Some(snapshot) = self.coalescing.take_snapshot() {
            match self.tx.push(ClapParamPayload::Params(snapshot)) {
                Ok(()) => {
                    let seq = self.next_seq.fetch_add(1, Ordering::Relaxed) + 1;
                    Ok(seq)
                }
                Err(rtrb::PushError::Full(_)) => Err(PushError::Full),
            }
        } else {
            Ok(0)
        }
    }

    /// Returns the last sequence number acknowledged by the audio
    /// thread (non-blocking, Acquire load).
    pub fn last_acked_seq(&self) -> u64 {
        self.last_ack.load(Ordering::Acquire)
    }

    /// Spin-waits until the audio thread has acknowledged `seq`
    /// (or any higher sequence number).
    ///
    /// Call this only on the main thread when blocking is acceptable
    /// (e.g. synchronous API calls). Do **not** call on the audio
    /// thread.
    pub fn wait_for_ack(&self, seq: u64) {
        while self.last_ack.load(Ordering::Acquire) < seq {
            std::hint::spin_loop();
        }
    }

    /// Returns `true` if the audio thread has already acknowledged
    /// `seq` (non-blocking).
    pub fn is_acked(&self, seq: u64) -> bool {
        self.last_ack.load(Ordering::Acquire) >= seq
    }
}

impl<'a> CommandConsumer<'a> {
    /// Creates a new consumer wrapping the given SPSC endpoint and
    /// ack atomic. The internal processed-sequence counter is seeded from
    /// the current `last_ack` so sequence tracking survives deactivate /
    /// activate cycles (the ring and ack atomics persist across them).
    pub fn new(rx: Consumer<ClapParamPayload>, last_ack: &'a AtomicU64) -> Self {
        let processed_seq = last_ack.load(Ordering::Acquire);
        Self {
            rx,
            last_ack,
            processed_seq,
        }
    }

    /// Pops a single command from the SPSC channel (non-blocking).
    ///
    /// Advances the internal processed-sequence counter on success so
    /// [`ack_processed`](Self::ack_processed) acks the exact sequence of
    /// the last command actually consumed.
    pub(crate) fn pop(&mut self) -> Option<ClapParamPayload> {
        match self.rx.pop() {
            Ok(payload) => {
                self.processed_seq = self.processed_seq.wrapping_add(1);
                Some(payload)
            }
            Err(_) => None,
        }
    }

    /// Drains up to `max` commands from the SPSC channel, calling
    /// `process` for each one. Returns the number of commands
    /// actually drained.
    pub fn drain_and_process<F>(&mut self, max: usize, mut process: F) -> usize
    where
        F: FnMut(ClapParamPayload),
    {
        let mut count = 0;
        while count < max {
            if let Some(payload) = self.pop() {
                process(payload);
                count += 1;
            } else {
                break;
            }
        }
        count
    }

    /// Records that all commands up to sequence `seq` have been
    /// processed (Release store).
    pub fn ack_up_to(&self, seq: u64) {
        self.last_ack.store(seq, Ordering::Release);
    }

    /// Acknowledges the exact sequence number of the last command popped
    /// by this consumer (Release store). Unlike the previous `ack_latest`,
    /// this never acks commands still queued in the ring — only those
    /// actually consumed.
    pub fn ack_processed(&self) {
        self.last_ack.store(self.processed_seq, Ordering::Release);
    }

    /// Returns the inner SPSC consumer for channel restoration
    /// during deactivation.
    pub(crate) fn into_inner(self) -> Consumer<ClapParamPayload> {
        self.rx
    }
}

#[cfg(test)]
#[path = "command_scheduler_test.rs"]
mod tests;
