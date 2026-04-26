use sonic_loader::feature_mask::feature_name;
use sonic_loader::{select_variant, CpuIdentity, FeatureMask, HostInfo, TargetArch, TargetKind, VariantMeta};

fn main() {
    let generic = VariantMeta {
        target_cpu: "generic",
        required_features: FeatureMask::EMPTY,
        rank_features: FeatureMask::EMPTY,
        rank_feature_count: 0,
        feature_tier: 0,
        target_kind: TargetKind::Generic,
    };
    let host = HostInfo {
        arch: if cfg!(target_arch = "aarch64") { TargetArch::Aarch64 } else { TargetArch::X86_64 },
        features: FeatureMask::EMPTY,
        identity: CpuIdentity::Unknown,
        heterogeneous: false,
    };
    let variants = [generic];
    let selected = select_variant(host, &variants);
    println!(
        "{{\"arch\":\"{}\",\"detected_features\":[],\"identity\":{{\"unknown\":true}},\"configured_variants\":[\"generic\"],\"eligible\":[\"generic\"],\"selected_target_cpu\":\"{}\",\"selected_flags\":[]}}",
        if cfg!(target_arch = "aarch64") { "aarch64" } else { "x86_64" },
        selected.target_cpu
    );
    let _ = feature_name;
}
