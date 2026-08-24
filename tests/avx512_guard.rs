// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Binary surface guard for NAM-Plug: fail-closed proof of zero EVEX/AVX-512
//! machine code in the default (non-avx512) build (Sprint 4 / F-ROB-PLUG-10).

use neural_amp_modeler_rs::testing::bin_guard::{
    EvexScanReport, ToolKind, resolve_llvm_tool, scan_for_evex, scan_symbols,
};
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const FORBIDDEN_SYMBOLS: &[&str] = &[
    "gemv_4gate_avx512",
    "dot_product_4x_f32_avx512",
    "Avx512Math",
    "process_sample_avx512",
    "process_avx512",
    "hard_swish_slice_avx512",
    "leaky_hard_tanh_slice_avx512",
    "simd_relu_avx512",
    "relu_slice_avx512",
    "simd_silu_avx512",
    "silu_slice_avx512",
    "simd_silu_poly_avx512",
    "silu_poly_slice_avx512",
    "simd_tanh_sigmoid_dual_avx512",
];

const EVEX_VL256_MASK_ZERO: &[u8] = &[0x62, 0xf1, 0x7c, 0xa9, 0x10, 0x06];
const EVEX_VL256_MASK: &[u8] = &[0x62, 0xf1, 0x7c, 0x29, 0x11, 0x07];
const EVEX_ZMM0: &[u8] = &[0x62, 0xf1, 0x7c, 0x48, 0x10, 0x07];
const EVEX_ZMM16_31: &[u8] = &[0x62, 0xa1, 0x74, 0x40, 0x58, 0xd0];
const CLEAN_VEX: &[u8] = &[0xc5, 0xfc, 0x28, 0x02];
const CLEAN_SSE: &[u8] = &[0x48, 0x8b, 0x44, 0x24, 0x08];

fn sysroot_objdump() -> PathBuf {
    resolve_llvm_tool(ToolKind::LlvmObjdump).expect("bin_guard: llvm-objdump required")
}

fn mini_elf(text: &[u8]) -> Vec<u8> {
    const ELF_HDR: usize = 64;
    const SHDR_SIZE: usize = 64;
    const SECTION_COUNT: usize = 3;
    const SHDRTAB_OFF: usize = ELF_HDR;
    const TEXT_OFF: usize = SHDRTAB_OFF + SHDR_SIZE * SECTION_COUNT;

    let mut shstrtab = vec![0u8];
    shstrtab.extend_from_slice(b".text\0");
    shstrtab.extend_from_slice(b".shstrtab\0");
    let text_name_idx = 1usize;
    let shstrtab_name_idx = 7usize;
    let shstrtab_off = TEXT_OFF + text.len();

    let mut elf = Vec::with_capacity(shstrtab_off + shstrtab.len());

    let mut hdr = [0u8; ELF_HDR];
    hdr[0..4].copy_from_slice(b"\x7fELF");
    hdr[4] = 2; // ELFCLASS64
    hdr[5] = 1; // ELFDATA2LSB
    hdr[6] = 1; // EV_CURRENT
    hdr[7] = 0; // ELFOSABI_NONE
    hdr[16..18].copy_from_slice(&1u16.to_le_bytes()); // ET_REL
    hdr[18..20].copy_from_slice(&62u16.to_le_bytes()); // EM_X86_64
    hdr[20..24].copy_from_slice(&1u32.to_le_bytes()); // e_version
    hdr[40..48].copy_from_slice(&(SHDRTAB_OFF as u64).to_le_bytes()); // e_shoff
    hdr[52..54].copy_from_slice(&(ELF_HDR as u16).to_le_bytes()); // e_ehsize
    hdr[54..56].copy_from_slice(&0u16.to_le_bytes()); // e_phentsize
    hdr[56..58].copy_from_slice(&0u16.to_le_bytes()); // e_phnum
    hdr[58..60].copy_from_slice(&(SHDR_SIZE as u16).to_le_bytes()); // e_shentsize
    hdr[60..62].copy_from_slice(&(SECTION_COUNT as u16).to_le_bytes()); // e_shnum
    hdr[62..64].copy_from_slice(&2u16.to_le_bytes()); // e_shstrndx
    elf.extend_from_slice(&hdr);

    elf.extend_from_slice(&[0u8; SHDR_SIZE]);

    let mut sh_text = [0u8; SHDR_SIZE];
    sh_text[0..4].copy_from_slice(&(text_name_idx as u32).to_le_bytes());
    sh_text[4..8].copy_from_slice(&1u32.to_le_bytes()); // SHT_PROGBITS
    sh_text[8..16].copy_from_slice(&6u64.to_le_bytes()); // SHF_ALLOC | SHF_EXECINSTR
    sh_text[24..32].copy_from_slice(&(TEXT_OFF as u64).to_le_bytes());
    sh_text[32..40].copy_from_slice(&(text.len() as u64).to_le_bytes());
    sh_text[48..56].copy_from_slice(&16u64.to_le_bytes());
    elf.extend_from_slice(&sh_text);

    let mut sh_shstr = [0u8; SHDR_SIZE];
    sh_shstr[0..4].copy_from_slice(&(shstrtab_name_idx as u32).to_le_bytes());
    sh_shstr[4..8].copy_from_slice(&3u32.to_le_bytes()); // SHT_STRTAB
    sh_shstr[24..32].copy_from_slice(&(shstrtab_off as u64).to_le_bytes());
    sh_shstr[32..40].copy_from_slice(&(shstrtab.len() as u64).to_le_bytes());
    sh_shstr[48..56].copy_from_slice(&1u64.to_le_bytes());
    elf.extend_from_slice(&sh_shstr);

    elf.extend_from_slice(text);
    elf.extend_from_slice(&shstrtab);
    elf
}

