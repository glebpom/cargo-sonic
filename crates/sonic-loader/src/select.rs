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
        unknown_neutral: if host.identity == CpuIdentity::Unknown
            && variant.target_kind.is_neutral_x86()
        {
            1
        } else {
            0
        },
        tier: if core_specific_penalty {
            0
        } else {
            variant.feature_tier
        },
        count: if core_specific_penalty {
            0
        } else {
            variant.rank_feature_count
        },
        weak: if core_specific_penalty {
            0
        } else {
            weak_affinity(host.identity, variant.target_kind)
        },
        generic_tie: if variant.target_kind.is_generic() {
            1
        } else {
            0
        },
    }
}

fn is_package_level(kind: TargetKind) -> bool {
    matches!(
        kind,
        TargetKind::X86IntelCore
            | TargetKind::X86AmdZen { .. }
            | TargetKind::X86NeutralLevel { .. }
    )
}

fn exact_affinity(identity: CpuIdentity, kind: TargetKind, target_cpu: &str) -> u8 {
    match (identity, kind) {
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Intel,
                family: 15,
                model: 6,
                ..
            },
            TargetKind::X86IntelCore,
        ) if target_cpu == "nocona" => 2,
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Intel,
                family: 6,
                model: 15,
                ..
            },
            TargetKind::X86IntelCore,
        ) if target_cpu == "core2" => 2,
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Intel,
                family: 6,
                model: 23,
                ..
            },
            TargetKind::X86IntelCore,
        ) if target_cpu == "penryn" => 2,
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Intel,
                family: 6,
                model: 26 | 30 | 31 | 46,
                ..
            },
            TargetKind::X86IntelCore,
        ) if target_cpu == "nehalem" => 2,
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Intel,
                family: 6,
                model: 37 | 44 | 47,
                ..
            },
            TargetKind::X86IntelCore,
        ) if target_cpu == "westmere" => 2,
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Intel,
                family: 6,
                model: 42 | 45,
                ..
            },
            TargetKind::X86IntelCore,
        ) if target_cpu == "sandybridge" => 2,
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Intel,
                family: 6,
                model: 58 | 62,
                ..
            },
            TargetKind::X86IntelCore,
        ) if target_cpu == "ivybridge" => 2,
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Intel,
                family: 6,
                model: 60 | 63 | 69 | 70,
                ..
            },
            TargetKind::X86IntelCore,
        ) if target_cpu == "haswell" => 2,
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Intel,
                family: 6,
                model: 61 | 71 | 79 | 86,
                ..
            },
            TargetKind::X86IntelCore,
        ) if target_cpu == "broadwell" => 2,
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Intel,
                family: 6,
                model: 94 | 78,
                ..
            },
            TargetKind::X86IntelCore,
        ) if target_cpu == "skylake" => 2,
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Intel,
                family: 6,
                model: 167,
                ..
            },
            TargetKind::X86IntelCore,
        ) if target_cpu == "rocketlake" => 2,
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Intel,
                family: 6,
                model: 151 | 154,
                ..
            },
            TargetKind::X86IntelCore,
        ) if target_cpu == "alderlake" => 2,
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Intel,
                family: 6,
                model: 183 | 186 | 191,
                ..
            },
            TargetKind::X86IntelCore,
        ) if target_cpu == "raptorlake" => 2,
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Intel,
                family: 6,
                model: 170 | 172,
                ..
            },
            TargetKind::X86IntelCore,
        ) if target_cpu == "meteorlake" => 2,
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Intel,
                family: 6,
                model: 181 | 197,
                ..
            },
            TargetKind::X86IntelCore,
        ) if target_cpu == "arrowlake" => 2,
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Intel,
                family: 6,
                model: 198,
                ..
            },
            TargetKind::X86IntelCore,
        ) if target_cpu == "arrowlake-s" => 2,
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Intel,
                family: 6,
                model: 189,
                ..
            },
            TargetKind::X86IntelCore,
        ) if target_cpu == "lunarlake" => 2,
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Intel,
                family: 6,
                model: 204,
                ..
            },
            TargetKind::X86IntelCore,
        ) if target_cpu == "pantherlake" => 2,
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Intel,
                family: 6,
                model: 143,
                ..
            },
            TargetKind::X86IntelXeon,
        ) if target_cpu == "sapphirerapids" => 2,
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Intel,
                family: 6,
                model: 207,
                ..
            },
            TargetKind::X86IntelXeon,
        ) if target_cpu == "emeraldrapids" => 2,
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Intel,
                family: 6,
                model: 173,
                ..
            },
            TargetKind::X86IntelXeon,
        ) if target_cpu == "graniterapids" => 2,
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Intel,
                family: 6,
                model: 174,
                ..
            },
            TargetKind::X86IntelXeon,
        ) if target_cpu == "graniterapids-d" => 2,
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Intel,
                family: 6,
                model: 175,
                ..
            },
            TargetKind::X86IntelAtom,
        ) if target_cpu == "sierraforest" => 2,
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Intel,
                family: 6,
                model: 182,
                ..
            },
            TargetKind::X86IntelAtom,
        ) if target_cpu == "grandridge" => 2,
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Intel,
                family: 6,
                model: 221,
                ..
            },
            TargetKind::X86IntelAtom,
        ) if target_cpu == "clearwaterforest" => 2,
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Intel,
                family: 19,
                model: 1,
                ..
            },
            TargetKind::X86IntelXeon,
        ) if target_cpu == "diamondrapids" => 2,
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Amd,
                family: 15,
                ..
            },
            TargetKind::X86AmdOther,
        ) if target_cpu == "k8-sse3" => 2,
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Amd,
                family: 23,
                model,
                ..
            },
            TargetKind::X86AmdZen { generation: 1 },
        ) if (0x10..=0x2f).contains(&model) => 2,
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Amd,
                family: 23,
                model,
                ..
            },
            TargetKind::X86AmdZen { generation: 2 },
        ) if (0x30..=0x3f).contains(&model)
            || model == 0x47
            || (0x60..=0x7f).contains(&model)
            || (0x84..=0x87).contains(&model)
            || (0x90..=0xaf).contains(&model) =>
        {
            2
        }
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Amd,
                family: 25,
                model,
                ..
            },
            TargetKind::X86AmdZen { generation: 3 },
        ) if model <= 0x0f
            || (0x20..=0x2f).contains(&model)
            || (0x30..=0x3f).contains(&model)
            || (0x40..=0x4f).contains(&model)
            || (0x50..=0x5f).contains(&model) =>
        {
            2
        }
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Amd,
                family: 25,
                model,
                ..
            },
            TargetKind::X86AmdZen { generation: 4 },
        ) if (0x10..=0x1f).contains(&model)
            || (0x60..=0x6f).contains(&model)
            || (0x70..=0x7f).contains(&model)
            || (0xa0..=0xaf).contains(&model) =>
        {
            2
        }
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Amd,
                family: 26,
                model,
                ..
            },
            TargetKind::X86AmdZen { generation: 5 },
        ) if model <= 0x4f || (0x60..=0x77).contains(&model) || (0xd0..=0xd7).contains(&model) => 2,
        (
            CpuIdentity::Aarch64 {
                implementer: 0x41,
                part: 0xd03,
                ..
            },
            TargetKind::Aarch64ArmCortexA,
        ) if target_cpu == "cortex-a53" => 2,
        (
            CpuIdentity::Aarch64 {
                implementer: 0x41,
                part: 0xd04,
                ..
            },
            TargetKind::Aarch64ArmCortexA,
        ) if target_cpu == "cortex-a35" => 2,
        (
            CpuIdentity::Aarch64 {
                implementer: 0x41,
                part: 0xd05,
                ..
            },
            TargetKind::Aarch64ArmCortexA,
        ) if target_cpu == "cortex-a55" => 2,
        (
            CpuIdentity::Aarch64 {
                implementer: 0x41,
                part: 0xd07,
                ..
            },
            TargetKind::Aarch64ArmCortexA,
        ) if target_cpu == "cortex-a57" => 2,
        (
            CpuIdentity::Aarch64 {
                implementer: 0x41,
                part: 0xd08,
                ..
            },
            TargetKind::Aarch64ArmCortexA,
        ) if target_cpu == "cortex-a72" => 2,
        (
            CpuIdentity::Aarch64 {
                implementer: 0x41,
                part: 0xd0b,
                ..
            },
            TargetKind::Aarch64ArmCortexA,
        ) if target_cpu == "cortex-a76" => 2,
        (
            CpuIdentity::Aarch64 {
                implementer: 0x41,
                part: 0xd46,
                ..
            },
            TargetKind::Aarch64ArmCortexA,
        ) if target_cpu == "cortex-a510" => 2,
        (
            CpuIdentity::Aarch64 {
                implementer: 0x41,
                part: 0xd80,
                ..
            },
            TargetKind::Aarch64ArmCortexA,
        ) if target_cpu == "cortex-a520" => 2,
        (
            CpuIdentity::Aarch64 {
                implementer: 0x41,
                part: 0xd47,
                ..
            },
            TargetKind::Aarch64ArmCortexA,
        ) if target_cpu == "cortex-a710" => 2,
        (
            CpuIdentity::Aarch64 {
                implementer: 0x41,
                part: 0xd4d,
                ..
            },
            TargetKind::Aarch64ArmCortexA,
        ) if target_cpu == "cortex-a715" => 2,
        (
            CpuIdentity::Aarch64 {
                implementer: 0x41,
                part: 0xd81,
                ..
            },
            TargetKind::Aarch64ArmCortexA,
        ) if target_cpu == "cortex-a720" => 2,
        (
            CpuIdentity::Aarch64 {
                implementer: 0x41,
                part: 0xd87,
                ..
            },
            TargetKind::Aarch64ArmCortexA,
        ) if target_cpu == "cortex-a725" => 2,
        (
            CpuIdentity::Aarch64 {
                implementer: 0x41,
                part: 0xd4a,
                ..
            },
            TargetKind::Aarch64ArmNeoverseE,
        ) if target_cpu == "neoverse-e1" => 2,
        (
            CpuIdentity::Aarch64 {
                implementer: 0x41,
                part: 0xd40,
                ..
            },
            TargetKind::Aarch64ArmNeoverseV,
        ) if target_cpu == "neoverse-v1" => 2,
        (
            CpuIdentity::Aarch64 {
                implementer: 0x41,
                part: 0xd0c,
                ..
            },
            TargetKind::Aarch64ArmNeoverseN,
        ) if target_cpu == "neoverse-n1" => 2,
        (
            CpuIdentity::Aarch64 {
                implementer: 0x41,
                part: 0xd49,
                ..
            },
            TargetKind::Aarch64ArmNeoverseN,
        ) if target_cpu == "neoverse-n2" => 2,
        (
            CpuIdentity::Aarch64 {
                implementer: 0x41,
                part: 0xd8e,
                ..
            },
            TargetKind::Aarch64ArmNeoverseN,
        ) if target_cpu == "neoverse-n3" => 2,
        (
            CpuIdentity::Aarch64 {
                implementer: 0x41,
                part: 0xd4f,
                ..
            },
            TargetKind::Aarch64ArmNeoverseV,
        ) if target_cpu == "neoverse-v2" => 2,
        (
            CpuIdentity::Aarch64 {
                implementer: 0x41,
                part: 0xd84,
                ..
            },
            TargetKind::Aarch64ArmNeoverseV,
        ) if target_cpu == "neoverse-v3" => 2,
        (
            CpuIdentity::Aarch64 {
                implementer: 0x41,
                part: 0xd83,
                ..
            },
            TargetKind::Aarch64ArmNeoverseV,
        ) if target_cpu == "neoverse-v3ae" => 2,
        (
            CpuIdentity::Aarch64 {
                implementer: 0x46,
                part: 0x001,
                ..
            },
            TargetKind::Aarch64Other,
        ) if target_cpu == "a64fx" => 2,
        _ => 0,
    }
}

