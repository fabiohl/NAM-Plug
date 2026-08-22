// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
//! Active SIMD backend identification at runtime.

/// Returns the SIMD badge text based on the detected instruction set.
///
/// In standard production builds, this returns `"AVX2"` (the mandatory `x86-64-v3` baseline),
/// as the engine only emits and executes AVX-512 kernels when the opt-in `avx512` feature
/// is enabled on `NeuralAmpModeler-rs` (which this plugin deliberately does not enable).
///
/// The `Avx512` and `Avx512VnniBf16` match arms are retained for complete matching against
/// the engine's public stable enum.
pub fn get_simd_badge() -> &'static str {
    use neural_amp_modeler_rs::math::common::{InstructionSet, effective_instruction_set};
    #[expect(deprecated)]
    match effective_instruction_set() {
        InstructionSet::Avx2 => "AVX2",
        InstructionSet::Avx512 | InstructionSet::Avx512VnniBf16 => "AVX-512",
    }
}
