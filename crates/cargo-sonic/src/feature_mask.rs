#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeatureMask {
    words: [u64; 2],
}

impl FeatureMask {
    pub const EMPTY: Self = Self { words: [0; 2] };

    pub const fn from_words(words: [u64; 2]) -> Self {
        Self { words }
    }

    pub const fn words(self) -> [u64; 2] {
        self.words
    }

    pub const fn contains(self, feature: Feature) -> bool {
        let bit = feature as usize;
        (self.words[bit / 64] & (1u64 << (bit % 64))) != 0
    }

    pub fn insert(&mut self, feature: Feature) {
        let bit = feature as usize;
        self.words[bit / 64] |= 1u64 << (bit % 64);
    }

    pub const fn union(self, other: Self) -> Self {
        Self {
            words: [
                self.words[0] | other.words[0],
                self.words[1] | other.words[1],
            ],
        }
    }

    pub const fn is_subset_of(self, host: Self) -> bool {
        (self.words[0] & !host.words[0]) == 0 && (self.words[1] & !host.words[1]) == 0
    }

    pub const fn count(self) -> u16 {
        self.words[0].count_ones() as u16 + self.words[1].count_ones() as u16
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Feature {
    Adx,
    Aes,
    Asimd,
    Avx,
    Avx2,
    Avx512Bf16,
    Avx512Bitalg,
    Avx512Bw,
    Avx512Cd,
    Avx512Dq,
    Avx512F,
    Avx512Fp16,
    Avx512Ifma,
    Avx512Vbmi,
    Avx512Vbmi2,
    Avx512Vl,
    Avx512Vnni,
    Avx512Vp2intersect,
    Avx512Vpopcntdq,
    AvxIfma,
    AvxNeConvert,
    AvxVnni,
    AvxVnniInt16,
    AvxVnniInt8,
    Bf16,
    Bmi1,
    Bmi2,
    Bti,
    Cmpxchg16b,
    Crc,
    Dit,
    Dotprod,
    Dpb,
    Dpb2,
    F16c,
    Fcma,
    Fxsr,
    Fhm,
    Flagm,
    Fma,
    Fp16,
    Frintts,
    Gfni,
    I8mm,
    Jsconv,
    Kl,
    Lor,
    Lse,
    Lzcnt,
    Movbe,
    Mte,
    Paca,
    Pacg,
    Pan,
    Pclmulqdq,
    Pmuv3,
    Popcnt,
    Rand,
    Ras,
    Rcpc,
    Rcpc2,
    Rdm,
    Rdrand,
    Rdseed,
    Sb,
    Sha,
    Sha2,
    Sha3,
    Sha512,
    Sm3,
    Sm4,
    Spe,
    Sse,
    Sse2,
    Sse3,
    Sse4_1,
    Sse4_2,
    Sse4a,
    Ssbs,
    Ssse3,
    Sve,
    Sve2,
    Tbm,
    Vaes,
    Vh,
    Vpclmulqdq,
    Widekl,
    Xsave,
    Xsavec,
    Xsaveopt,
    Xsaves,
}

pub fn feature_by_name(name: &str) -> Option<Feature> {
    Some(match name {
        "adx" => Feature::Adx,
        "aes" => Feature::Aes,
        "asimd" | "neon" => Feature::Asimd,
        "avx" => Feature::Avx,
        "avx2" => Feature::Avx2,
        "avx512bf16" => Feature::Avx512Bf16,
        "avx512bitalg" => Feature::Avx512Bitalg,
        "avx512bw" => Feature::Avx512Bw,
        "avx512cd" => Feature::Avx512Cd,
        "avx512dq" => Feature::Avx512Dq,
        "avx512f" => Feature::Avx512F,
        "avx512fp16" => Feature::Avx512Fp16,
        "avx512ifma" => Feature::Avx512Ifma,
        "avx512vbmi" => Feature::Avx512Vbmi,
        "avx512vbmi2" => Feature::Avx512Vbmi2,
        "avx512vl" => Feature::Avx512Vl,
        "avx512vnni" => Feature::Avx512Vnni,
        "avx512vp2intersect" => Feature::Avx512Vp2intersect,
        "avx512vpopcntdq" => Feature::Avx512Vpopcntdq,
        "avxifma" => Feature::AvxIfma,
        "avxneconvert" => Feature::AvxNeConvert,
        "avxvnni" => Feature::AvxVnni,
        "avxvnniint16" => Feature::AvxVnniInt16,
        "avxvnniint8" => Feature::AvxVnniInt8,
        "bf16" => Feature::Bf16,
        "bmi1" => Feature::Bmi1,
        "bmi2" => Feature::Bmi2,
        "bti" => Feature::Bti,
        "cmpxchg16b" => Feature::Cmpxchg16b,
        "crc" => Feature::Crc,
        "dit" => Feature::Dit,
        "dotprod" => Feature::Dotprod,
        "dpb" => Feature::Dpb,
        "dpb2" => Feature::Dpb2,
        "f16c" => Feature::F16c,
        "fcma" => Feature::Fcma,
        "fxsr" => Feature::Fxsr,
        "fhm" => Feature::Fhm,
        "flagm" => Feature::Flagm,
        "fma" => Feature::Fma,
        "fp16" => Feature::Fp16,
        "frintts" => Feature::Frintts,
        "gfni" => Feature::Gfni,
        "i8mm" => Feature::I8mm,
        "jsconv" => Feature::Jsconv,
        "kl" => Feature::Kl,
        "lor" => Feature::Lor,
        "lse" => Feature::Lse,
        "lzcnt" => Feature::Lzcnt,
        "movbe" => Feature::Movbe,
        "mte" => Feature::Mte,
        "paca" => Feature::Paca,
        "pacg" => Feature::Pacg,
        "pan" => Feature::Pan,
        "pclmulqdq" => Feature::Pclmulqdq,
        "pmuv3" => Feature::Pmuv3,
        "popcnt" => Feature::Popcnt,
        "rand" => Feature::Rand,
        "ras" => Feature::Ras,
        "rcpc" => Feature::Rcpc,
        "rcpc2" => Feature::Rcpc2,
        "rdm" => Feature::Rdm,
        "rdrand" => Feature::Rdrand,
        "rdseed" => Feature::Rdseed,
        "sb" => Feature::Sb,
        "sha" => Feature::Sha,
        "sha2" => Feature::Sha2,
        "sha3" => Feature::Sha3,
        "sha512" => Feature::Sha512,
        "sm3" => Feature::Sm3,
        "sm4" => Feature::Sm4,
        "spe" => Feature::Spe,
        "sse" => Feature::Sse,
        "sse2" => Feature::Sse2,
        "sse3" => Feature::Sse3,
        "sse4.1" => Feature::Sse4_1,
        "sse4.2" => Feature::Sse4_2,
        "sse4a" => Feature::Sse4a,
        "ssbs" => Feature::Ssbs,
        "ssse3" => Feature::Ssse3,
        "sve" => Feature::Sve,
        "sve2" => Feature::Sve2,
        "tbm" => Feature::Tbm,
        "vaes" => Feature::Vaes,
        "vh" => Feature::Vh,
        "vpclmulqdq" => Feature::Vpclmulqdq,
        "widekl" => Feature::Widekl,
        "xsave" => Feature::Xsave,
        "xsavec" => Feature::Xsavec,
        "xsaveopt" => Feature::Xsaveopt,
        "xsaves" => Feature::Xsaves,
        _ => return None,
    })
}

pub fn feature_name(feature: Feature) -> &'static str {
    match feature {
        Feature::Adx => "adx",
        Feature::Aes => "aes",
        Feature::Asimd => "neon",
        Feature::Avx => "avx",
        Feature::Avx2 => "avx2",
        Feature::Avx512Bf16 => "avx512bf16",
        Feature::Avx512Bitalg => "avx512bitalg",
        Feature::Avx512Bw => "avx512bw",
        Feature::Avx512Cd => "avx512cd",
        Feature::Avx512Dq => "avx512dq",
        Feature::Avx512F => "avx512f",
        Feature::Avx512Fp16 => "avx512fp16",
        Feature::Avx512Ifma => "avx512ifma",
        Feature::Avx512Vbmi => "avx512vbmi",
        Feature::Avx512Vbmi2 => "avx512vbmi2",
        Feature::Avx512Vl => "avx512vl",
        Feature::Avx512Vnni => "avx512vnni",
        Feature::Avx512Vp2intersect => "avx512vp2intersect",
        Feature::Avx512Vpopcntdq => "avx512vpopcntdq",
        Feature::AvxIfma => "avxifma",
        Feature::AvxNeConvert => "avxneconvert",
        Feature::AvxVnni => "avxvnni",
        Feature::AvxVnniInt16 => "avxvnniint16",
        Feature::AvxVnniInt8 => "avxvnniint8",
        Feature::Bf16 => "bf16",
        Feature::Bmi1 => "bmi1",
        Feature::Bmi2 => "bmi2",
        Feature::Bti => "bti",
        Feature::Cmpxchg16b => "cmpxchg16b",
        Feature::Crc => "crc",
        Feature::Dit => "dit",
        Feature::Dotprod => "dotprod",
        Feature::Dpb => "dpb",
        Feature::Dpb2 => "dpb2",
        Feature::F16c => "f16c",
        Feature::Fcma => "fcma",
        Feature::Fxsr => "fxsr",
        Feature::Fhm => "fhm",
        Feature::Flagm => "flagm",
        Feature::Fma => "fma",
        Feature::Fp16 => "fp16",
        Feature::Frintts => "frintts",
        Feature::Gfni => "gfni",
        Feature::I8mm => "i8mm",
        Feature::Jsconv => "jsconv",
        Feature::Kl => "kl",
        Feature::Lor => "lor",
        Feature::Lse => "lse",
        Feature::Lzcnt => "lzcnt",
        Feature::Movbe => "movbe",
        Feature::Mte => "mte",
        Feature::Paca => "paca",
        Feature::Pacg => "pacg",
        Feature::Pan => "pan",
        Feature::Pclmulqdq => "pclmulqdq",
        Feature::Pmuv3 => "pmuv3",
        Feature::Popcnt => "popcnt",
        Feature::Rand => "rand",
        Feature::Ras => "ras",
        Feature::Rcpc => "rcpc",
        Feature::Rcpc2 => "rcpc2",
        Feature::Rdm => "rdm",
        Feature::Rdrand => "rdrand",
        Feature::Rdseed => "rdseed",
        Feature::Sb => "sb",
        Feature::Sha => "sha",
        Feature::Sha2 => "sha2",
        Feature::Sha3 => "sha3",
        Feature::Sha512 => "sha512",
        Feature::Sm3 => "sm3",
        Feature::Sm4 => "sm4",
        Feature::Spe => "spe",
        Feature::Sse => "sse",
        Feature::Sse2 => "sse2",
        Feature::Sse3 => "sse3",
        Feature::Sse4_1 => "sse4.1",
        Feature::Sse4_2 => "sse4.2",
        Feature::Sse4a => "sse4a",
        Feature::Ssbs => "ssbs",
        Feature::Ssse3 => "ssse3",
        Feature::Sve => "sve",
        Feature::Sve2 => "sve2",
        Feature::Tbm => "tbm",
        Feature::Vaes => "vaes",
        Feature::Vh => "vh",
        Feature::Vpclmulqdq => "vpclmulqdq",
        Feature::Widekl => "widekl",
        Feature::Xsave => "xsave",
        Feature::Xsavec => "xsavec",
        Feature::Xsaveopt => "xsaveopt",
        Feature::Xsaves => "xsaves",
    }
}

pub const ALL_FEATURES: &[Feature] = &[
    Feature::Adx,
    Feature::Aes,
    Feature::Asimd,
    Feature::Avx,
    Feature::Avx2,
    Feature::Avx512Bf16,
    Feature::Avx512Bitalg,
    Feature::Avx512Bw,
    Feature::Avx512Cd,
    Feature::Avx512Dq,
    Feature::Avx512F,
    Feature::Avx512Fp16,
    Feature::Avx512Ifma,
    Feature::Avx512Vbmi,
    Feature::Avx512Vbmi2,
    Feature::Avx512Vl,
    Feature::Avx512Vnni,
    Feature::Avx512Vp2intersect,
    Feature::Avx512Vpopcntdq,
    Feature::AvxIfma,
    Feature::AvxNeConvert,
    Feature::AvxVnni,
    Feature::AvxVnniInt16,
    Feature::AvxVnniInt8,
    Feature::Bf16,
    Feature::Bmi1,
    Feature::Bmi2,
    Feature::Bti,
    Feature::Cmpxchg16b,
    Feature::Crc,
    Feature::Dit,
    Feature::Dotprod,
    Feature::Dpb,
    Feature::Dpb2,
    Feature::F16c,
    Feature::Fcma,
    Feature::Fxsr,
    Feature::Fhm,
    Feature::Flagm,
    Feature::Fma,
    Feature::Fp16,
    Feature::Frintts,
    Feature::Gfni,
    Feature::I8mm,
    Feature::Jsconv,
    Feature::Kl,
    Feature::Lor,
    Feature::Lse,
    Feature::Lzcnt,
    Feature::Movbe,
    Feature::Mte,
    Feature::Paca,
    Feature::Pacg,
    Feature::Pan,
    Feature::Pclmulqdq,
    Feature::Pmuv3,
    Feature::Popcnt,
    Feature::Rand,
    Feature::Ras,
    Feature::Rcpc,
    Feature::Rcpc2,
    Feature::Rdm,
    Feature::Rdrand,
    Feature::Rdseed,
    Feature::Sb,
    Feature::Sha,
    Feature::Sha2,
    Feature::Sha3,
    Feature::Sha512,
    Feature::Sm3,
    Feature::Sm4,
    Feature::Spe,
    Feature::Sse,
    Feature::Sse2,
    Feature::Sse3,
    Feature::Sse4_1,
    Feature::Sse4_2,
    Feature::Sse4a,
    Feature::Ssbs,
    Feature::Ssse3,
    Feature::Sve,
    Feature::Sve2,
    Feature::Tbm,
    Feature::Vaes,
    Feature::Vh,
    Feature::Vpclmulqdq,
    Feature::Widekl,
    Feature::Xsave,
    Feature::Xsavec,
    Feature::Xsaveopt,
    Feature::Xsaves,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_mask_contains_no_features() {
        let empty = FeatureMask::EMPTY;
        assert_eq!(empty.count(), 0);
        assert_eq!(empty.words(), [0, 0]);
        for &feature in ALL_FEATURES {
            assert!(!empty.contains(feature), "EMPTY contains {:?}", feature);
        }
    }

    #[test]
    fn insert_and_contains_round_trip_for_every_feature() {
        for &feature in ALL_FEATURES {
            let mut mask = FeatureMask::EMPTY;
            mask.insert(feature);
            assert!(
                mask.contains(feature),
                "insert/contains failed for {:?}",
                feature
            );
            assert_eq!(
                mask.count(),
                1,
                "count != 1 after single insert of {:?}",
                feature
            );
        }
    }

    #[test]
    fn insert_is_idempotent() {
        let mut mask = FeatureMask::EMPTY;
        mask.insert(Feature::Sse2);
        mask.insert(Feature::Sse2);
        assert_eq!(mask.count(), 1);
        assert!(mask.contains(Feature::Sse2));
    }

    #[test]
    fn from_words_and_words_round_trip() {
        let words = [0xdead_beef_cafe_babe_u64, 0x0123_4567_89ab_cdef_u64];
        assert_eq!(FeatureMask::from_words(words).words(), words);
    }

    #[test]
    fn count_matches_popcount_of_words() {
        let mask = FeatureMask::from_words([0b1011, 0b110_0001]);
        // 3 bits set in low word, 3 bits set in high word.
        assert_eq!(mask.count(), 6);
    }

    #[test]
    fn union_is_bitwise_or_of_each_word() {
        let a = FeatureMask::from_words([0b1010, 0b0101]);
        let b = FeatureMask::from_words([0b0110, 0b1001]);
        let u = a.union(b);
        assert_eq!(u.words(), [0b1110, 0b1101]);
    }

    #[test]
    fn union_preserves_features_from_both_operands() {
        let mut a = FeatureMask::EMPTY;
        a.insert(Feature::Sse2);
        let mut b = FeatureMask::EMPTY;
        b.insert(Feature::Avx2);
        let u = a.union(b);
        assert!(u.contains(Feature::Sse2));
        assert!(u.contains(Feature::Avx2));
        assert_eq!(u.count(), 2);
    }

    #[test]
    fn empty_is_subset_of_anything_and_self() {
        let empty = FeatureMask::EMPTY;
        let mut full = FeatureMask::EMPTY;
        for &feature in ALL_FEATURES {
            full.insert(feature);
        }
        assert!(empty.is_subset_of(empty));
        assert!(empty.is_subset_of(full));
        assert!(full.is_subset_of(full));
    }

    #[test]
    fn is_subset_of_rejects_when_required_bit_is_missing() {
        let mut required = FeatureMask::EMPTY;
        required.insert(Feature::Avx2);
        required.insert(Feature::Sse2);
        let mut host = FeatureMask::EMPTY;
        host.insert(Feature::Sse2);
        // host lacks Avx2 → required is NOT a subset of host.
        assert!(!required.is_subset_of(host));
        host.insert(Feature::Avx2);
        assert!(required.is_subset_of(host));
    }

    #[test]
    fn feature_by_name_returns_none_for_unknown_input() {
        assert_eq!(feature_by_name(""), None);
        assert_eq!(feature_by_name("not-a-feature"), None);
        assert_eq!(feature_by_name("AVX"), None, "lookup is case-sensitive");
    }

    #[test]
    fn feature_by_name_recognises_neon_alias_for_asimd() {
        assert_eq!(feature_by_name("neon"), Some(Feature::Asimd));
        assert_eq!(feature_by_name("asimd"), Some(Feature::Asimd));
    }

    #[test]
    fn feature_name_round_trips_through_feature_by_name_for_every_feature() {
        // Every feature in ALL_FEATURES must have a name, and that name must map
        // back to the same feature via feature_by_name. This invariant is what
        // lets the rest of the crate use these two functions as a bijection
        // (modulo the asimd↔neon alias, where the canonical name is "neon").
        for &feature in ALL_FEATURES {
            let name = feature_name(feature);
            assert!(!name.is_empty(), "empty name for {:?}", feature);
            let parsed = feature_by_name(name).unwrap_or_else(|| {
                panic!(
                    "feature_by_name({:?}) returned None for canonical name of {:?}",
                    name, feature
                )
            });
            assert_eq!(
                parsed, feature,
                "round-trip failed: feature_name({:?}) = {:?}, feature_by_name({:?}) = {:?}",
                feature, name, name, parsed
            );
        }
    }

    #[test]
    fn all_features_canonical_names_are_unique() {
        // No two features may share the same canonical name, otherwise
        // feature_name → feature_by_name would not be a function.
        let mut names: Vec<&'static str> = ALL_FEATURES.iter().map(|&f| feature_name(f)).collect();
        names.sort();
        let len_before = names.len();
        names.dedup();
        assert_eq!(
            len_before,
            names.len(),
            "feature_name produced duplicate canonical names"
        );
    }

    #[test]
    fn all_features_have_distinct_bit_positions() {
        // Two features assigned to the same bit would silently conflate in
        // FeatureMask::contains. Verify each feature occupies its own bit.
        let mut seen = FeatureMask::EMPTY;
        for &feature in ALL_FEATURES {
            assert!(
                !seen.contains(feature),
                "duplicate bit position for {:?}",
                feature
            );
            seen.insert(feature);
        }
        assert_eq!(seen.count() as usize, ALL_FEATURES.len());
    }

    #[test]
    fn feature_bits_fit_within_two_u64_words() {
        // FeatureMask uses [u64; 2], so the largest discriminant must be < 128.
        for &feature in ALL_FEATURES {
            let bit = feature as usize;
            assert!(
                bit < 128,
                "{:?} has bit {} which overflows FeatureMask",
                feature,
                bit
            );
        }
    }
}