fn weak_affinity(identity: CpuIdentity, kind: TargetKind) -> u8 {
    match (identity, kind) {
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Amd,
                ..
            },
            TargetKind::X86AmdZen { .. } | TargetKind::X86AmdOther,
        ) => 1,
        (
            CpuIdentity::X86 {
                vendor: X86Vendor::Intel,
                ..
            },
            TargetKind::X86IntelCore | TargetKind::X86IntelXeon | TargetKind::X86IntelAtom,
        ) => 1,
        (
            CpuIdentity::Aarch64 {
                implementer: 0x41, ..
            },
            TargetKind::Aarch64ArmNeoverseN
            | TargetKind::Aarch64ArmNeoverseV
            | TargetKind::Aarch64ArmNeoverseE
            | TargetKind::Aarch64ArmCortexA
            | TargetKind::Aarch64ArmCortexX,
        ) => 1,
        (
            CpuIdentity::Aarch64 {
                implementer: 0xc0, ..
            },
            TargetKind::Aarch64Ampere,
        ) => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feature_mask::{Feature, FeatureMask, feature_by_name};
    use std::{boxed::Box, string::ToString, vec::Vec};

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

    fn parsed_v(name: &'static str, features: FeatureMask, kind: TargetKind) -> VariantMeta {
        VariantMeta {
            target_cpu: name,
            required_features: features,
            rank_features: features,
            rank_feature_count: features.count(),
            feature_tier: 5,
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

    fn leak(value: &str) -> &'static str {
        Box::leak(value.to_string().into_boxed_str())
    }

    fn parse_hex_u16(value: &str) -> u16 {
        u16::from_str_radix(value.trim_start_matches("0x"), 16).unwrap()
    }

    fn parsed_features(value: &str) -> FeatureMask {
        let mut mask = FeatureMask::EMPTY;
        for feature in value.split(',').filter(|feature| !feature.is_empty()) {
            let feature = feature_by_name(feature)
                .unwrap_or_else(|| panic!("fixture uses unknown feature `{feature}`"));
            mask.insert(feature);
        }
        mask
    }

    #[derive(Clone, Copy)]
    struct X86Fixture {
        name: &'static str,
        vendor: X86Vendor,
        family: u16,
        model: u16,
        expected: &'static str,
        features: FeatureMask,
    }

    fn x86_fixture_rows() -> Vec<X86Fixture> {
        include_str!("../../../tests/cpu-fixtures/x86_64-modern.tsv")
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| {
                let mut columns = line.split('\t');
                let name = leak(columns.next().unwrap());
                let vendor = match columns.next().unwrap() {
                    "Intel" => X86Vendor::Intel,
                    "AMD" => X86Vendor::Amd,
                    other => panic!("unknown x86 fixture vendor `{other}`"),
                };
                let family = parse_hex_u16(columns.next().unwrap());
                let model = parse_hex_u16(columns.next().unwrap());
                let expected = leak(columns.next().unwrap());
                let features = parsed_features(columns.next().unwrap());
                assert!(columns.next().is_none(), "too many columns in `{line}`");
                X86Fixture {
                    name,
                    vendor,
                    family,
                    model,
                    expected,
                    features,
                }
            })
            .collect()
    }

    fn x86_fixture_kind(cpu: &str) -> TargetKind {
        if cpu.starts_with("znver") {
            return TargetKind::X86AmdZen {
                generation: cpu.trim_start_matches("znver").parse().unwrap(),
            };
        }
        match cpu {
            "sierraforest" | "clearwaterforest" | "grandridge" => TargetKind::X86IntelAtom,
            "sapphirerapids" | "emeraldrapids" | "graniterapids" | "graniterapids-d"
            | "diamondrapids" => TargetKind::X86IntelXeon,
            _ => TargetKind::X86IntelCore,
        }
    }

    #[derive(Clone, Copy)]
    struct Aarch64Fixture {
        name: &'static str,
        implementer: u16,
        part: u16,
        expected: &'static str,
        features: FeatureMask,
    }

    fn aarch64_fixture_rows() -> Vec<Aarch64Fixture> {
        include_str!("../../../tests/cpu-fixtures/aarch64-modern.tsv")
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| {
                let mut columns = line.split('\t');
                let name = leak(columns.next().unwrap());
                let implementer = parse_hex_u16(columns.next().unwrap());
                let part = parse_hex_u16(columns.next().unwrap());
                let expected = leak(columns.next().unwrap());
                let features = parsed_features(columns.next().unwrap());
                assert!(columns.next().is_none(), "too many columns in `{line}`");
                Aarch64Fixture {
                    name,
                    implementer,
                    part,
                    expected,
                    features,
                }
            })
            .collect()
    }

    fn aarch64_fixture_kind(cpu: &str) -> TargetKind {
        match cpu {
            "a64fx" => TargetKind::Aarch64Other,
            cpu if cpu.starts_with("cortex-a") => TargetKind::Aarch64ArmCortexA,
            cpu if cpu.starts_with("neoverse-e") => TargetKind::Aarch64ArmNeoverseE,
            cpu if cpu.starts_with("neoverse-n") => TargetKind::Aarch64ArmNeoverseN,
            cpu if cpu.starts_with("neoverse-v") => TargetKind::Aarch64ArmNeoverseV,
            other => panic!("unknown aarch64 fixture target cpu `{other}`"),
        }
    }

    #[test]
    fn generic_is_always_eligible() {
        let variants = [v("generic", &[], 0, TargetKind::Generic)];
        assert_eq!(
            select_variant(host(&[], CpuIdentity::Unknown), &variants).target_cpu,
            "generic"
        );
    }

    #[test]
    fn fixture_modern_x86_identity_uses_llvm_host_cpu_affinity() {
        let rows = x86_fixture_rows();
        let variants: Vec<_> = rows
            .iter()
            .map(|row| parsed_v(row.expected, row.features, x86_fixture_kind(row.expected)))
            .collect();

        for row in rows {
            let host = HostInfo {
                arch: TargetArch::X86_64,
                features: row.features,
                identity: CpuIdentity::X86 {
                    vendor: row.vendor,
                    family: row.family,
                    model: row.model,
                    stepping: 0,
                },
                heterogeneous: false,
            };
            assert_eq!(
                select_variant(host, &variants).target_cpu,
                row.expected,
                "{}",
                row.name
            );
        }
    }

    #[test]
    fn fixture_modern_aarch64_midr_uses_llvm_host_cpu_affinity() {
        let rows = aarch64_fixture_rows();
        let variants: Vec<_> = rows
            .iter()
            .map(|row| {
                parsed_v(
                    row.expected,
                    row.features,
                    aarch64_fixture_kind(row.expected),
                )
            })
            .collect();

        for row in rows {
            let host = HostInfo {
                arch: TargetArch::Aarch64,
                features: row.features,
                identity: CpuIdentity::Aarch64 {
                    implementer: row.implementer,
                    part: row.part,
                    variant: 0,
                    revision: 0,
                },
                heterogeneous: false,
            };
            assert_eq!(
                select_variant(host, &variants).target_cpu,
                row.expected,
                "{}",
                row.name
            );
        }
    }

    #[test]
    fn non_generic_requires_feature_subset() {
        let variants = [
            v("generic", &[], 0, TargetKind::Generic),
            v("haswell", &[Feature::Avx2], 3, TargetKind::X86IntelCore),
        ];
        assert_eq!(
            select_variant(host(&[], CpuIdentity::Unknown), &variants).target_cpu,
            "generic"
        );
    }

    #[test]
    fn selects_larger_feature_set_when_no_affinity() {
        let variants = [
            v("generic", &[], 0, TargetKind::Generic),
            v(
                "haswell",
                &[Feature::Avx2, Feature::Fma, Feature::Bmi1],
                3,
                TargetKind::X86IntelCore,
            ),
        ];
        assert_eq!(
            select_variant(
                host(
                    &[Feature::Avx2, Feature::Fma, Feature::Bmi1],
                    CpuIdentity::Unknown
                ),
                &variants
            )
            .target_cpu,
            "haswell"
        );
    }

    #[test]
    fn exact_affinity_beats_higher_feature_count() {
        let variants = [
            v(
                "diamondrapids",
                &[Feature::Avx512F, Feature::Avx512Bw, Feature::Avx512Dq],
                5,
                TargetKind::X86IntelXeon,
            ),
            v(
                "znver5",
                &[Feature::Avx512F],
                4,
                TargetKind::X86AmdZen { generation: 5 },
            ),
        ];
        let identity = CpuIdentity::X86 {
            vendor: X86Vendor::Amd,
            family: 26,
            model: 1,
            stepping: 0,
        };
        assert_eq!(
            select_variant(
                host(
                    &[Feature::Avx512F, Feature::Avx512Bw, Feature::Avx512Dq],
                    identity
                ),
                &variants
            )
            .target_cpu,
            "znver5"
        );
    }

    #[test]
    fn neutral_x86_level_beats_vendor_specific_when_identity_unknown() {
        let variants = [
            v(
                "sapphirerapids",
                &[Feature::Avx512F],
                4,
                TargetKind::X86IntelXeon,
            ),
            v(
                "x86-64-v4",
                &[Feature::Avx512F],
                4,
                TargetKind::X86NeutralLevel { level: 4 },
            ),
            v(
                "znver4",
                &[Feature::Avx512F],
                4,
                TargetKind::X86AmdZen { generation: 4 },
            ),
        ];
        assert_eq!(
            select_variant(host(&[Feature::Avx512F], CpuIdentity::Unknown), &variants).target_cpu,
            "x86-64-v4"
        );
    }

    #[test]
    fn same_line_affinity_beats_neutral_when_identity_known() {
        let variants = [
            v(
                "x86-64-v4",
                &[Feature::Avx512F],
                4,
                TargetKind::X86NeutralLevel { level: 4 },
            ),
            v(
                "znver4",
                &[Feature::Avx512F],
                4,
                TargetKind::X86AmdZen { generation: 4 },
            ),
        ];
        let identity = CpuIdentity::X86 {
            vendor: X86Vendor::Amd,
            family: 25,
            model: 0x11,
            stepping: 0,
        };
        assert_eq!(
            select_variant(host(&[Feature::Avx512F], identity), &variants).target_cpu,
            "znver4"
        );
    }

    #[test]
    fn same_feature_collision_uses_exact_affinity() {
        let variants = [
            v(
                "neoverse-512tvb",
                &[Feature::Sve],
                4,
                TargetKind::Aarch64ArmNeoverseV,
            ),
            v(
                "neoverse-v1",
                &[Feature::Sve],
                4,
                TargetKind::Aarch64ArmNeoverseV,
            ),
        ];
        let host = HostInfo {
            arch: TargetArch::Aarch64,
            features: mask(&[Feature::Sve]),
            identity: CpuIdentity::Aarch64 {
                implementer: 0x41,
                part: 0xd40,
                variant: 0,
                revision: 0,
            },
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
        assert_eq!(
            select_variant(host(&[], CpuIdentity::Unknown), &variants).target_cpu,
            "generic"
        );
    }

    #[test]
    fn same_feature_collision_falls_back_to_lexical_order() {
        let variants = [
            v(
                "skylake_avx512",
                &[Feature::Avx512F],
                4,
                TargetKind::X86IntelXeon,
            ),
            v(
                "skylake-avx512",
                &[Feature::Avx512F],
                4,
                TargetKind::X86IntelXeon,
            ),
        ];
        assert_eq!(
            select_variant(host(&[Feature::Avx512F], CpuIdentity::Unknown), &variants).target_cpu,
            "skylake-avx512"
        );
    }

    #[test]
    fn feature_tier_beats_raw_feature_count_when_no_exact_affinity() {
        let variants = [
            v(
                "many-small",
                &[
                    Feature::Sse3,
                    Feature::Ssse3,
                    Feature::Sse4_1,
                    Feature::Sse4_2,
                ],
                1,
                TargetKind::X86IntelCore,
            ),
            v(
                "avx2",
                &[Feature::Avx2],
                3,
                TargetKind::X86NeutralLevel { level: 3 },
            ),
        ];
        assert_eq!(
            select_variant(
                host(
                    &[
                        Feature::Sse3,
                        Feature::Ssse3,
                        Feature::Sse4_1,
                        Feature::Sse4_2,
                        Feature::Avx2
                    ],
                    CpuIdentity::Unknown
                ),
                &variants
            )
            .target_cpu,
            "avx2"
        );
    }

    #[test]
    fn heterogeneous_x86_downgrades_core_specific_target() {
        let variants = [
            v("gracemont", &[Feature::Sse4_2], 1, TargetKind::X86IntelAtom),
            v(
                "x86-64-v2",
                &[Feature::Sse4_2],
                1,
                TargetKind::X86NeutralLevel { level: 2 },
            ),
        ];
        let mut h = host(
            &[Feature::Sse4_2],
            CpuIdentity::X86 {
                vendor: X86Vendor::Intel,
                family: 6,
                model: 0,
                stepping: 0,
            },
        );
        h.heterogeneous = true;
        assert_eq!(select_variant(h, &variants).target_cpu, "x86-64-v2");
    }

    #[test]
    fn exact_raptorlake_affinity_selects_raptorlake() {
        let variants = [
            v(
                "x86-64-v3",
                &[Feature::Avx2],
                3,
                TargetKind::X86NeutralLevel { level: 3 },
            ),
            v("raptorlake", &[Feature::Avx2], 3, TargetKind::X86IntelCore),
        ];
        let identity = CpuIdentity::X86 {
            vendor: X86Vendor::Intel,
            family: 6,
            model: 183,
            stepping: 1,
        };
        assert_eq!(
            select_variant(host(&[Feature::Avx2], identity), &variants).target_cpu,
            "raptorlake"
        );
    }

    #[test]
    fn exact_nocona_affinity_selects_nocona_over_k8_sse3() {
        let variants = [
            v("k8-sse3", &[Feature::Sse3], 1, TargetKind::X86AmdOther),
            v("nocona", &[Feature::Sse3], 1, TargetKind::X86IntelCore),
        ];
        let identity = CpuIdentity::X86 {
            vendor: X86Vendor::Intel,
            family: 15,
            model: 6,
            stepping: 1,
        };
        assert_eq!(
            select_variant(host(&[Feature::Sse3], identity), &variants).target_cpu,
            "nocona"
        );
    }

    #[test]
    fn exact_amd_k8_affinity_selects_k8_sse3_over_athlon64_sse3() {
        let variants = [
            v(
                "athlon64-sse3",
                &[Feature::Sse3],
                1,
                TargetKind::X86AmdOther,
            ),
            v("k8-sse3", &[Feature::Sse3], 1, TargetKind::X86AmdOther),
        ];
        let identity = CpuIdentity::X86 {
            vendor: X86Vendor::Amd,
            family: 15,
            model: 6,
            stepping: 1,
        };

        assert_eq!(
            select_variant(host(&[Feature::Sse3], identity), &variants).target_cpu,
            "k8-sse3"
        );
    }

    #[test]
    fn exact_aarch64_affinity_beats_same_feature_count() {
        let variants = [
            v(
                "cortex-a78ae",
                &[Feature::Crc, Feature::Dotprod],
                2,
                TargetKind::Aarch64ArmCortexA,
            ),
            v(
                "cortex-a55",
                &[Feature::Crc, Feature::Dotprod],
                2,
                TargetKind::Aarch64ArmCortexA,
            ),
            v(
                "neoverse-n1",
                &[Feature::Crc, Feature::Dotprod],
                2,
                TargetKind::Aarch64ArmNeoverseN,
            ),
        ];
        let h = HostInfo {
            arch: TargetArch::Aarch64,
            features: mask(&[Feature::Crc, Feature::Dotprod]),
            identity: CpuIdentity::Aarch64 {
                implementer: 0x41,
                part: 0xd05,
                variant: 2,
                revision: 0,
            },
            heterogeneous: false,
        };
        assert_eq!(select_variant(h, &variants).target_cpu, "cortex-a55");
    }

    #[test]
    fn exact_neoverse_n2_affinity_beats_cortex_a710_collision() {
        let variants = [
            v(
                "cortex-a710",
                &[Feature::Sve2],
                5,
                TargetKind::Aarch64ArmCortexA,
            ),
            v(
                "neoverse-n2",
                &[Feature::Sve2],
                5,
                TargetKind::Aarch64ArmNeoverseN,
            ),
        ];
        let h = HostInfo {
            arch: TargetArch::Aarch64,
            features: mask(&[Feature::Sve2]),
            identity: CpuIdentity::Aarch64 {
                implementer: 0x41,
                part: 0xd49,
                variant: 0,
                revision: 3,
            },
            heterogeneous: false,
        };
        assert_eq!(select_variant(h, &variants).target_cpu, "neoverse-n2");
    }

    #[test]
    fn heterogeneous_aarch64_downgrades_single_core_target() {
        let variants = [
            v(
                "cortex-a76",
                &[Feature::Crc],
                1,
                TargetKind::Aarch64ArmCortexA,
            ),
            v("generic", &[], 0, TargetKind::Generic),
        ];
        let h = HostInfo {
            arch: TargetArch::Aarch64,
            features: mask(&[Feature::Crc]),
            identity: CpuIdentity::Aarch64 {
                implementer: 0x41,
                part: 0xd0b,
                variant: 0,
                revision: 0,
            },
            heterogeneous: true,
        };
        assert_eq!(select_variant(h, &variants).target_cpu, "generic");
    }
}