fn write_temp(bytes: &[u8], name: &str) -> PathBuf {
    let mut dir = env::temp_dir();
    dir.push(format!("nam-plug-guard-test-{}", std::process::id()));
    fs::create_dir_all(&dir).expect("temp dir create failed");
    let path = dir.join(name);
    let mut f = fs::File::create(&path).expect("temp file create failed");
    f.write_all(bytes).expect("temp write failed");
    path
}

fn cleanup(path: &Path) {
    let _ = fs::remove_file(path);
    if let Some(parent) = path.parent() {
        let _ = fs::remove_dir(parent);
    }
}

fn scan_clean_or_panic(objdump: &Path, path: &Path, label: &str) -> EvexScanReport {
    let report = scan_for_evex(objdump, path).unwrap_or_else(|e| {
        panic!(
            "avx512_guard: fail-closed scan error on {label} ({}): {e}",
            path.display()
        )
    });
    assert!(
        report.is_clean(),
        "avx512_guard: EVEX instruction leak in {label} ({}): {:#?}",
        path.display(),
        report.evex_violations
    );
    assert!(
        report.sections_scanned > 0,
        "avx512_guard: {label} reported zero executable sections — scan is vacuous"
    );
    assert!(
        report.instructions_scanned > 0,
        "avx512_guard: {label} reported zero instructions — scan is vacuous"
    );
    report
}

#[test]
fn test_no_avx512_in_linked_test_binary() {
    let objdump = resolve_llvm_tool(ToolKind::LlvmObjdump)
        .expect("avx512_guard: llvm-objdump required for binary certification");
    let nm = resolve_llvm_tool(ToolKind::LlvmNm)
        .expect("avx512_guard: llvm-nm required for binary certification");
    let current_exe = env::current_exe().expect("avx512_guard: current_exe unavailable");

    let report = scan_clean_or_panic(&objdump, &current_exe, "linked test binary");
    eprintln!(
        "avx512_guard: certified clean — {} ({} executable sections, {} instructions).",
        current_exe.display(),
        report.sections_scanned,
        report.instructions_scanned
    );

    let sym_violations = scan_symbols(&nm, &current_exe, FORBIDDEN_SYMBOLS).unwrap_or_else(|e| {
        panic!(
            "avx512_guard: nm scan failed on {}: {e}",
            current_exe.display()
        )
    });
    assert!(
        sym_violations.is_empty(),
        "AVX-512 symbol leak in linked test binary {}: {sym_violations:?}",
        current_exe.display()
    );
}

#[test]
fn guard_rejects_evex_vl256_ymm_low_registers() {
    let path = write_temp(&mini_elf(EVEX_VL256_MASK_ZERO), "evex-vl256.o");
    let report = scan_for_evex(&sysroot_objdump(), &path).expect("scan must run");
    assert!(
        !report.is_clean(),
        "guard must reject EVEX VL256 with low ymm register"
    );
    assert_eq!(report.evex_violations.len(), 1);
    assert_eq!(report.sections_scanned, 1);
    cleanup(&path);
}

#[test]
fn guard_rejects_evex_opmask() {
    let path = write_temp(&mini_elf(EVEX_VL256_MASK), "evex-opmask.o");
    let report = scan_for_evex(&sysroot_objdump(), &path).expect("scan must run");
    assert!(!report.is_clean(), "guard must reject EVEX with opmask");
    assert_eq!(report.evex_violations.len(), 1);
    cleanup(&path);
}

#[test]
fn guard_rejects_evex_zmm0() {
    let path = write_temp(&mini_elf(EVEX_ZMM0), "evex-zmm0.o");
    let report = scan_for_evex(&sysroot_objdump(), &path).expect("scan must run");
    assert!(!report.is_clean(), "guard must reject EVEX ZMM");
    assert_eq!(report.evex_violations.len(), 1);
    cleanup(&path);
}

#[test]
fn guard_rejects_evex_zmm_high_registers() {
    let path = write_temp(&mini_elf(EVEX_ZMM16_31), "evex-zmm16.o");
    let report = scan_for_evex(&sysroot_objdump(), &path).expect("scan must run");
    assert!(
        !report.is_clean(),
        "guard must reject EVEX with zmm16..31 registers"
    );
    assert_eq!(report.evex_violations.len(), 1);
    cleanup(&path);
}

#[test]
fn guard_accepts_clean_x86_64_v3_elf() {
    let mut text = Vec::new();
    text.extend_from_slice(CLEAN_VEX);
    text.extend_from_slice(CLEAN_SSE);
    let path = write_temp(&mini_elf(&text), "clean-avx2.o");
    let report = scan_clean_or_panic(&sysroot_objdump(), &path, "clean x86-64-v3 fixture");
    assert_eq!(report.instructions_scanned, 2);
    cleanup(&path);
}
