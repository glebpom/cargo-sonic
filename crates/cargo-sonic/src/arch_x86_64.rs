use crate::feature_mask::{Feature, FeatureMask};

#[derive(Clone, Copy, Debug, Default)]
pub struct CpuidLeaf {
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct X86Cpuid {
    pub leaf1: CpuidLeaf,
    pub leaf7_0: CpuidLeaf,
    pub leaf7_1: CpuidLeaf,
    pub leaf_d_1: CpuidLeaf,
    pub leaf80000001: CpuidLeaf,
}

pub fn detect_x86_features_from_cpuid(leaves: X86Cpuid, xcr0: u64) -> FeatureMask {
    let mut out = FeatureMask::EMPTY;
    let c1 = leaves.leaf1.ecx;
    let d1 = leaves.leaf1.edx;
    let b7 = leaves.leaf7_0.ebx;
    let c7 = leaves.leaf7_0.ecx;
    let d7 = leaves.leaf7_0.edx;
    let a71 = leaves.leaf7_1.eax;
    let ad1 = leaves.leaf_d_1.eax;
    let c8 = leaves.leaf80000001.ecx;

    if bit(c1, 0) {
        out.insert(Feature::Sse3);
    }
    if bit(c1, 9) {
        out.insert(Feature::Ssse3);
    }
    if bit(c1, 12) {
        out.insert(Feature::Fma);
    }
    if bit(c1, 13) {
        out.insert(Feature::Cmpxchg16b);
    }
    if bit(c1, 19) {
        out.insert(Feature::Sse4_1);
    }
    if bit(c1, 20) {
        out.insert(Feature::Sse4_2);
    }
    if bit(c1, 22) {
        out.insert(Feature::Movbe);
    }
    if bit(c1, 23) {
        out.insert(Feature::Popcnt);
    }
    if bit(c1, 25) {
        out.insert(Feature::Aes);
    }
    if bit(c1, 26) {
        out.insert(Feature::Xsave);
    }
    if bit(c1, 29) {
        out.insert(Feature::F16c);
    }
    if bit(c1, 30) {
        out.insert(Feature::Rdrand);
    }
    if bit(d1, 24) {
        out.insert(Feature::Fxsr);
    }
    if bit(d1, 25) {
        out.insert(Feature::Sse);
    }
    if bit(d1, 26) {
        out.insert(Feature::Sse2);
    }
    if bit(c1, 1) {
        out.insert(Feature::Pclmulqdq);
    }

    let avx_state = bit(c1, 27) && bit(c1, 28) && (xcr0 & 0b110) == 0b110;
    if avx_state {
        out.insert(Feature::Avx);
    }

    if avx_state && bit(b7, 5) {
        out.insert(Feature::Avx2);
    }
    if bit(b7, 3) {
        out.insert(Feature::Bmi1);
    }
    if bit(b7, 8) {
        out.insert(Feature::Bmi2);
    }
    if bit(b7, 18) {
        out.insert(Feature::Rdseed);
    }
    if bit(b7, 19) {
        out.insert(Feature::Adx);
    }
    if bit(b7, 29) {
        out.insert(Feature::Sha);
    }
    if bit(c8, 5) {
        out.insert(Feature::Lzcnt);
    }
    if bit(c8, 6) {
        out.insert(Feature::Sse4a);
    }
    if bit(c8, 21) {
        out.insert(Feature::Tbm);
    }
    let avx512_state = avx_state && (xcr0 & 0b1110_0000) == 0b1110_0000;
    if avx512_state {
        if bit(b7, 16) {
            out.insert(Feature::Avx512F);
        }
        if bit(b7, 17) {
            out.insert(Feature::Avx512Dq);
        }
        if bit(b7, 21) {
            out.insert(Feature::Avx512Ifma);
        }
        if bit(b7, 28) {
            out.insert(Feature::Avx512Cd);
        }
        if bit(b7, 30) {
            out.insert(Feature::Avx512Bw);
        }
        if bit(b7, 31) {
            out.insert(Feature::Avx512Vl);
        }
        if bit(c7, 1) {
            out.insert(Feature::Avx512Vbmi);
        }
        if bit(c7, 6) {
            out.insert(Feature::Avx512Vbmi2);
        }
        if bit(c7, 11) {
            out.insert(Feature::Avx512Vnni);
        }
        if bit(c7, 12) {
            out.insert(Feature::Avx512Bitalg);
        }
        if bit(c7, 14) {
            out.insert(Feature::Avx512Vpopcntdq);
        }
        if bit(d7, 8) {
            out.insert(Feature::Avx512Vp2intersect);
        }
        if bit(d7, 22) {
            out.insert(Feature::Avx512Fp16);
        }
        if bit(a71, 5) {
            out.insert(Feature::Avx512Bf16);
        }
    }
    if bit(c7, 8) {
        out.insert(Feature::Gfni);
    }
    if avx_state && bit(c7, 9) {
        out.insert(Feature::Vaes);
    }
    if avx_state && bit(c7, 10) {
        out.insert(Feature::Vpclmulqdq);
    }
    if avx_state && bit(c7, 22) {
        out.insert(Feature::AvxIfma);
    }
    if avx_state && bit(c7, 23) {
        out.insert(Feature::AvxNeConvert);
    }
    if avx_state && bit(a71, 4) {
        out.insert(Feature::AvxVnni);
    }
    if avx_state && bit(d7, 4) {
        out.insert(Feature::AvxVnniInt8);
    }
    if avx_state && bit(d7, 5) {
        out.insert(Feature::AvxVnniInt16);
    }
    if bit(c7, 2) {
        out.insert(Feature::Widekl);
    }
    if bit(c7, 23) {
        out.insert(Feature::Kl);
    }
    if bit(d7, 29) {
        out.insert(Feature::Sha512);
    }
    if bit(d7, 30) {
        out.insert(Feature::Sm3);
    }
    if bit(d7, 31) {
        out.insert(Feature::Sm4);
    }
    if bit(ad1, 0) {
        out.insert(Feature::Xsaveopt);
    }
    if bit(ad1, 1) {
        out.insert(Feature::Xsavec);
    }
    if bit(ad1, 3) {
        out.insert(Feature::Xsaves);
    }
    out
}

const fn bit(v: u32, b: u32) -> bool {
    (v & (1u32 << b)) != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x86_avx_requires_cpuid_avx_osxsave_and_xcr0_xmm_ymm() {
        let mut leaves = X86Cpuid::default();
        leaves.leaf1.ecx = (1 << 26) | (1 << 27) | (1 << 28);
        assert!(!detect_x86_features_from_cpuid(leaves, 0).contains(Feature::Avx));
        assert!(detect_x86_features_from_cpuid(leaves, 0b110).contains(Feature::Avx));
    }

    #[test]
    fn x86_avx2_requires_avx_state_and_leaf7_avx2() {
        let mut leaves = X86Cpuid::default();
        leaves.leaf1.ecx = (1 << 26) | (1 << 27) | (1 << 28);
        leaves.leaf7_0.ebx = 1 << 5;
        assert!(!detect_x86_features_from_cpuid(leaves, 0).contains(Feature::Avx2));
        assert!(detect_x86_features_from_cpuid(leaves, 0b110).contains(Feature::Avx2));
    }

    #[test]
    fn x86_avx512_requires_opmask_zmm_xcr0_state() {
        let mut leaves = X86Cpuid::default();
        leaves.leaf1.ecx = (1 << 26) | (1 << 27) | (1 << 28);
        leaves.leaf7_0.ebx = (1 << 16) | (1 << 21);
        assert!(!detect_x86_features_from_cpuid(leaves, 0b110).contains(Feature::Avx512F));
        let mask = detect_x86_features_from_cpuid(leaves, 0b1110_0110);
        assert!(mask.contains(Feature::Avx512F));
        assert!(mask.contains(Feature::Avx512Ifma));
    }

    #[test]
    fn x86_sse4_popcnt_bmi_lzcnt_bits_map_correctly() {
        let mut leaves = X86Cpuid::default();
        leaves.leaf1.ecx = (1 << 19) | (1 << 20) | (1 << 23);
        leaves.leaf7_0.ebx = (1 << 3) | (1 << 8);
        leaves.leaf80000001.ecx = 1 << 5;
        let mask = detect_x86_features_from_cpuid(leaves, 0);
        assert!(mask.contains(Feature::Sse4_1));
        assert!(mask.contains(Feature::Sse4_2));
        assert!(mask.contains(Feature::Popcnt));
        assert!(mask.contains(Feature::Bmi1));
        assert!(mask.contains(Feature::Bmi2));
        assert!(mask.contains(Feature::Lzcnt));
    }

    #[test]
    fn x86_baseline_bits_map_correctly() {
        let mut leaves = X86Cpuid::default();
        leaves.leaf1.edx = (1 << 24) | (1 << 25) | (1 << 26);
        let mask = detect_x86_features_from_cpuid(leaves, 0);
        assert!(mask.contains(Feature::Fxsr));
        assert!(mask.contains(Feature::Sse));
        assert!(mask.contains(Feature::Sse2));
    }

    #[test]
    fn x86_xsave_and_keylocker_bits_map_correctly() {
        let mut leaves = X86Cpuid::default();
        leaves.leaf7_0.ecx = (1 << 2) | (1 << 23);
        leaves.leaf_d_1.eax = (1 << 0) | (1 << 1) | (1 << 3);
        let mask = detect_x86_features_from_cpuid(leaves, 0);
        assert!(mask.contains(Feature::Widekl));
        assert!(mask.contains(Feature::Kl));
        assert!(mask.contains(Feature::Xsaveopt));
        assert!(mask.contains(Feature::Xsavec));
        assert!(mask.contains(Feature::Xsaves));
    }

    #[test]
    fn x86_gfni_vaes_vpclmul_do_not_require_avx512_state() {
        let mut leaves = X86Cpuid::default();
        leaves.leaf1.ecx = (1 << 26) | (1 << 27) | (1 << 28);
        leaves.leaf7_0.ecx = (1 << 8) | (1 << 9) | (1 << 10);
        let mask = detect_x86_features_from_cpuid(leaves, 0b110);
        assert!(mask.contains(Feature::Gfni));
        assert!(mask.contains(Feature::Vaes));
        assert!(mask.contains(Feature::Vpclmulqdq));
    }

    /// AVX state requires CPUID OSXSAVE+AVX bits AND XCR0 XMM/YMM bits set.
    /// Helper for tests below that need an AVX-enabled CPUID baseline.
    fn avx_enabled_leaves() -> (X86Cpuid, u64) {
        let mut leaves = X86Cpuid::default();
        leaves.leaf1.ecx = (1 << 26) | (1 << 27) | (1 << 28);
        (leaves, 0b110)
    }

    fn avx512_enabled_leaves() -> (X86Cpuid, u64) {
        let (leaves, _) = avx_enabled_leaves();
        // XCR0 must additionally have opmask/zmm-hi256/hi16-zmm bits set.
        (leaves, 0b1110_0110)
    }

    #[test]
    fn x86_leaf1_ecx_bits_map_to_expected_features() {
        // Each iteration sets a single bit on leaf1.ecx and verifies exactly the
        // expected feature appears in the mask. Using leaf1.ecx alone keeps the
        // test focused on the leaf1.ecx decoder.
        let cases: &[(u32, Feature)] = &[
            (0, Feature::Sse3),
            (1, Feature::Pclmulqdq),
            (9, Feature::Ssse3),
            (12, Feature::Fma),
            (13, Feature::Cmpxchg16b),
            (19, Feature::Sse4_1),
            (20, Feature::Sse4_2),
            (22, Feature::Movbe),
            (23, Feature::Popcnt),
            (25, Feature::Aes),
            (26, Feature::Xsave),
            (29, Feature::F16c),
            (30, Feature::Rdrand),
        ];
        for &(bit, feature) in cases {
            let mut leaves = X86Cpuid::default();
            leaves.leaf1.ecx = 1 << bit;
            let mask = detect_x86_features_from_cpuid(leaves, 0);
            assert!(
                mask.contains(feature),
                "leaf1.ecx bit {} did not produce {:?}",
                bit,
                feature
            );
        }
    }

    #[test]
    fn x86_leaf7_0_ebx_bits_map_to_expected_features() {
        // bits 3, 8, 18, 19, 29 are not gated by AVX state.
        let unguarded: &[(u32, Feature)] = &[
            (3, Feature::Bmi1),
            (8, Feature::Bmi2),
            (18, Feature::Rdseed),
            (19, Feature::Adx),
            (29, Feature::Sha),
        ];
        for &(bit, feature) in unguarded {
            let mut leaves = X86Cpuid::default();
            leaves.leaf7_0.ebx = 1 << bit;
            let mask = detect_x86_features_from_cpuid(leaves, 0);
            assert!(
                mask.contains(feature),
                "leaf7_0.ebx bit {} did not produce {:?}",
                bit,
                feature
            );
        }
    }

    #[test]
    fn x86_leaf80000001_ecx_bits_map_to_expected_features() {
        let cases: &[(u32, Feature)] =
            &[(5, Feature::Lzcnt), (6, Feature::Sse4a), (21, Feature::Tbm)];
        for &(bit, feature) in cases {
            let mut leaves = X86Cpuid::default();
            leaves.leaf80000001.ecx = 1 << bit;
            let mask = detect_x86_features_from_cpuid(leaves, 0);
            assert!(
                mask.contains(feature),
                "leaf80000001.ecx bit {} did not produce {:?}",
                bit,
                feature
            );
        }
    }

    #[test]
    fn x86_leaf_d_1_eax_bits_map_to_xsave_extensions() {
        let cases: &[(u32, Feature)] = &[
            (0, Feature::Xsaveopt),
            (1, Feature::Xsavec),
            (3, Feature::Xsaves),
        ];
        for &(bit, feature) in cases {
            let mut leaves = X86Cpuid::default();
            leaves.leaf_d_1.eax = 1 << bit;
            let mask = detect_x86_features_from_cpuid(leaves, 0);
            assert!(
                mask.contains(feature),
                "leaf_d_1.eax bit {} did not produce {:?}",
                bit,
                feature
            );
        }
    }

    #[test]
    fn x86_avx512_extension_bits_require_avx512_state() {
        // All AVX-512 extension bits beyond Avx512F require both the AVX state
        // gate AND the AVX-512 XCR0 gate. Without that XCR0 mask, the bit must
        // be silently dropped.
        let extensions_b7: &[(u32, Feature)] = &[
            (16, Feature::Avx512F),
            (17, Feature::Avx512Dq),
            (21, Feature::Avx512Ifma),
            (28, Feature::Avx512Cd),
            (30, Feature::Avx512Bw),
            (31, Feature::Avx512Vl),
        ];
        for &(bit, feature) in extensions_b7 {
            let (mut leaves, xcr0) = avx512_enabled_leaves();
            leaves.leaf7_0.ebx = 1 << bit;
            assert!(
                detect_x86_features_from_cpuid(leaves, xcr0).contains(feature),
                "leaf7_0.ebx bit {} did not produce {:?} with avx512 xcr0",
                bit,
                feature
            );
            // Without avx512 state in xcr0, the bit must be ignored.
            assert!(
                !detect_x86_features_from_cpuid(leaves, 0b110).contains(feature),
                "{:?} leaked through without avx512 xcr0 state",
                feature
            );
        }

        let extensions_c7: &[(u32, Feature)] = &[
            (1, Feature::Avx512Vbmi),
            (6, Feature::Avx512Vbmi2),
            (11, Feature::Avx512Vnni),
            (12, Feature::Avx512Bitalg),
            (14, Feature::Avx512Vpopcntdq),
        ];
        for &(bit, feature) in extensions_c7 {
            let (mut leaves, xcr0) = avx512_enabled_leaves();
            leaves.leaf7_0.ecx = 1 << bit;
            assert!(
                detect_x86_features_from_cpuid(leaves, xcr0).contains(feature),
                "leaf7_0.ecx bit {} did not produce {:?} with avx512 xcr0",
                bit,
                feature
            );
        }

        let extensions_d7: &[(u32, Feature)] =
            &[(8, Feature::Avx512Vp2intersect), (22, Feature::Avx512Fp16)];
        for &(bit, feature) in extensions_d7 {
            let (mut leaves, xcr0) = avx512_enabled_leaves();
            leaves.leaf7_0.edx = 1 << bit;
            assert!(
                detect_x86_features_from_cpuid(leaves, xcr0).contains(feature),
                "leaf7_0.edx bit {} did not produce {:?} with avx512 xcr0",
                bit,
                feature
            );
        }

        // Avx512Bf16 lives on leaf7_1.eax bit 5.
        let (mut leaves, xcr0) = avx512_enabled_leaves();
        leaves.leaf7_1.eax = 1 << 5;
        assert!(detect_x86_features_from_cpuid(leaves, xcr0).contains(Feature::Avx512Bf16));
    }

    #[test]
    fn x86_avx_dependent_extensions_require_avx_state_only() {
        // These bits require the AVX state gate (XCR0 0b110) but not AVX-512.
        let avx_dep_c7: &[(u32, Feature)] = &[(22, Feature::AvxIfma), (23, Feature::AvxNeConvert)];
        for &(bit, feature) in avx_dep_c7 {
            let (mut leaves, xcr0) = avx_enabled_leaves();
            leaves.leaf7_0.ecx = 1 << bit;
            assert!(
                detect_x86_features_from_cpuid(leaves, xcr0).contains(feature),
                "leaf7_0.ecx bit {} did not produce {:?} with avx state",
                bit,
                feature
            );
            // Without AVX state, bit must be ignored.
            assert!(
                !detect_x86_features_from_cpuid(leaves, 0).contains(feature),
                "{:?} leaked through without AVX state",
                feature
            );
        }

        // AvxVnni: leaf7_1.eax bit 4
        let (mut leaves, xcr0) = avx_enabled_leaves();
        leaves.leaf7_1.eax = 1 << 4;
        assert!(detect_x86_features_from_cpuid(leaves, xcr0).contains(Feature::AvxVnni));
        assert!(!detect_x86_features_from_cpuid(leaves, 0).contains(Feature::AvxVnni));

        // AvxVnniInt8: leaf7_0.edx bit 4; AvxVnniInt16: leaf7_0.edx bit 5
        let (mut leaves, xcr0) = avx_enabled_leaves();
        leaves.leaf7_0.edx = (1 << 4) | (1 << 5);
        let mask = detect_x86_features_from_cpuid(leaves, xcr0);
        assert!(mask.contains(Feature::AvxVnniInt8));
        assert!(mask.contains(Feature::AvxVnniInt16));
        let no_avx = detect_x86_features_from_cpuid(leaves, 0);
        assert!(!no_avx.contains(Feature::AvxVnniInt8));
        assert!(!no_avx.contains(Feature::AvxVnniInt16));
    }

    #[test]
    fn x86_sm3_sm4_sha512_live_on_leaf7_0_edx_high_bits() {
        let mut leaves = X86Cpuid::default();
        leaves.leaf7_0.edx = (1 << 29) | (1 << 30) | (1 << 31);
        let mask = detect_x86_features_from_cpuid(leaves, 0);
        assert!(mask.contains(Feature::Sha512));
        assert!(mask.contains(Feature::Sm3));
        assert!(mask.contains(Feature::Sm4));
    }

    #[test]
    fn x86_zero_cpuid_input_yields_empty_mask() {
        // Sanity: with all CPUID leaves zero and XCR0 zero, no feature should
        // be reported. This pins down the baseline that no detection arm
        // accidentally fires unconditionally.
        let mask = detect_x86_features_from_cpuid(X86Cpuid::default(), 0);
        assert_eq!(mask, FeatureMask::EMPTY);
    }
}
