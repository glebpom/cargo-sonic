use crate::feature_mask::FeatureMask;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TargetArch {
    X86_64,
    Aarch64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CpuIdentity {
    Unknown,
    X86 {
        vendor: X86Vendor,
        family: u16,
        model: u16,
        stepping: u8,
    },
    Aarch64 {
        implementer: u16,
        part: u16,
        variant: u8,
        revision: u8,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum X86Vendor {
    Intel,
    Amd,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TargetKind {
    Generic,
    X86NeutralLevel { level: u8 },
    X86IntelCore,
    X86IntelXeon,
    X86IntelAtom,
    X86AmdZen { generation: u8 },
    X86AmdOther,
    Aarch64ArmNeoverseN,
    Aarch64ArmNeoverseV,
    Aarch64ArmNeoverseE,
    Aarch64ArmCortexA,
    Aarch64ArmCortexX,
    Aarch64Apple,
    Aarch64Ampere,
    Aarch64Other,
}

impl TargetKind {
    pub const fn is_generic(self) -> bool {
        matches!(self, Self::Generic)
    }

    pub const fn is_neutral_x86(self) -> bool {
        matches!(self, Self::X86NeutralLevel { .. })
    }

    pub const fn is_core_specific(self) -> bool {
        matches!(
            self,
            Self::X86IntelAtom
                | Self::Aarch64ArmCortexA
                | Self::Aarch64ArmCortexX
                | Self::Aarch64ArmNeoverseN
                | Self::Aarch64ArmNeoverseV
                | Self::Aarch64ArmNeoverseE
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostInfo {
    pub arch: TargetArch,
    pub features: FeatureMask,
    pub identity: CpuIdentity,
    pub heterogeneous: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VariantMeta {
    pub target_cpu: &'static str,
    pub required_features: FeatureMask,
    pub rank_features: FeatureMask,
    pub rank_feature_count: u16,
    pub feature_tier: u8,
    pub target_kind: TargetKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct Score {
    exact: u8,
    package: u8,
    unknown_neutral: u8,
    tier: u8,
    count: u16,
    weak: u8,
    generic_tie: u8,
}

pub fn select_variant<'a>(host: HostInfo, variants: &'a [VariantMeta]) -> &'a VariantMeta {
    let mut best = None::<(&VariantMeta, Score)>;

    for variant in variants {
        if !eligible(host, variant) {
            continue;
        }
        let score = score(host, variant);
        match best {
            None => best = Some((variant, score)),
            Some((current, current_score)) => {
                if score > current_score
                    || (score == current_score && variant.target_cpu < current.target_cpu)
                {
                    best = Some((variant, score));
                }
            }
        }
    }

    best.map(|(variant, _)| variant)
        .unwrap_or_else(|| &variants[0])
}

fn eligible(host: HostInfo, variant: &VariantMeta) -> bool {
    variant.target_kind.is_generic() || variant.required_features.is_subset_of(host.features)
}

fn score(host: HostInfo, variant: &VariantMeta) -> Score {
    let exact = exact_affinity(host.identity, variant.target_kind, variant.target_cpu);
    let core_specific_penalty = host.heterogeneous && variant.target_kind.is_core_specific();
    Score {
        exact: if core_specific_penalty { 0 } else { exact },
        package: if host.heterogeneous && is_package_level(variant.target_kind) {
            1
        } else {
            0
        },
        unknown_neutral: if host.identity == CpuIdentity::Unknown && variant.target_kind.is_neutral_x86()
        {
            1
        } else {
            0
        },
        tier: if core_specific_penalty { 0 } else { variant.feature_tier },
        count: if core_specific_penalty { 0 } else { variant.rank_feature_count },
        weak: if core_specific_penalty {
            0
        } else {
            weak_affinity(host.identity, variant.target_kind)
        },
        generic_tie: if variant.target_kind.is_generic() { 1 } else { 0 },
    }
}

fn is_package_level(kind: TargetKind) -> bool {
    matches!(kind, TargetKind::X86IntelCore | TargetKind::X86AmdZen { .. } | TargetKind::X86NeutralLevel { .. })
}

fn exact_affinity(identity: CpuIdentity, kind: TargetKind, target_cpu: &str) -> u8 {
    match (identity, kind) {
        (CpuIdentity::X86 { vendor: X86Vendor::Intel, family: 6, model: 183, .. }, TargetKind::X86IntelCore)
            if target_cpu == "raptorlake" => 2,
        (CpuIdentity::X86 { vendor: X86Vendor::Intel, family: 6, model: 186, .. }, TargetKind::X86IntelCore)
            if target_cpu == "raptorlake" => 2,
        (CpuIdentity::X86 { vendor: X86Vendor::Amd, family: 25, model, .. }, TargetKind::X86AmdZen { generation: 4 })
            if (0x10..=0x7f).contains(&model) => 2,
        (CpuIdentity::X86 { vendor: X86Vendor::Amd, family: 26, .. }, TargetKind::X86AmdZen { generation: 5 }) => 2,
        (CpuIdentity::Aarch64 { implementer: 0x41, part: 0xd40, .. }, TargetKind::Aarch64ArmNeoverseV)
            if target_cpu == "neoverse-v1" => 2,
        (CpuIdentity::Aarch64 { implementer: 0x41, part: 0xd0c, .. }, TargetKind::Aarch64ArmNeoverseN) => 2,
        _ => 0,
    }
}

fn weak_affinity(identity: CpuIdentity, kind: TargetKind) -> u8 {
    match (identity, kind) {
        (CpuIdentity::X86 { vendor: X86Vendor::Amd, .. }, TargetKind::X86AmdZen { .. } | TargetKind::X86AmdOther) => 1,
        (CpuIdentity::X86 { vendor: X86Vendor::Intel, .. }, TargetKind::X86IntelCore | TargetKind::X86IntelXeon | TargetKind::X86IntelAtom) => 1,
        (CpuIdentity::Aarch64 { implementer: 0x41, .. }, TargetKind::Aarch64ArmNeoverseN | TargetKind::Aarch64ArmNeoverseV | TargetKind::Aarch64ArmNeoverseE | TargetKind::Aarch64ArmCortexA | TargetKind::Aarch64ArmCortexX) => 1,
        (CpuIdentity::Aarch64 { implementer: 0xc0, .. }, TargetKind::Aarch64Ampere) => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feature_mask::{Feature, FeatureMask};

    fn mask(features: &[Feature]) -> FeatureMask {
        let mut mask = FeatureMask::EMPTY;
        for feature in features {
            mask.insert(*feature);
        }
        mask
    }

    fn v(name: &'static str, features: &[Feature], tier: u8, kind: TargetKind) -> VariantMeta {
        let mask = mask(features);
        VariantMeta {
            target_cpu: name,
            required_features: mask,
            rank_features: mask,
            rank_feature_count: mask.count(),
            feature_tier: tier,
            target_kind: kind,
        }
    }

    fn host(features: &[Feature], identity: CpuIdentity) -> HostInfo {
        HostInfo {
            arch: TargetArch::X86_64,
            features: mask(features),
            identity,
            heterogeneous: false,
        }
    }

    #[test]
    fn generic_is_always_eligible() {
        let variants = [v("generic", &[], 0, TargetKind::Generic)];
        assert_eq!(select_variant(host(&[], CpuIdentity::Unknown), &variants).target_cpu, "generic");
    }

    #[test]
    fn non_generic_requires_feature_subset() {
        let variants = [
            v("generic", &[], 0, TargetKind::Generic),
            v("haswell", &[Feature::Avx2], 3, TargetKind::X86IntelCore),
        ];
        assert_eq!(select_variant(host(&[], CpuIdentity::Unknown), &variants).target_cpu, "generic");
    }

    #[test]
    fn selects_larger_feature_set_when_no_affinity() {
        let variants = [
            v("generic", &[], 0, TargetKind::Generic),
            v("haswell", &[Feature::Avx2, Feature::Fma, Feature::Bmi1], 3, TargetKind::X86IntelCore),
        ];
        assert_eq!(select_variant(host(&[Feature::Avx2, Feature::Fma, Feature::Bmi1], CpuIdentity::Unknown), &variants).target_cpu, "haswell");
    }

    #[test]
    fn exact_affinity_beats_higher_feature_count() {
        let variants = [
            v("diamondrapids", &[Feature::Avx512F, Feature::Avx512Bw, Feature::Avx512Dq], 5, TargetKind::X86IntelXeon),
            v("znver5", &[Feature::Avx512F], 4, TargetKind::X86AmdZen { generation: 5 }),
        ];
        let identity = CpuIdentity::X86 { vendor: X86Vendor::Amd, family: 26, model: 1, stepping: 0 };
        assert_eq!(select_variant(host(&[Feature::Avx512F, Feature::Avx512Bw, Feature::Avx512Dq], identity), &variants).target_cpu, "znver5");
    }

    #[test]
    fn neutral_x86_level_beats_vendor_specific_when_identity_unknown() {
        let variants = [
            v("sapphirerapids", &[Feature::Avx512F], 4, TargetKind::X86IntelXeon),
            v("x86-64-v4", &[Feature::Avx512F], 4, TargetKind::X86NeutralLevel { level: 4 }),
            v("znver4", &[Feature::Avx512F], 4, TargetKind::X86AmdZen { generation: 4 }),
        ];
        assert_eq!(select_variant(host(&[Feature::Avx512F], CpuIdentity::Unknown), &variants).target_cpu, "x86-64-v4");
    }

    #[test]
    fn same_line_affinity_beats_neutral_when_identity_known() {
        let variants = [
            v("x86-64-v4", &[Feature::Avx512F], 4, TargetKind::X86NeutralLevel { level: 4 }),
            v("znver4", &[Feature::Avx512F], 4, TargetKind::X86AmdZen { generation: 4 }),
        ];
        let identity = CpuIdentity::X86 { vendor: X86Vendor::Amd, family: 25, model: 0x11, stepping: 0 };
        assert_eq!(select_variant(host(&[Feature::Avx512F], identity), &variants).target_cpu, "znver4");
    }

    #[test]
    fn same_feature_collision_uses_exact_affinity() {
        let variants = [
            v("neoverse-512tvb", &[Feature::Sve], 4, TargetKind::Aarch64ArmNeoverseV),
            v("neoverse-v1", &[Feature::Sve], 4, TargetKind::Aarch64ArmNeoverseV),
        ];
        let host = HostInfo {
            arch: TargetArch::Aarch64,
            features: mask(&[Feature::Sve]),
            identity: CpuIdentity::Aarch64 { implementer: 0x41, part: 0xd40, variant: 0, revision: 0 },
            heterogeneous: false,
        };
        assert_eq!(select_variant(host, &variants).target_cpu, "neoverse-v1");
    }

    #[test]
    fn same_feature_collision_prefers_generic_when_identity_unknown_and_generic_tied() {
        let variants = [
            v("oldalias", &[], 0, TargetKind::X86IntelCore),
            v("generic", &[], 0, TargetKind::Generic),
        ];
        assert_eq!(select_variant(host(&[], CpuIdentity::Unknown), &variants).target_cpu, "generic");
    }

    #[test]
    fn same_feature_collision_falls_back_to_lexical_order() {
        let variants = [
            v("skylake_avx512", &[Feature::Avx512F], 4, TargetKind::X86IntelXeon),
            v("skylake-avx512", &[Feature::Avx512F], 4, TargetKind::X86IntelXeon),
        ];
        assert_eq!(select_variant(host(&[Feature::Avx512F], CpuIdentity::Unknown), &variants).target_cpu, "skylake-avx512");
    }

    #[test]
    fn feature_tier_beats_raw_feature_count_when_no_exact_affinity() {
        let variants = [
            v("many-small", &[Feature::Sse3, Feature::Ssse3, Feature::Sse4_1, Feature::Sse4_2], 1, TargetKind::X86IntelCore),
            v("avx2", &[Feature::Avx2], 3, TargetKind::X86NeutralLevel { level: 3 }),
        ];
        assert_eq!(select_variant(host(&[Feature::Sse3, Feature::Ssse3, Feature::Sse4_1, Feature::Sse4_2, Feature::Avx2], CpuIdentity::Unknown), &variants).target_cpu, "avx2");
    }

    #[test]
    fn heterogeneous_x86_downgrades_core_specific_target() {
        let variants = [
            v("gracemont", &[Feature::Sse4_2], 1, TargetKind::X86IntelAtom),
            v("x86-64-v2", &[Feature::Sse4_2], 1, TargetKind::X86NeutralLevel { level: 2 }),
        ];
        let mut h = host(&[Feature::Sse4_2], CpuIdentity::X86 { vendor: X86Vendor::Intel, family: 6, model: 0, stepping: 0 });
        h.heterogeneous = true;
        assert_eq!(select_variant(h, &variants).target_cpu, "x86-64-v2");
    }

    #[test]
    fn exact_raptorlake_affinity_selects_raptorlake() {
        let variants = [
            v("x86-64-v3", &[Feature::Avx2], 3, TargetKind::X86NeutralLevel { level: 3 }),
            v("raptorlake", &[Feature::Avx2], 3, TargetKind::X86IntelCore),
        ];
        let identity = CpuIdentity::X86 {
            vendor: X86Vendor::Intel,
            family: 6,
            model: 183,
            stepping: 1,
        };
        assert_eq!(select_variant(host(&[Feature::Avx2], identity), &variants).target_cpu, "raptorlake");
    }

    #[test]
    fn heterogeneous_aarch64_downgrades_single_core_target() {
        let variants = [
            v("cortex-a76", &[Feature::Crc], 1, TargetKind::Aarch64ArmCortexA),
            v("generic", &[], 0, TargetKind::Generic),
        ];
        let h = HostInfo {
            arch: TargetArch::Aarch64,
            features: mask(&[Feature::Crc]),
            identity: CpuIdentity::Aarch64 { implementer: 0x41, part: 0xd0b, variant: 0, revision: 0 },
            heterogeneous: true,
        };
        assert_eq!(select_variant(h, &variants).target_cpu, "generic");
    }
}
