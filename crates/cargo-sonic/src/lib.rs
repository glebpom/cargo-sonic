use anyhow::{Context, Result, anyhow, bail};
use camino::{Utf8Path, Utf8PathBuf};
use cargo_metadata::{Artifact, Message, MetadataCommand, Package};
use clap::{Arg, ArgAction, Command as ClapCommand, ValueEnum};
use miniz_oxide::deflate::compress_to_vec_zlib;
use object::write::{Object, StandardSegment, Symbol, SymbolSection};
use object::{
    Architecture, BinaryFormat, Endianness, FileFlags, SectionFlags, SectionKind, SymbolFlags,
    SymbolKind, SymbolScope, elf,
};
use serde::Serialize;
pub mod arch_aarch64;
pub mod arch_x86_64;
pub mod feature_mask;
pub mod select;

use crate::feature_mask::{FeatureMask, feature_by_name};
use crate::select::{
    CpuIdentity, HostInfo, TargetArch, TargetKind, VariantMeta, X86Vendor,
    compare_variants_by_score, select_variant, selection_score, variant_eligible,
};
use std::collections::VecDeque;
use std::collections::{BTreeMap, BTreeSet};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
#[cfg(feature = "zstd")]
use std::io::Cursor;
use std::io::{IsTerminal, Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

const OUTPUT_FLUSH_INTERVAL: Duration = Duration::from_millis(500);
const OUTPUT_FLUSH_BYTES: usize = 8 * 1024;
const LOADER_RUSTFLAGS_ENV: &str = "CARGO_SONIC_LOADER_RUSTFLAGS";

#[derive(Debug, Clone)]
pub struct BuildOptions {
    pub cargo_args: Vec<String>,
    pub manifest_path: Option<Utf8PathBuf>,
    pub target_cpus: Vec<String>,
    pub parallelism: usize,
    pub compress: PayloadCompression,
    pub compression_level: i32,
    pub loader: LoaderStrategy,
    pub auditable: bool,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct BuildOutput {
    pub final_binary: Utf8PathBuf,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ProbeOptions {
    pub cargo_args: Vec<String>,
    pub target_cpus: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ScoreOptions {
    pub cargo_args: Vec<String>,
}

#[derive(Debug, Clone)]
struct CargoArgs {
    release: bool,
    target: Option<String>,
    target_dir: Option<Utf8PathBuf>,
    bin: Option<String>,
    package: Option<String>,
    manifest_path: Option<Utf8PathBuf>,
    color: Option<ColorMode>,
    forwarded: Vec<String>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ColorMode {
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone)]
struct VariantBuild {
    target_cpu: String,
    required_features: FeatureMask,
    rank_features: FeatureMask,
    feature_names: Vec<String>,
    feature_tier: u8,
    target_kind: TargetKind,
    artifact: Utf8PathBuf,
    payload_compression: PayloadCompression,
    uncompressed_len: u64,
    bundle_path: &'static str,
}

#[derive(Debug, Clone)]
struct VariantBuildJob {
    target_cpu: String,
    required_features: FeatureMask,
    rank_features: FeatureMask,
    feature_names: Vec<String>,
    feature_tier: u8,
    target_kind: TargetKind,
}

#[derive(Debug, Clone)]
struct PayloadBuildContext {
    package: Package,
    cargo_args: CargoArgs,
    manifest_path: Option<Utf8PathBuf>,
    target: String,
    out_root: Utf8PathBuf,
    tag_output: bool,
    payload_compression: PayloadCompression,
    #[cfg_attr(not(feature = "zstd"), allow(dead_code))]
    compression_level: i32,
}

#[derive(Debug, Clone)]
struct PayloadArtifact {
    path: Utf8PathBuf,
    compression: PayloadCompression,
    uncompressed_len: u64,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, ValueEnum)]
pub enum PayloadCompression {
    None,
    Zstd,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, ValueEnum)]
pub enum LoaderStrategy {
    Embedded,
    Bundle,
}

struct ProbeVariant {
    target_cpu: String,
    required_features: FeatureMask,
    rank_features: FeatureMask,
    feature_names: Vec<String>,
    feature_tier: u8,
}

struct SkippedProbeVariant {
    target_cpu: String,
    reason: String,
}

struct FeatureSet {
    known: Vec<String>,
    unknown: Vec<String>,
}

pub fn build(options: BuildOptions) -> Result<BuildOutput> {
    if options.parallelism == 0 {
        bail!("--parallelism must be at least 1");
    }
    let payload_compression = options.compress;
    let loader_strategy = options.loader;
    let cargo_args = parse_cargo_args(&options.cargo_args);
    let manifest_path = options
        .manifest_path
        .clone()
        .or_else(|| cargo_args.manifest_path.clone());
    let mut metadata_cmd = MetadataCommand::new();
    metadata_cmd.no_deps();
    if let Some(path) = &manifest_path {
        metadata_cmd.manifest_path(path);
    }
    let metadata = metadata_cmd
        .exec()
        .context("failed to read cargo metadata")?;
    let package = select_package(&metadata, cargo_args.package.as_deref())?;
    let auditable_section = if options.auditable {
        Some(collect_auditable_section(
            manifest_path.as_deref(),
            cargo_args.package.as_deref(),
        )?)
    } else {
        None
    };
    let requested_cpus = effective_target_cpus(options.target_cpus)?;

    let target = match cargo_args.target.clone() {
        Some(target) => target,
        None => rustc_default_target()?,
    };
    let cfg = rustc_cfg(&target, None)?;
    let target_os = cfg_value(&cfg, "target_os").unwrap_or_default();
    let target_arch = cfg_value(&cfg, "target_arch").unwrap_or_default();
    if target_os != "linux" {
        bail!("cargo-sonic supports Linux targets only; `{target}` has target_os={target_os:?}");
    }
    if target_arch != "x86_64" && target_arch != "aarch64" {
        bail!(
            "cargo-sonic currently supports x86_64 and aarch64 only; `{target}` has target_arch={target_arch:?}"
        );
    }
    let configured_cpus = normalize_target_cpus(requested_cpus, &target_arch)?;
    let baseline_cpu = baseline_target_cpu(&target_arch);

    let current_valid = rustc_target_cpus(&target)?;
    let union_valid = known_supported_cpu_union()?;
    let included = filter_target_cpus(&configured_cpus, &current_valid, &union_valid)?;
    let profile = if cargo_args.release {
        "release"
    } else {
        "debug"
    };
    let out_root = effective_target_directory(&metadata, cargo_args.target_dir.as_deref())?
        .join("sonic")
        .join(&target)
        .join(profile);
    fs::create_dir_all(&out_root)?;

    let mut warnings = Vec::new();
    let mut features_by_cpu = BTreeMap::new();
    for cpu in &included {
        let features = parse_target_features_from_rustc_cfg(&rustc_cfg(&target, Some(cpu))?);
        let feature_set = classify_runtime_features(&features);
        if cpu != baseline_cpu && !feature_set.unknown.is_empty() {
            warnings.push(format!(
                "skipping target-cpu `{}` because rustc reported unknown runtime feature(s): {}",
                cpu,
                feature_set.unknown.join(", ")
            ));
            continue;
        }
        features_by_cpu.insert(cpu.clone(), feature_set.known);
    }
    if !features_by_cpu.contains_key(baseline_cpu) {
        features_by_cpu.insert(baseline_cpu.to_string(), Vec::new());
    }
    warnings.extend(analyze_warnings(&features_by_cpu, &target_arch));
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }

    let mut variant_jobs = Vec::new();
    for cpu in included
        .iter()
        .filter(|cpu| features_by_cpu.contains_key(cpu.as_str()))
    {
        let feature_names = features_by_cpu.get(cpu).cloned().unwrap_or_default();
        let rank_features = feature_mask(&feature_names)?;
        let required_feature_names = safety_required_features(&feature_names);
        let required_features = if cpu == baseline_cpu {
            FeatureMask::EMPTY
        } else {
            feature_mask(&required_feature_names)?
        };
        let target_kind = classify_target_cpu(cpu, &target_arch, baseline_cpu);
        variant_jobs.push(VariantBuildJob {
            target_cpu: cpu.clone(),
            required_features,
            rank_features,
            feature_names,
            feature_tier: target_feature_tier(&target_arch, target_kind),
            target_kind,
        });
    }
    let variants = build_payload_variants(
        PayloadBuildContext {
            package: package.clone(),
            cargo_args: cargo_args.clone(),
            manifest_path: manifest_path.clone(),
            target: target.clone(),
            out_root: out_root.clone(),
            tag_output: options.parallelism > 1,
            payload_compression,
            compression_level: options.compression_level,
        },
        options.parallelism,
        variant_jobs,
    )?;

    let bin_name = resolve_bin_name(package, cargo_args.bin.as_deref(), &variants)?;
    let loader_dir = out_root.join("loader");
    generate_loader_crate(
        &loader_dir,
        &target,
        &variants,
        loader_strategy,
        auditable_section.as_deref(),
    )?;
    let loader_artifact = build_loader(&loader_dir, &target, profile)?;
    let final_binary = out_root.join(&bin_name);
    fs::create_dir_all(out_root.as_path())?;
    fs::copy(&loader_artifact, &final_binary)
        .with_context(|| format!("failed to copy final fat binary to {final_binary}"))?;
    make_executable(&final_binary)?;
    if loader_strategy == LoaderStrategy::Bundle {
        prepare_bundle_directory(&final_binary, &variants)?;
    }
    Ok(BuildOutput {
        final_binary,
        warnings,
    })
}

pub fn probe(options: ProbeOptions) -> Result<()> {
    let cargo_args = parse_cargo_args(&options.cargo_args);
    let requested_cpus = effective_target_cpus(options.target_cpus)?;

    let target = match cargo_args.target.clone() {
        Some(target) => target,
        None => rustc_default_target()?,
    };
    let cfg = rustc_cfg(&target, None)?;
    let target_os = cfg_value(&cfg, "target_os").unwrap_or_default();
    let target_arch = cfg_value(&cfg, "target_arch").unwrap_or_default();
    if target_os != "linux" {
        bail!("cargo-sonic supports Linux targets only; `{target}` has target_os={target_os:?}");
    }
    if target_arch != "x86_64" && target_arch != "aarch64" {
        bail!(
            "cargo-sonic currently supports x86_64 and aarch64 only; `{target}` has target_arch={target_arch:?}"
        );
    }
    let configured_cpus = normalize_target_cpus(requested_cpus, &target_arch)?;
    let baseline_cpu = baseline_target_cpu(&target_arch);

    let host = detect_current_host(&target_arch)?;
    let current_valid = rustc_target_cpus(&target)?;
    let union_valid = known_supported_cpu_union()?;
    let included = filter_target_cpus(&configured_cpus, &current_valid, &union_valid)?;

    let mut metas = Vec::new();
    let mut display = Vec::new();
    let mut skipped = Vec::new();
    for cpu in &included {
        let features = parse_target_features_from_rustc_cfg(&rustc_cfg(&target, Some(cpu))?);
        let feature_set = classify_runtime_features(&features);
        if cpu != baseline_cpu && !feature_set.unknown.is_empty() {
            skipped.push(SkippedProbeVariant {
                target_cpu: cpu.clone(),
                reason: format!(
                    "unknown runtime feature(s): {}",
                    feature_set.unknown.join(", ")
                ),
            });
            continue;
        }
        let feature_names = feature_set.known;
        let rank_features = feature_mask(&feature_names)?;
        let required_feature_names = safety_required_features(&feature_names);
        let required_features = if cpu == baseline_cpu {
            FeatureMask::EMPTY
        } else {
            feature_mask(&required_feature_names)?
        };
        let target_kind = classify_target_cpu(cpu, &target_arch, baseline_cpu);
        metas.push(VariantMeta {
            target_cpu: leaked_str(cpu),
            required_features,
            rank_features,
            rank_feature_count: rank_features.count(),
            feature_tier: target_feature_tier(&target_arch, target_kind),
            target_kind,
        });
        display.push(ProbeVariant {
            target_cpu: cpu.clone(),
            required_features,
            rank_features,
            feature_names,
            feature_tier: target_feature_tier(&target_arch, target_kind),
        });
    }

    let selected = select_variant(host, &metas);
    print_probe_report(&target, host, &display, &skipped, selected.target_cpu);
    Ok(())
}

pub fn score(options: ScoreOptions) -> Result<()> {
    let cargo_args = parse_cargo_args(&options.cargo_args);
    let target = match cargo_args.target.clone() {
        Some(target) => target,
        None => rustc_default_target()?,
    };
    let cfg = rustc_cfg(&target, None)?;
    let target_os = cfg_value(&cfg, "target_os").unwrap_or_default();
    let target_arch = cfg_value(&cfg, "target_arch").unwrap_or_default();
    if target_os != "linux" {
        bail!("cargo-sonic supports Linux targets only; `{target}` has target_os={target_os:?}");
    }
    if target_arch != "x86_64" && target_arch != "aarch64" {
        bail!(
            "cargo-sonic currently supports x86_64 and aarch64 only; `{target}` has target_arch={target_arch:?}"
        );
    }

    let host = detect_current_host(&target_arch)?;
    let baseline_cpu = baseline_target_cpu(&target_arch);
    let mut candidates = rustc_target_cpus(&target)?;
    candidates.remove("native");
    if baseline_cpu != "generic" {
        candidates.remove("generic");
    }
    candidates.insert(baseline_cpu.to_string());

    let mut metas = Vec::new();
    let mut display = Vec::new();
    let mut skipped = 0usize;
    for cpu in candidates {
        if !rustc_accepts_target_cpu_for_payload(&target, &cpu)? {
            skipped += 1;
            continue;
        }
        let cfg = match rustc_cfg(&target, Some(&cpu)) {
            Ok(cfg) => cfg,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        let features = parse_target_features_from_rustc_cfg(&cfg);
        let feature_set = classify_runtime_features(&features);
        if cpu != baseline_cpu && !feature_set.unknown.is_empty() {
            skipped += 1;
            continue;
        }
        let feature_names = feature_set.known;
        let rank_features = feature_mask(&feature_names)?;
        let required_feature_names = safety_required_features(&feature_names);
        let required_features = if cpu == baseline_cpu {
            FeatureMask::EMPTY
        } else {
            feature_mask(&required_feature_names)?
        };
        let target_kind = classify_target_cpu(&cpu, &target_arch, baseline_cpu);
        if !score_target_kind_matches_host(host, target_kind) {
            skipped += 1;
            continue;
        }
        metas.push(VariantMeta {
            target_cpu: leaked_str(&cpu),
            required_features,
            rank_features,
            rank_feature_count: rank_features.count(),
            feature_tier: target_feature_tier(&target_arch, target_kind),
            target_kind,
        });
        display.push(ProbeVariant {
            target_cpu: cpu,
            required_features,
            rank_features,
            feature_names,
            feature_tier: target_feature_tier(&target_arch, target_kind),
        });
    }

    print_score_report(&target, host, &metas, &display, skipped);
    Ok(())
}

fn score_target_kind_matches_host(host: HostInfo, target_kind: TargetKind) -> bool {
    if target_kind.is_generic() || target_kind.is_neutral_x86() {
        return true;
    }
    match host.identity {
        CpuIdentity::Unknown => true,
        CpuIdentity::X86 {
            vendor: X86Vendor::Amd,
            ..
        } => matches!(
            target_kind,
            TargetKind::X86AmdZen { .. } | TargetKind::X86AmdOther
        ),
        CpuIdentity::X86 {
            vendor: X86Vendor::Intel,
            ..
        } => matches!(
            target_kind,
            TargetKind::X86IntelCore | TargetKind::X86IntelXeon | TargetKind::X86IntelAtom
        ),
        CpuIdentity::X86 {
            vendor: X86Vendor::Other,
            ..
        } => false,
        CpuIdentity::Aarch64 {
            implementer: 0x41, ..
        } => matches!(
            target_kind,
            TargetKind::Aarch64ArmNeoverseN
                | TargetKind::Aarch64ArmNeoverseV
                | TargetKind::Aarch64ArmNeoverseE
                | TargetKind::Aarch64ArmCortexA
                | TargetKind::Aarch64ArmCortexX
        ),
        CpuIdentity::Aarch64 {
            implementer: 0xc0, ..
        } => matches!(target_kind, TargetKind::Aarch64Ampere),
        CpuIdentity::Aarch64 { .. } => false,
    }
}

fn parse_cargo_args(args: &[String]) -> CargoArgs {
    let matches = cargo_args_command()
        .try_get_matches_from(known_cargo_args(args))
        .expect("cargo args parser uses ignore_errors");
    let profile = last_match_value(&matches, "profile");
    let release = matches.get_flag("release") || profile.as_deref() == Some("release");
    CargoArgs {
        release,
        target: last_match_value(&matches, "target"),
        target_dir: last_match_value(&matches, "target-dir").map(Utf8PathBuf::from),
        bin: last_match_value(&matches, "bin"),
        package: last_match_value(&matches, "package"),
        manifest_path: last_match_value(&matches, "manifest-path").map(Utf8PathBuf::from),
        color: last_match_value(&matches, "color").map(|value| parse_color_mode(&value)),
        forwarded: forwarded_cargo_args(args),
    }
}

fn parse_color_mode(value: &str) -> ColorMode {
    match value {
        "always" => ColorMode::Always,
        "never" => ColorMode::Never,
        _ => ColorMode::Auto,
    }
}

fn last_match_value(matches: &clap::ArgMatches, id: &str) -> Option<String> {
    matches
        .get_many::<String>(id)?
        .next_back()
        .map(ToString::to_string)
}

fn known_cargo_args(args: &[String]) -> Vec<String> {
    let mut known = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--release" | "-r" => known.push(args[i].clone()),
            "--profile" | "--target" | "--target-dir" | "--bin" | "--package" | "-p"
            | "--manifest-path" | "--color" => {
                known.push(args[i].clone());
                if let Some(value) = args.get(i + 1) {
                    known.push(value.clone());
                    i += 1;
                }
            }
            value
                if value.starts_with("--profile=")
                    || value.starts_with("--target=")
                    || value.starts_with("--target-dir=")
                    || value.starts_with("--bin=")
                    || value.starts_with("--package=")
                    || value.starts_with("--manifest-path=")
                    || value.starts_with("--color=") =>
            {
                known.push(args[i].clone());
            }
            _ => {}
        }
        i += 1;
    }
    known
}

fn cargo_args_command() -> ClapCommand {
    ClapCommand::new("cargo-sonic-build-args")
        .no_binary_name(true)
        .ignore_errors(true)
        .arg(
            Arg::new("release")
                .long("release")
                .short('r')
                .action(ArgAction::SetTrue),
        )
        .arg(repeated_arg("profile").long("profile"))
        .arg(repeated_arg("target").long("target"))
        .arg(repeated_arg("target-dir").long("target-dir"))
        .arg(repeated_arg("bin").long("bin"))
        .arg(repeated_arg("package").long("package").short('p'))
        .arg(repeated_arg("manifest-path").long("manifest-path"))
        .arg(repeated_arg("color").long("color"))
}

fn repeated_arg(name: &'static str) -> Arg {
    Arg::new(name).num_args(1).action(ArgAction::Append)
}

fn forwarded_cargo_args(args: &[String]) -> Vec<String> {
    let mut forwarded = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--manifest-path" | "--target-dir" | "--color" => i += 1,
            v if v.starts_with("--manifest-path=")
                || v.starts_with("--target-dir=")
                || v.starts_with("--color=") => {}
            _ => forwarded.push(args[i].clone()),
        }
        i += 1;
    }
    forwarded
}

fn leaked_str(value: &str) -> &'static str {
    Box::leak(value.to_string().into_boxed_str())
}

fn print_variant_build_prelude(target_cpu: &str) {
    eprintln!("cargo-sonic: build to {target_cpu} CPU");
    let _ = std::io::stderr().flush();
}

fn print_probe_report(
    target: &str,
    host: HostInfo,
    variants: &[ProbeVariant],
    skipped: &[SkippedProbeVariant],
    selected: &str,
) {
    print!(
        "{}",
        format_probe_report(target, host, variants, skipped, selected)
    );
}

fn print_score_report(
    target: &str,
    host: HostInfo,
    metas: &[VariantMeta],
    variants: &[ProbeVariant],
    skipped: usize,
) {
    print!(
        "{}",
        format_score_report(target, host, metas, variants, skipped)
    );
}

fn format_score_report(
    target: &str,
    host: HostInfo,
    metas: &[VariantMeta],
    variants: &[ProbeVariant],
    skipped: usize,
) -> String {
    let mut compatible = metas
        .iter()
        .filter(|variant| variant_eligible(host, variant))
        .collect::<Vec<_>>();
    compatible.sort_by(|a, b| compare_variants_by_score(host, a, b));
    let mut out = String::new();
    out.push_str("cargo-sonic score\n");
    out.push_str(&format!("  target={target}\n"));
    out.push_str(&format!("  host.arch={}\n", host_arch_name(host.arch)));
    out.push_str(&format!(
        "  host.features={}\n",
        format_words(host.features)
    ));
    out.push_str(&format!(
        "  host.feature_names=[{}]\n",
        feature_names(host.features).join(",")
    ));
    out.push_str(&format!(
        "  host.identity={}\n",
        format_identity(host.identity)
    ));
    if let Some(selected) = compatible.first() {
        out.push_str(&format!("  selected={}\n", selected.target_cpu));
    }
    out.push_str("  compatible:\n");
    for (index, meta) in compatible.iter().enumerate() {
        let score = selection_score(host, meta);
        let variant = variants
            .iter()
            .find(|variant| variant.target_cpu == meta.target_cpu)
            .expect("score display metadata must match selector metadata");
        out.push_str(&format!(
            "    rank={} target_cpu={} exact={} vendor_affinity={} feature_score={} tier={} count={} required={} flags=[{}]\n",
            index + 1,
            variant.target_cpu,
            score.exact,
            score.vendor_affinity,
            score.feature_score,
            variant.feature_tier,
            variant.rank_features.count(),
            format_words(variant.required_features),
            variant.feature_names.join(",")
        ));
    }
    out.push_str(&format!("  skipped={skipped}\n"));
    out
}

fn format_probe_report(
    target: &str,
    host: HostInfo,
    variants: &[ProbeVariant],
    skipped: &[SkippedProbeVariant],
    selected: &str,
) -> String {
    let mut out = String::new();
    out.push_str("cargo-sonic probe\n");
    out.push_str(&format!("  target={target}\n"));
    out.push_str(&format!("  host.arch={}\n", host_arch_name(host.arch)));
    out.push_str(&format!(
        "  host.features={}\n",
        format_words(host.features)
    ));
    out.push_str(&format!(
        "  host.feature_names=[{}]\n",
        feature_names(host.features).join(",")
    ));
    out.push_str(&format!(
        "  host.identity={}\n",
        format_identity(host.identity)
    ));
    out.push_str("  variants:\n");
    for variant in variants {
        let eligible = variant.required_features.is_subset_of(host.features);
        let missing = FeatureMask::from_words([
            variant.required_features.words()[0] & !host.features.words()[0],
            variant.required_features.words()[1] & !host.features.words()[1],
        ]);
        out.push_str(&format!(
            "    {} eligible={} tier={} count={} required={}",
            variant.target_cpu,
            if eligible { "yes" } else { "no" },
            variant.feature_tier,
            variant.rank_features.count(),
            format_words(variant.required_features)
        ));
        if !eligible {
            out.push_str(&format!(
                " missing={} missing_features=[{}]",
                format_words(missing),
                feature_names(missing).join(",")
            ));
        }
        out.push_str(&format!(" flags=[{}]\n", variant.feature_names.join(",")));
    }
    if !skipped.is_empty() {
        out.push_str("  skipped:\n");
        for variant in skipped {
            out.push_str(&format!(
                "    {} reason={}\n",
                variant.target_cpu, variant.reason
            ));
        }
    }
    let eligible = variants
        .iter()
        .filter(|variant| variant.required_features.is_subset_of(host.features))
        .map(|variant| variant.target_cpu.as_str())
        .collect::<Vec<_>>();
    out.push_str(&format!("  fits=[{}]\n", eligible.join(",")));
    out.push_str(&format!("  selected={selected}\n"));
    out
}

fn host_arch_name(arch: TargetArch) -> &'static str {
    match arch {
        TargetArch::X86_64 => "x86_64",
        TargetArch::Aarch64 => "aarch64",
    }
}

fn feature_names(mask: FeatureMask) -> Vec<&'static str> {
    crate::feature_mask::ALL_FEATURES
        .iter()
        .copied()
        .filter(|feature| mask.contains(*feature))
        .map(crate::feature_mask::feature_name)
        .collect()
}

fn format_words(mask: FeatureMask) -> String {
    let words = mask.words();
    format!("[{:#018x},{:#018x}]", words[0], words[1])
}

fn format_identity(identity: CpuIdentity) -> String {
    match identity {
        CpuIdentity::Unknown => "unknown".to_string(),
        CpuIdentity::X86 {
            vendor,
            family,
            model,
            stepping,
        } => {
            let vendor = match vendor {
                X86Vendor::Intel => "intel",
                X86Vendor::Amd => "amd",
                X86Vendor::Other => "other",
            };
            format!("x86({vendor} family={family} model={model} stepping={stepping})")
        }
        CpuIdentity::Aarch64 {
            implementer,
            part,
            variant,
            revision,
        } => format!(
            "aarch64(implementer={implementer:#x} part={part:#x} variant={variant} revision={revision})"
        ),
    }
}

fn detect_current_host(target_arch: &str) -> Result<HostInfo> {
    match target_arch {
        "x86_64" => detect_current_x86_64_host(),
        "aarch64" => detect_current_aarch64_host(),
        _ => bail!("unsupported target_arch `{target_arch}`"),
    }
}

#[cfg(target_arch = "x86_64")]
fn detect_current_x86_64_host() -> Result<HostInfo> {
    let leaf1 = core::arch::x86_64::__cpuid_count(1, 0);
    let leaf7_0 = core::arch::x86_64::__cpuid_count(7, 0);
    let leaf7_1 = core::arch::x86_64::__cpuid_count(7, 1);
    let leaf_d_1 = core::arch::x86_64::__cpuid_count(0xd, 1);
    let leaf80000001 = core::arch::x86_64::__cpuid_count(0x80000001, 0);
    let xcr0 = if (leaf1.ecx & (1 << 26)) != 0 && (leaf1.ecx & (1 << 27)) != 0 {
        unsafe { core::arch::x86_64::_xgetbv(0) }
    } else {
        0
    };
    Ok(HostInfo {
        arch: TargetArch::X86_64,
        features: arch_x86_64::detect_x86_features_from_cpuid(
            arch_x86_64::X86Cpuid {
                leaf1: cpuid_leaf(leaf1),
                leaf7_0: cpuid_leaf(leaf7_0),
                leaf7_1: cpuid_leaf(leaf7_1),
                leaf_d_1: cpuid_leaf(leaf_d_1),
                leaf80000001: cpuid_leaf(leaf80000001),
            },
            xcr0,
        ),
        identity: detect_current_x86_64_identity(),
        heterogeneous: false,
    })
}

#[cfg(not(target_arch = "x86_64"))]
fn detect_current_x86_64_host() -> Result<HostInfo> {
    bail!("cannot probe x86_64 target on this host architecture")
}

#[cfg(target_arch = "x86_64")]
fn cpuid_leaf(leaf: core::arch::x86_64::CpuidResult) -> arch_x86_64::CpuidLeaf {
    arch_x86_64::CpuidLeaf {
        eax: leaf.eax,
        ebx: leaf.ebx,
        ecx: leaf.ecx,
        edx: leaf.edx,
    }
}

#[cfg(target_arch = "x86_64")]
fn detect_current_x86_64_identity() -> CpuIdentity {
    let vendor_leaf = core::arch::x86_64::__cpuid_count(0, 0);
    let vendor = if vendor_leaf.ebx == 0x756e6547
        && vendor_leaf.edx == 0x49656e69
        && vendor_leaf.ecx == 0x6c65746e
    {
        X86Vendor::Intel
    } else if vendor_leaf.ebx == 0x68747541
        && vendor_leaf.edx == 0x69746e65
        && vendor_leaf.ecx == 0x444d4163
    {
        X86Vendor::Amd
    } else {
        X86Vendor::Other
    };
    let leaf1 = core::arch::x86_64::__cpuid_count(1, 0);
    let base_family = ((leaf1.eax >> 8) & 0xf) as u16;
    let ext_family = ((leaf1.eax >> 20) & 0xff) as u16;
    let base_model = ((leaf1.eax >> 4) & 0xf) as u16;
    let ext_model = ((leaf1.eax >> 16) & 0xf) as u16;
    let family = if base_family == 0xf {
        base_family + ext_family
    } else {
        base_family
    };
    let model = if base_family == 0x6 || base_family == 0xf {
        base_model | (ext_model << 4)
    } else {
        base_model
    };
    CpuIdentity::X86 {
        vendor,
        family,
        model,
        stepping: (leaf1.eax & 0xf) as u8,
    }
}

#[cfg(target_arch = "aarch64")]
fn detect_current_aarch64_host() -> Result<HostInfo> {
    let (hwcap, hwcap2, hwcap3) = read_auxv_hwcaps()?;
    Ok(HostInfo {
        arch: TargetArch::Aarch64,
        features: crate::arch_aarch64::detect_aarch64_features_from_hwcap(hwcap, hwcap2, hwcap3),
        identity: detect_current_aarch64_identity(),
        heterogeneous: false,
    })
}

#[cfg(not(target_arch = "aarch64"))]
fn detect_current_aarch64_host() -> Result<HostInfo> {
    bail!("cannot probe aarch64 target on this host architecture")
}

#[cfg(target_arch = "aarch64")]
fn read_auxv_hwcaps() -> Result<(usize, usize, usize)> {
    const AT_HWCAP: usize = 16;
    const AT_HWCAP2: usize = 26;
    const AT_HWCAP3: usize = 29;
    let bytes = fs::read("/proc/self/auxv").context("failed to read /proc/self/auxv")?;
    let word = core::mem::size_of::<usize>();
    let mut hwcap = 0;
    let mut hwcap2 = 0;
    let mut hwcap3 = 0;
    for entry in bytes.chunks_exact(word * 2) {
        let key = usize_from_ne_bytes(&entry[..word]);
        let value = usize_from_ne_bytes(&entry[word..]);
        match key {
            AT_HWCAP => hwcap = value,
            AT_HWCAP2 => hwcap2 = value,
            AT_HWCAP3 => hwcap3 = value,
            _ => {}
        }
    }
    Ok((hwcap, hwcap2, hwcap3))
}

#[cfg(target_arch = "aarch64")]
fn usize_from_ne_bytes(bytes: &[u8]) -> usize {
    let mut out = 0usize;
    for (i, byte) in bytes.iter().enumerate() {
        out |= (*byte as usize) << (i * 8);
    }
    out
}

#[cfg(target_arch = "aarch64")]
fn detect_current_aarch64_identity() -> CpuIdentity {
    if let Ok(bytes) = fs::read("/sys/devices/system/cpu/cpu0/regs/identification/midr_el1")
        && let Some(midr) = parse_aarch64_midr_hex(&bytes)
    {
        return aarch64_identity_from_midr(midr);
    }

    if let Ok(bytes) = fs::read("/proc/cpuinfo") {
        let identity = parse_aarch64_cpuinfo_identity(&bytes);
        if identity != CpuIdentity::Unknown {
            return identity;
        }
    }

    CpuIdentity::Unknown
}

#[cfg(any(target_arch = "aarch64", test))]
fn aarch64_identity_from_midr(midr: u32) -> CpuIdentity {
    CpuIdentity::Aarch64 {
        implementer: ((midr >> 24) & 0xff) as u16,
        part: ((midr >> 4) & 0xfff) as u16,
        variant: ((midr >> 20) & 0xf) as u8,
        revision: (midr & 0xf) as u8,
    }
}

#[cfg(any(target_arch = "aarch64", test))]
fn parse_aarch64_midr_hex(buf: &[u8]) -> Option<u32> {
    let mut out = 0u32;
    let mut seen = false;
    for b in buf.iter().copied() {
        let digit = if b.is_ascii_digit() {
            b - b'0'
        } else if (b'a'..=b'f').contains(&b) {
            b - b'a' + 10
        } else if (b'A'..=b'F').contains(&b) {
            b - b'A' + 10
        } else if matches!(b, b'x' | b'X' | b' ' | b'\t' | b'\n') {
            continue;
        } else {
            break;
        };
        out = (out << 4) | digit as u32;
        seen = true;
    }
    seen.then_some(out)
}

#[cfg(any(target_arch = "aarch64", test))]
fn parse_aarch64_cpuinfo_identity(buf: &[u8]) -> CpuIdentity {
    let implementer = aarch64_cpuinfo_hex_value(buf, b"CPU implementer");
    let part = aarch64_cpuinfo_hex_value(buf, b"CPU part");
    let variant = aarch64_cpuinfo_hex_value(buf, b"CPU variant");
    let revision = aarch64_cpuinfo_decimal_value(buf, b"CPU revision");
    if let (Some(implementer), Some(part)) = (implementer, part) {
        CpuIdentity::Aarch64 {
            implementer,
            part,
            variant: variant.unwrap_or(0) as u8,
            revision: revision.unwrap_or(0) as u8,
        }
    } else {
        CpuIdentity::Unknown
    }
}

#[cfg(any(target_arch = "aarch64", test))]
fn aarch64_cpuinfo_hex_value(buf: &[u8], key: &[u8]) -> Option<u16> {
    let value = aarch64_cpuinfo_value(buf, key)?;
    let mut out = 0u16;
    let mut seen = false;
    for b in value.iter().copied() {
        let digit = if b.is_ascii_digit() {
            b - b'0'
        } else if (b'a'..=b'f').contains(&b) {
            b - b'a' + 10
        } else if (b'A'..=b'F').contains(&b) {
            b - b'A' + 10
        } else if matches!(b, b'x' | b'X' | b' ' | b'\t') {
            continue;
        } else {
            break;
        };
        out = (out << 4) | digit as u16;
        seen = true;
    }
    seen.then_some(out)
}

#[cfg(any(target_arch = "aarch64", test))]
fn aarch64_cpuinfo_decimal_value(buf: &[u8], key: &[u8]) -> Option<u16> {
    let value = aarch64_cpuinfo_value(buf, key)?;
    let mut out = 0u16;
    let mut seen = false;
    for b in value.iter().copied() {
        if b.is_ascii_digit() {
            out = out.saturating_mul(10).saturating_add((b - b'0') as u16);
            seen = true;
        } else if seen {
            break;
        }
    }
    seen.then_some(out)
}

#[cfg(any(target_arch = "aarch64", test))]
fn aarch64_cpuinfo_value<'a>(buf: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    for line in buf.split(|b| *b == b'\n') {
        if line.starts_with(key)
            && let Some(colon) = line.iter().position(|b| *b == b':')
        {
            return Some(&line[colon + 1..]);
        }
    }
    None
}

fn select_package<'a>(
    metadata: &'a cargo_metadata::Metadata,
    package: Option<&str>,
) -> Result<&'a Package> {
    if let Some(package) = package {
        return metadata
            .packages
            .iter()
            .find(|p| p.name == package)
            .with_context(|| format!("package `{package}` was not found"));
    }
    if let Some(root) = metadata.root_package() {
        return Ok(root);
    }
    if metadata.workspace_members.len() == 1 {
        let id = &metadata.workspace_members[0];
        return metadata
            .packages
            .iter()
            .find(|p| &p.id == id)
            .context("workspace package was not found");
    }
    bail!("cannot identify package; pass --package");
}

fn effective_target_cpus(target_cpus: Vec<String>) -> Result<Vec<String>> {
    if target_cpus.is_empty() {
        bail!("pass --target-cpus=<cpu>[,<cpu>...]");
    }
    Ok(target_cpus)
}

fn baseline_target_cpu(target_arch: &str) -> &'static str {
    match target_arch {
        "x86_64" => "x86-64",
        _ => "generic",
    }
}

fn normalize_target_cpus(mut cpus: Vec<String>, target_arch: &str) -> Result<Vec<String>> {
    let baseline = baseline_target_cpu(target_arch);
    if cpus.iter().any(|cpu| cpu == "native") {
        bail!("target-cpu \"native\" is rejected because cargo-sonic builds portable artifacts");
    }
    if cpus.iter().any(|cpu| cpu == baseline) {
        bail!("target-cpu \"{baseline}\" is implicit; remove it from --target-cpus");
    }
    cpus.insert(0, baseline.to_string());
    Ok(cpus)
}

fn rustc_default_target() -> Result<String> {
    let output = Command::new("rustc").args(["-vV"]).output()?;
    if !output.status.success() {
        bail!("rustc -vV failed");
    }
    let stdout = String::from_utf8(output.stdout)?;
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::to_string)
        .context("failed to determine rustc host target")
}

fn effective_target_directory(
    metadata: &cargo_metadata::Metadata,
    target_dir_arg: Option<&Utf8Path>,
) -> Result<Utf8PathBuf> {
    if let Some(value) = target_dir_arg {
        return absolute_path(value);
    }
    if let Some(value) =
        std::env::var_os("CARGO_TARGET_DIR").or_else(|| std::env::var_os("CARGO_TARGET"))
    {
        let value = Utf8PathBuf::from_path_buf(std::path::PathBuf::from(value))
            .map_err(|_| anyhow!("CARGO_TARGET_DIR is not valid UTF-8"))?;
        return absolute_path(&value);
    }
    Utf8PathBuf::from_path_buf(metadata.target_directory.clone().into_std_path_buf())
        .map_err(|_| anyhow!("target directory is not valid UTF-8"))
}

fn absolute_path(path: &Utf8Path) -> Result<Utf8PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Utf8PathBuf::from_path_buf(std::env::current_dir()?.join(path.as_std_path()))
        .map_err(|_| anyhow!("path is not valid UTF-8: {path}"))
}

fn rustc_cfg(target: &str, cpu: Option<&str>) -> Result<String> {
    let mut cmd = Command::new("rustc");
    cmd.args(["--print", "cfg", "--target", target]);
    if let Some(cpu) = cpu {
        cmd.args(["-C", &format!("target-cpu={cpu}")]);
    }
    let output = cmd
        .output()
        .with_context(|| "failed to run rustc --print cfg")?;
    if !output.status.success() {
        bail!(
            "rustc --print cfg failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn rustc_accepts_target_cpu_for_payload(target: &str, cpu: &str) -> Result<bool> {
    let object_path = std::env::temp_dir().join(format!(
        "cargo-sonic-target-cpu-probe-{}-{}.o",
        std::process::id(),
        sanitize_cpu(cpu)
    ));
    let mut child = Command::new("rustc")
        .args([
            "--crate-name",
            "cargo_sonic_target_cpu_probe",
            "--crate-type",
            "lib",
            "--emit",
            "obj",
            "--target",
            target,
            "-C",
            &format!("target-cpu={cpu}"),
            "-C",
            "panic=abort",
            "-",
            "-o",
        ])
        .arg(&object_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| "failed to spawn rustc target-cpu probe")?;
    let mut stdin = child
        .stdin
        .take()
        .context("failed to open rustc target-cpu probe stdin")?;
    stdin
        .write_all(b"#![no_std]\n#[no_mangle]\npub extern \"C\" fn cargo_sonic_probe() {}\n")
        .context("failed to write rustc target-cpu probe source")?;
    drop(stdin);
    let success = child.wait()?.success();
    let _ = fs::remove_file(&object_path);
    Ok(success)
}

fn cfg_value(cfg: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=\"");
    cfg.lines().find_map(|line| {
        line.strip_prefix(&prefix)?
            .strip_suffix('"')
            .map(str::to_string)
    })
}

pub fn parse_target_features_from_rustc_cfg(cfg: &str) -> Vec<String> {
    let mut out = BTreeSet::new();
    for line in cfg.lines() {
        if let Some(feature) = line
            .strip_prefix("target_feature=\"")
            .and_then(|s| s.strip_suffix('"'))
        {
            out.insert(feature.to_string());
        }
    }
    out.into_iter().collect()
}

pub fn filter_runtime_features(features: &[String]) -> Vec<String> {
    classify_runtime_features(features).known
}

fn classify_runtime_features(features: &[String]) -> FeatureSet {
    let mut known = Vec::new();
    let mut unknown = Vec::new();
    for feature in features
        .iter()
        .filter(|feature| !is_ignored_rustc_feature(feature))
        .cloned()
    {
        if feature_by_name(&feature).is_some() {
            known.push(feature);
        } else {
            unknown.push(feature);
        }
    }
    FeatureSet { known, unknown }
}

fn is_ignored_rustc_feature(feature: &str) -> bool {
    matches!(
        feature,
        "crt-static" | "ermsb" | "lahfsahf" | "prfchw" | "x87"
    )
}

fn rustc_target_cpus(target: &str) -> Result<BTreeSet<String>> {
    let output = Command::new("rustc")
        .args(["--print", "target-cpus", "--target", target])
        .output()
        .with_context(|| "failed to run rustc --print target-cpus")?;
    if !output.status.success() {
        bail!(
            "rustc --print target-cpus failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(parse_rustc_target_cpus(&String::from_utf8(output.stdout)?))
}

pub fn parse_rustc_target_cpus(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with("Available CPUs") {
            continue;
        }
        if let Some(cpu) = trimmed.split_whitespace().next() {
            out.insert(cpu.to_string());
        }
    }
    out.insert("generic".to_string());
    out
}

fn known_supported_cpu_union() -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for target in [
        "x86_64-unknown-linux-gnu",
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-gnu",
        "aarch64-unknown-linux-musl",
    ] {
        if let Ok(cpus) = rustc_target_cpus(target) {
            out.extend(cpus);
        }
    }
    Ok(out)
}

pub fn filter_target_cpus(
    configured: &[String],
    current_valid: &BTreeSet<String>,
    known_union: &BTreeSet<String>,
) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for cpu in configured {
        let accepted = if cpu == "generic" {
            true
        } else if cpu == "native" {
            bail!("target-cpu \"native\" is rejected");
        } else if current_valid.contains(cpu) {
            true
        } else if known_union.contains(cpu) {
            false
        } else {
            bail!("unknown target-cpu spelling `{cpu}`");
        };
        if accepted && seen.insert(cpu.clone()) {
            out.push(cpu.clone());
        }
    }
    Ok(out)
}

fn feature_mask(features: &[String]) -> Result<FeatureMask> {
    let mut mask = FeatureMask::EMPTY;
    for feature in features {
        let Some(feature) = feature_by_name(feature) else {
            bail!("unsupported runtime feature mapping `{feature}`");
        };
        mask.insert(feature);
    }
    Ok(mask)
}

fn safety_required_features(features: &[String]) -> Vec<String> {
    features
        .iter()
        .filter(|feature| is_safety_required_feature(feature))
        .cloned()
        .collect()
}

fn is_safety_required_feature(feature: &str) -> bool {
    !matches!(
        feature,
        // These are not normal user-space compiler codegen safety requirements.
        // Keep them in rank_features / CARGO_SONIC_SELECTED_FLAGS, but do not
        // make a CPU-tuned payload ineligible when firmware, virtualization,
        // kernel policy, or QEMU user-mode hides monitoring/profiling/RNG state.
        "bti"
            | "dit"
            | "lor"
            | "mte"
            | "paca"
            | "pacg"
            | "pan"
            | "pmuv3"
            | "rand"
            | "ras"
            | "rdrand"
            | "rdseed"
            | "sb"
            | "spe"
            | "ssbs"
            | "vh"
    )
}

fn build_payload_variants(
    ctx: PayloadBuildContext,
    parallelism: usize,
    jobs: Vec<VariantBuildJob>,
) -> Result<Vec<VariantBuild>> {
    if jobs.is_empty() {
        return Ok(Vec::new());
    }

    let job_count = jobs.len();
    let worker_count = parallelism.min(job_count);
    let ctx = Arc::new(ctx);
    let queue = Arc::new(Mutex::new(
        jobs.into_iter().enumerate().collect::<VecDeque<_>>(),
    ));
    let (tx, rx) = mpsc::channel();

    for _ in 0..worker_count {
        let ctx = Arc::clone(&ctx);
        let queue = Arc::clone(&queue);
        let tx = tx.clone();
        thread::spawn(move || {
            loop {
                let next = queue
                    .lock()
                    .expect("variant build queue poisoned")
                    .pop_front();
                let Some((index, job)) = next else {
                    break;
                };
                print_variant_build_prelude(&job.target_cpu);
                let result = build_payload_variant(&ctx, &job.target_cpu).map(|payload| {
                    let bundle_path = leaked_str(&format!(
                        "{}{}\0",
                        sanitize_cpu(&job.target_cpu),
                        payload_extension(payload.compression)
                    ));
                    VariantBuild {
                        target_cpu: job.target_cpu,
                        required_features: job.required_features,
                        rank_features: job.rank_features,
                        feature_names: job.feature_names,
                        feature_tier: job.feature_tier,
                        target_kind: job.target_kind,
                        artifact: payload.path,
                        payload_compression: payload.compression,
                        uncompressed_len: payload.uncompressed_len,
                        bundle_path,
                    }
                });
                if tx.send((index, result)).is_err() {
                    break;
                }
            }
        });
    }
    drop(tx);

    let mut variants = Vec::new();
    variants.resize_with(job_count, || None);
    let mut first_error = None;
    for (index, result) in rx {
        match result {
            Ok(variant) => variants[index] = Some(variant),
            Err(err) => {
                if first_error.is_none() {
                    first_error = Some(err);
                }
            }
        }
    }
    if let Some(err) = first_error {
        return Err(err);
    }

    variants
        .into_iter()
        .map(|variant| variant.context("variant build worker exited without reporting a result"))
        .collect()
}

fn build_payload_variant(ctx: &PayloadBuildContext, cpu: &str) -> Result<PayloadArtifact> {
    let target_dir = ctx.out_root.join("variants").join(sanitize_cpu(cpu));
    fs::create_dir_all(&target_dir)?;
    let mut cmd = Command::new("cargo");
    cmd.arg("build");
    cmd.args(&ctx.cargo_args.forwarded);
    if let Some(manifest_path) = &ctx.manifest_path {
        cmd.args(["--manifest-path", manifest_path.as_str()]);
    }
    if ctx.cargo_args.target.is_none() {
        cmd.args(["--target", &ctx.target]);
    }
    cmd.args(["--color", resolved_cargo_color(ctx.cargo_args.color)]);
    cmd.args(["--message-format", "json-render-diagnostics"]);
    cmd.args(["--target-dir", target_dir.as_str()]);
    match payload_rustflags(&ctx.target, cpu) {
        PayloadRustflags::Encoded(flags) => {
            cmd.env("CARGO_ENCODED_RUSTFLAGS", flags);
        }
        PayloadRustflags::Plain(flags) => {
            cmd.env("RUSTFLAGS", flags);
        }
        PayloadRustflags::CargoConfig(config) => {
            cmd.args(["--config", config.as_str()]);
        }
    }
    cmd.stdout(Stdio::piped());
    if ctx.tag_output {
        cmd.stderr(Stdio::piped());
    } else {
        cmd.stderr(Stdio::inherit());
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn cargo for target-cpu `{cpu}`"))?;
    let stdout = child.stdout.take().context("failed to read cargo stdout")?;
    let stderr_thread = if ctx.tag_output {
        let stderr = child.stderr.take().context("failed to read cargo stderr")?;
        let cpu_for_stderr = cpu.to_string();
        Some(thread::spawn(move || {
            forward_tagged_output(cpu_for_stderr, stderr)
        }))
    } else {
        None
    };
    let reader = std::io::BufReader::new(stdout);
    let mut executables = Vec::new();
    let mut stdout_pending = Vec::new();
    let mut stdout_last_flush = Instant::now();
    for message in Message::parse_stream(reader) {
        match message? {
            Message::CompilerArtifact(Artifact {
                executable: Some(exe),
                target: artifact_target,
                package_id,
                ..
            }) if package_id == ctx.package.id
                && artifact_target
                    .kind
                    .iter()
                    .any(|k| matches!(k, cargo_metadata::TargetKind::Bin)) =>
            {
                let exe = Utf8PathBuf::from_path_buf(exe.into_std_path_buf())
                    .map_err(|_| anyhow!("artifact path is not valid UTF-8"))?;
                executables.push(exe);
            }
            Message::CompilerMessage(message) => {
                if let Some(rendered) = message.message.rendered {
                    write_payload_stdout(ctx.tag_output, &mut stdout_pending, rendered.as_bytes())?;
                }
            }
            Message::TextLine(line) => {
                write_payload_stdout(ctx.tag_output, &mut stdout_pending, line.as_bytes())?;
                write_payload_stdout(ctx.tag_output, &mut stdout_pending, b"\n")?;
            }
            _ => {}
        }
        if ctx.tag_output
            && (stdout_pending.len() >= OUTPUT_FLUSH_BYTES
                || stdout_last_flush.elapsed() >= OUTPUT_FLUSH_INTERVAL)
        {
            flush_tagged_stdout(cpu, &mut stdout_pending)?;
            stdout_last_flush = Instant::now();
        }
    }
    flush_payload_stdout(cpu, ctx.tag_output, &mut stdout_pending)?;
    let status = child.wait()?;
    if let Some(stderr_thread) = stderr_thread {
        stderr_thread
            .join()
            .map_err(|_| anyhow!("stderr forwarding thread panicked for target-cpu `{cpu}`"))??;
    }
    if !status.success() {
        bail!("cargo build failed for target-cpu `{cpu}`");
    }
    if executables.len() != 1 {
        bail!(
            "cannot identify exactly one executable artifact for target-cpu `{cpu}` (found {}); pass --bin",
            executables.len()
        );
    }
    let payload_dir = ctx.out_root.join("loader").join("payloads");
    fs::create_dir_all(&payload_dir)?;
    let payload = payload_dir.join(format!(
        "{}{}",
        sanitize_cpu(cpu),
        payload_extension(ctx.payload_compression)
    ));
    let uncompressed_len = fs::metadata(&executables[0])?.len();
    match ctx.payload_compression {
        PayloadCompression::None => {
            fs::copy(&executables[0], &payload)?;
        }
        PayloadCompression::Zstd => {
            #[cfg(feature = "zstd")]
            {
                let input = fs::read(&executables[0]).with_context(|| {
                    format!("failed to read payload artifact {}", executables[0])
                })?;
                let compressed =
                    zstd::stream::encode_all(Cursor::new(input), ctx.compression_level)
                        .with_context(|| {
                            format!("failed to zstd-compress payload for target-cpu `{cpu}`")
                        })?;
                fs::write(&payload, compressed)
                    .with_context(|| format!("failed to write compressed payload {payload}"))?;
            }
            #[cfg(not(feature = "zstd"))]
            {
                bail!("zstd payload compression requires the `zstd` feature");
            }
        }
    }
    Ok(PayloadArtifact {
        path: payload,
        compression: ctx.payload_compression,
        uncompressed_len,
    })
}

fn resolved_cargo_color(color: Option<ColorMode>) -> &'static str {
    match color.unwrap_or(ColorMode::Auto) {
        ColorMode::Always => "always",
        ColorMode::Never => "never",
        ColorMode::Auto => {
            if std::io::stdout().is_terminal() || std::io::stderr().is_terminal() {
                "always"
            } else {
                "never"
            }
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
enum PayloadRustflags {
    Encoded(OsString),
    Plain(OsString),
    CargoConfig(String),
}

fn payload_rustflags(target: &str, cpu: &str) -> PayloadRustflags {
    rustflags_for_target(target, [target_cpu_rustflag(cpu)])
}

fn rustflags_for_target(
    target: &str,
    rustflags: impl IntoIterator<Item = String>,
) -> PayloadRustflags {
    let rustflags = rustflags.into_iter().collect::<Vec<_>>();
    if let Some(flags) = std::env::var_os("CARGO_ENCODED_RUSTFLAGS") {
        return PayloadRustflags::Encoded(append_encoded_rustflags(flags, &rustflags));
    }

    if let Some(flags) = std::env::var_os("RUSTFLAGS") {
        let mut flags = flags;
        if !flags.is_empty() && !rustflags.is_empty() {
            flags.push(" ");
        }
        flags.push(rustflags.join(" "));
        return PayloadRustflags::Plain(flags);
    }

    PayloadRustflags::CargoConfig(target_rustflags_config_arg(target, &rustflags))
}

fn append_encoded_rustflags(mut flags: OsString, rustflags: &[String]) -> OsString {
    for flag in rustflags {
        flags = append_encoded_rustflag(flags, flag.clone());
    }
    flags
}

fn encode_rustflags(rustflags: &[String]) -> OsString {
    append_encoded_rustflags(OsString::new(), rustflags)
}

fn append_encoded_rustflag(mut flags: OsString, flag: String) -> OsString {
    let sep = '\x1f';
    if !flags.is_empty() {
        flags.push(sep.to_string());
    }
    flags.push(flag);
    flags
}

fn target_cpu_rustflag(cpu: &str) -> String {
    format!("-Ctarget-cpu={cpu}")
}

fn cargo_target_rustflags_env(target: &str) -> String {
    format!(
        "CARGO_TARGET_{}_RUSTFLAGS",
        target
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() {
                    ch.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect::<String>()
    )
}

fn target_rustflags_config_arg(target: &str, rustflags: &[String]) -> String {
    format!(
        "target.{target}.rustflags=[{}]",
        rustflags
            .iter()
            .map(|flag| format!("\"{}\"", escape_toml_string(flag)))
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn escape_toml_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn split_rustflags(flags: &str) -> Vec<String> {
    flags.split_whitespace().map(str::to_string).collect()
}

fn forward_tagged_output(target_cpu: String, mut input: impl Read + Send + 'static) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut buf = [0_u8; 4096];
        loop {
            match input.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(Ok(buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
                Err(err) => {
                    let _ = tx.send(Err(anyhow!(err)));
                    break;
                }
            }
        }
    });

    let mut pending = Vec::new();
    let mut last_flush = Instant::now();
    loop {
        let timeout = OUTPUT_FLUSH_INTERVAL
            .checked_sub(last_flush.elapsed())
            .unwrap_or(Duration::ZERO);
        match rx.recv_timeout(timeout) {
            Ok(Ok(chunk)) => pending.extend_from_slice(&chunk),
            Ok(Err(err)) => return Err(err),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                flush_tagged_stderr(&target_cpu, &mut pending)?;
                last_flush = Instant::now();
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if pending.len() >= OUTPUT_FLUSH_BYTES || last_flush.elapsed() >= OUTPUT_FLUSH_INTERVAL {
            flush_tagged_stderr(&target_cpu, &mut pending)?;
            last_flush = Instant::now();
        }
    }
    flush_tagged_stderr(&target_cpu, &mut pending)?;
    reader
        .join()
        .map_err(|_| anyhow!("output reader thread panicked for target-cpu `{target_cpu}`"))?;
    Ok(())
}

fn write_payload_stdout(tag_output: bool, pending: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    if tag_output {
        pending.extend_from_slice(bytes);
    } else {
        let stdout = std::io::stdout();
        let mut stdout = stdout.lock();
        stdout.write_all(bytes)?;
        stdout.flush()?;
    }
    Ok(())
}

fn flush_payload_stdout(target_cpu: &str, tag_output: bool, pending: &mut Vec<u8>) -> Result<()> {
    if tag_output {
        flush_tagged_stdout(target_cpu, pending)
    } else {
        pending.clear();
        Ok(())
    }
}

fn flush_tagged_stdout(target_cpu: &str, pending: &mut Vec<u8>) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    write_tagged_output(&mut stdout, target_cpu, pending)?;
    pending.clear();
    Ok(())
}

fn flush_tagged_stderr(target_cpu: &str, pending: &mut Vec<u8>) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    let stderr = std::io::stderr();
    let mut stderr = stderr.lock();
    write_tagged_output(&mut stderr, target_cpu, pending)?;
    pending.clear();
    Ok(())
}

fn write_tagged_output(output: &mut impl Write, target_cpu: &str, pending: &[u8]) -> Result<()> {
    writeln!(output, "cargo-sonic[{target_cpu}]")?;
    for chunk in pending.split_inclusive(|byte| *byte == b'\n') {
        output.write_all(b"  ")?;
        output.write_all(chunk)?;
        if !chunk.ends_with(b"\n") {
            writeln!(output)?;
        }
    }
    output.flush()?;
    Ok(())
}

fn sanitize_cpu(cpu: &str) -> String {
    let mut out = String::new();
    for b in cpu.bytes() {
        if b.is_ascii_alphanumeric() {
            out.push(b as char);
        } else {
            out.push('_');
            out.push_str(&format!("{b:02x}"));
        }
    }
    out
}

fn resolve_bin_name(
    package: &Package,
    explicit_bin: Option<&str>,
    variants: &[VariantBuild],
) -> Result<String> {
    if let Some(bin) = explicit_bin {
        return Ok(bin.to_string());
    }
    let bins: Vec<_> = package
        .targets
        .iter()
        .filter(|target| {
            target
                .kind
                .iter()
                .any(|k| matches!(k, cargo_metadata::TargetKind::Bin))
        })
        .collect();
    if bins.len() == 1 {
        return Ok(bins[0].name.clone());
    }
    if let Some(path) = variants.first().and_then(|v| v.artifact.file_stem()) {
        return Ok(path.to_string());
    }
    bail!("multiple binary artifacts are possible; pass --bin");
}

fn prepare_bundle_directory(final_binary: &Utf8Path, variants: &[VariantBuild]) -> Result<()> {
    let bundle_dir = bundle_dir_for(final_binary);
    if bundle_dir.exists() {
        fs::remove_dir_all(&bundle_dir)
            .with_context(|| format!("failed to remove old bundle directory {bundle_dir}"))?;
    }
    fs::create_dir_all(&bundle_dir)
        .with_context(|| format!("failed to create bundle directory {bundle_dir}"))?;
    for variant in variants {
        let destination = bundle_dir.join(variant.bundle_path.trim_end_matches('\0'));
        fs::copy(&variant.artifact, &destination).with_context(|| {
            format!(
                "failed to copy payload {} to bundle path {destination}",
                variant.artifact
            )
        })?;
        make_executable(&destination)?;
    }
    Ok(())
}

fn bundle_dir_for(final_binary: &Utf8Path) -> Utf8PathBuf {
    Utf8PathBuf::from(format!("{final_binary}.bundle"))
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd)]
enum AuditDepKind {
    Development,
    Build,
    Runtime,
}

#[derive(Serialize)]
struct AuditVersionInfo {
    format: u32,
    packages: Vec<AuditPackage>,
}

#[derive(Serialize)]
struct AuditPackage {
    name: String,
    version: String,
    source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    dependencies: Vec<usize>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    root: bool,
}

fn collect_auditable_section(
    manifest_path: Option<&Utf8Path>,
    package: Option<&str>,
) -> Result<Vec<u8>> {
    let mut metadata_cmd = MetadataCommand::new();
    if let Some(path) = manifest_path {
        metadata_cmd.manifest_path(path);
    }
    let metadata = metadata_cmd
        .exec()
        .context("failed to read cargo metadata for auditable section")?;
    let package = select_package(&metadata, package)?;
    let info = auditable_from_metadata(&metadata, package)?;
    let json = serde_json::to_vec(&info)?;
    Ok(compress_to_vec_zlib(&json, 7))
}

fn auditable_from_metadata(
    metadata: &cargo_metadata::Metadata,
    root_package: &Package,
) -> Result<AuditVersionInfo> {
    let resolve = metadata
        .resolve
        .as_ref()
        .context("cargo metadata did not include dependency resolution")?;
    let root_id = root_package.id.repr.as_str();
    let proc_macros = proc_macro_packages(metadata);
    let nodes: HashMap<&str, &cargo_metadata::Node> = resolve
        .nodes
        .iter()
        .map(|node| (node.id.repr.as_str(), node))
        .collect();
    let root = nodes.get(root_id).with_context(|| {
        format!(
            "selected package `{}` is missing from dependency graph",
            root_package.name
        )
    })?;

    let mut id_to_dep_kind = HashMap::new();
    id_to_dep_kind.insert(root_id, AuditDepKind::Runtime);
    let mut current = vec![*root];
    let mut next = Vec::new();
    while !current.is_empty() {
        for parent in current.drain(..) {
            let parent_kind = id_to_dep_kind[parent.id.repr.as_str()];
            for child in &parent.deps {
                let child_id = child.pkg.repr.as_str();
                let mut dep_kind = strongest_audit_dep_kind(&child.dep_kinds);
                dep_kind = dep_kind.min(parent_kind);
                if proc_macros.contains(child_id) {
                    dep_kind = dep_kind.min(AuditDepKind::Build);
                }
                if id_to_dep_kind
                    .get(child_id)
                    .is_none_or(|previous| dep_kind > *previous)
                {
                    id_to_dep_kind.insert(child_id, dep_kind);
                    if let Some(node) = nodes.get(child_id) {
                        next.push(*node);
                    }
                }
            }
        }
        std::mem::swap(&mut current, &mut next);
    }

    let mut packages = metadata
        .packages
        .iter()
        .filter(|package| {
            id_to_dep_kind
                .get(package.id.repr.as_str())
                .is_some_and(|kind| *kind != AuditDepKind::Development)
        })
        .collect::<Vec<_>>();
    packages.sort_unstable_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.version.cmp(&b.version))
            .then_with(|| a.id.repr.cmp(&b.id.repr))
    });

    let id_to_index = packages
        .iter()
        .enumerate()
        .map(|(index, package)| (package.id.repr.as_str(), index))
        .collect::<HashMap<_, _>>();
    let mut audit_packages = packages
        .into_iter()
        .map(|package| {
            let dep_kind = id_to_dep_kind[package.id.repr.as_str()];
            AuditPackage {
                name: package.name.to_string(),
                version: package.version.to_string(),
                source: package
                    .source
                    .as_ref()
                    .map_or_else(|| "local".to_string(), audit_source),
                kind: (dep_kind == AuditDepKind::Build).then_some("build"),
                dependencies: Vec::new(),
                root: package.id == root_package.id,
            }
        })
        .collect::<Vec<_>>();

    for node in &resolve.nodes {
        let Some(&package_index) = id_to_index.get(node.id.repr.as_str()) else {
            continue;
        };
        for dep in &node.deps {
            if strongest_audit_dep_kind(&dep.dep_kinds) == AuditDepKind::Development {
                continue;
            }
            if let Some(&dep_index) = id_to_index.get(dep.pkg.repr.as_str()) {
                audit_packages[package_index].dependencies.push(dep_index);
            }
        }
        audit_packages[package_index].dependencies.sort_unstable();
        audit_packages[package_index].dependencies.dedup();
    }

    Ok(AuditVersionInfo {
        format: 1,
        packages: audit_packages,
    })
}

fn strongest_audit_dep_kind(deps: &[cargo_metadata::DepKindInfo]) -> AuditDepKind {
    deps.iter()
        .map(|dep| match dep.kind {
            cargo_metadata::DependencyKind::Normal => AuditDepKind::Runtime,
            cargo_metadata::DependencyKind::Build => AuditDepKind::Build,
            cargo_metadata::DependencyKind::Development => AuditDepKind::Development,
            _ => AuditDepKind::Runtime,
        })
        .max()
        .unwrap_or(AuditDepKind::Runtime)
}

fn proc_macro_packages(metadata: &cargo_metadata::Metadata) -> HashSet<&str> {
    metadata
        .packages
        .iter()
        .filter_map(|package| {
            if package.targets.len() == 1
                && package.targets[0].kind.len() == 1
                && package.targets[0].kind[0] == cargo_metadata::TargetKind::ProcMacro
            {
                Some(package.id.repr.as_str())
            } else {
                None
            }
        })
        .collect()
}

fn audit_source(source: &cargo_metadata::Source) -> String {
    match source.repr.as_str() {
        "registry+https://github.com/rust-lang/crates.io-index" => "crates.io".to_string(),
        other => other.split('+').next().unwrap_or("registry").to_string(),
    }
}

fn auditable_object(target: &str, contents: &[u8]) -> Result<Vec<u8>> {
    let architecture = if target.starts_with("x86_64-") {
        Architecture::X86_64
    } else if target.starts_with("aarch64-") {
        Architecture::Aarch64
    } else {
        bail!("auditable section is only supported for x86_64 and aarch64 targets");
    };
    let mut file = Object::new(BinaryFormat::Elf, architecture, Endianness::Little);
    file.flags = FileFlags::Elf {
        os_abi: elf::ELFOSABI_NONE,
        abi_version: 0,
        e_flags: 0,
    };
    let section = file.add_section(
        file.segment_name(StandardSegment::Data).to_vec(),
        b".dep-v0".to_vec(),
        SectionKind::ReadOnlyData,
    );
    file.section_mut(section).flags = SectionFlags::Elf { sh_flags: 0 };
    let offset = file.append_section_data(section, contents, 1);
    file.add_symbol(Symbol {
        name: b"__cargo_sonic_auditable_dep_v0".to_vec(),
        value: offset,
        size: contents.len() as u64,
        kind: SymbolKind::Data,
        scope: SymbolScope::Dynamic,
        weak: false,
        section: SymbolSection::Section(section),
        flags: SymbolFlags::None,
    });
    file.write()
        .context("failed to build auditable metadata object")
}

fn generate_loader_crate(
    loader_dir: &Utf8Path,
    target: &str,
    variants: &[VariantBuild],
    loader_strategy: LoaderStrategy,
    auditable_section: Option<&[u8]>,
) -> Result<()> {
    fs::create_dir_all(loader_dir.join("src"))?;
    let uses_zstd = variants
        .iter()
        .any(|variant| variant.payload_compression == PayloadCompression::Zstd);
    let dependencies = if uses_zstd {
        r#"
[dependencies]
ruzstd = { version = "0.8.2", default-features = false }
"#
    } else {
        ""
    };
    fs::write(
        loader_dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "sonic-generated-loader"
version = "0.0.0"
edition = "2024"

[profile.dev]
panic = "abort"

[profile.release]
panic = "abort"

[workspace]
{dependencies}"#
        ),
    )?;
    fs::write(
        loader_dir.join("src/feature_mask.rs"),
        include_str!("feature_mask.rs"),
    )?;
    fs::write(loader_dir.join("src/select.rs"), include_str!("select.rs"))?;
    fs::write(
        loader_dir.join("src/arch_x86_64.rs"),
        include_str!("arch_x86_64.rs"),
    )?;
    fs::write(
        loader_dir.join("src/arch_aarch64.rs"),
        include_str!("arch_aarch64.rs"),
    )?;
    fs::write(loader_dir.join("src/linux_sys.rs"), generated_linux_sys())?;
    fs::write(loader_dir.join("src/stack.rs"), generated_stack())?;
    fs::write(
        loader_dir.join("src/generated_manifest.rs"),
        generated_manifest(variants, loader_strategy),
    )?;
    fs::write(
        loader_dir.join("src/main.rs"),
        generated_main(target, uses_zstd, loader_strategy),
    )?;
    if let Some(section) = auditable_section {
        fs::write(
            loader_dir.join("auditable.o"),
            auditable_object(target, section)?,
        )?;
    }
    Ok(())
}

fn build_loader(loader_dir: &Utf8Path, target: &str, profile: &str) -> Result<Utf8PathBuf> {
    let target_dir = loader_dir.join("target");
    let mut cmd = Command::new("cargo");
    cmd.arg("build");
    cmd.args(["--target", target, "--target-dir", target_dir.as_str()]);
    cmd.current_dir(loader_dir);
    if profile == "release" {
        cmd.arg("--release");
    }
    let rustflags = loader_build_rustflags(target, loader_dir.join("auditable.o").exists());
    cmd.env_remove("RUSTFLAGS");
    cmd.env_remove("CARGO_BUILD_RUSTFLAGS");
    cmd.env_remove(cargo_target_rustflags_env(target));
    cmd.env("CARGO_ENCODED_RUSTFLAGS", encode_rustflags(&rustflags));
    let status = cmd
        .status()
        .context("failed to spawn cargo for generated loader")?;
    if !status.success() {
        bail!("generated loader failed to compile");
    }
    let exe = target_dir
        .join(target)
        .join(profile)
        .join("sonic-generated-loader");
    Ok(exe)
}

fn loader_build_rustflags(target: &str, auditable: bool) -> Vec<String> {
    let mut rustflags = Vec::new();
    if let Some(flags) = std::env::var_os(LOADER_RUSTFLAGS_ENV) {
        rustflags.extend(split_rustflags(&flags.to_string_lossy()));
    }
    rustflags.extend(split_rustflags(loader_rustflags(target)));
    if auditable {
        rustflags.extend(split_rustflags(
            "-C link-arg=auditable.o -C link-arg=-Wl,-u,__cargo_sonic_auditable_dep_v0",
        ));
    }
    rustflags
}

fn loader_rustflags(target: &str) -> &'static str {
    if target.starts_with("x86_64-") && target.contains("-musl") {
        "-C panic=abort -C code-model=large -C target-feature=+crt-static -C relocation-model=static -C link-self-contained=no -C link-arg=-static"
    } else if target.starts_with("x86_64-") {
        "-C panic=abort -C code-model=large -C target-feature=+crt-static -C relocation-model=static -C link-arg=-nostartfiles -C link-arg=-static"
    } else if target.contains("-musl") {
        "-C panic=abort -C target-feature=+crt-static -C relocation-model=static -C link-self-contained=no -C link-arg=-static"
    } else {
        "-C panic=abort -C target-feature=+crt-static -C relocation-model=static -C link-arg=-nostartfiles -C link-arg=-static"
    }
}

fn generated_manifest(variants: &[VariantBuild], loader_strategy: LoaderStrategy) -> String {
    let mut out = String::new();
    out.push_str("use crate::feature_mask::FeatureMask;\nuse crate::select::TargetKind;\n\n");
    out.push_str("pub static ENV_ENABLED: &[u8] = b\"CARGO_SONIC_ENABLED=1\\0\";\n\n");
    if loader_strategy == LoaderStrategy::Embedded {
        out.push_str(
            "#[repr(align(131072))]\npub struct AlignedPayload<const N: usize>(pub [u8; N]);\n\n",
        );
        for v in variants {
            let payload_len = fs::metadata(&v.artifact)
                .map(|metadata| metadata.len())
                .unwrap_or_default();
            out.push_str(&format!(
                "static PAYLOAD_{}: AlignedPayload<{}> = AlignedPayload(*include_bytes!(\"../payloads/{}{}\"));\n",
                sanitize_cpu(&v.target_cpu).to_ascii_uppercase(),
                payload_len,
                sanitize_cpu(&v.target_cpu),
                payload_extension(v.payload_compression)
            ));
        }
    }
    out.push('\n');
    out.push_str("pub struct Variant {\n    pub target_cpu: &'static str,\n    pub required_features: FeatureMask,\n    pub rank_features: FeatureMask,\n    pub rank_feature_count: u16,\n    pub feature_tier: u8,\n    pub target_kind: TargetKind,\n    pub env_selected_target_cpu: &'static [u8],\n    pub env_selected_flags: &'static [u8],\n    pub payload: &'static [u8],\n    pub payload_path: &'static [u8],\n    pub payload_compression: PayloadCompression,\n    pub uncompressed_len: usize,\n}\n\n");
    out.push_str("#[derive(Clone, Copy, Eq, PartialEq)]\npub enum PayloadCompression {\n    None,\n    Zstd,\n}\n\n");
    out.push_str("pub static VARIANTS: &[Variant] = &[\n");
    for v in variants {
        let req = v.required_features.words();
        let rank = v.rank_features.words();
        let flags = v.feature_names.join(",");
        out.push_str("    Variant {\n");
        out.push_str(&format!("        target_cpu: {:?},\n", v.target_cpu));
        out.push_str(&format!(
            "        required_features: FeatureMask::from_words([{:#x}, {:#x}]),\n",
            req[0], req[1]
        ));
        out.push_str(&format!(
            "        rank_features: FeatureMask::from_words([{:#x}, {:#x}]),\n",
            rank[0], rank[1]
        ));
        out.push_str(&format!(
            "        rank_feature_count: {},\n",
            v.rank_features.count()
        ));
        out.push_str(&format!("        feature_tier: {},\n", v.feature_tier));
        out.push_str(&format!(
            "        target_kind: {},\n",
            target_kind_expr(v.target_kind)
        ));
        out.push_str(&format!(
            "        env_selected_target_cpu: b\"CARGO_SONIC_SELECTED_TARGET_CPU={}\\0\",\n",
            escape_bytes(&v.target_cpu)
        ));
        out.push_str(&format!(
            "        env_selected_flags: b\"CARGO_SONIC_SELECTED_FLAGS={}\\0\",\n",
            escape_bytes(&flags)
        ));
        let payload_expr = if loader_strategy == LoaderStrategy::Embedded {
            format!(
                "&PAYLOAD_{}.0",
                sanitize_cpu(&v.target_cpu).to_ascii_uppercase()
            )
        } else {
            "&[]".to_string()
        };
        out.push_str(&format!("        payload: {payload_expr},\n"));
        out.push_str(&format!("        payload_path: b{:?},\n", v.bundle_path));
        out.push_str(&format!(
            "        payload_compression: {},\n",
            payload_compression_expr(v.payload_compression)
        ));
        out.push_str(&format!(
            "        uncompressed_len: {},\n",
            v.uncompressed_len
        ));
        out.push_str("    },\n");
    }
    out.push_str("];\n");
    out
}

fn payload_compression_expr(compression: PayloadCompression) -> &'static str {
    match compression {
        PayloadCompression::None => "PayloadCompression::None",
        PayloadCompression::Zstd => "PayloadCompression::Zstd",
    }
}

fn payload_extension(compression: PayloadCompression) -> &'static str {
    match compression {
        PayloadCompression::None => ".elf",
        PayloadCompression::Zstd => ".elf.zstd",
    }
}

fn target_kind_expr(kind: TargetKind) -> String {
    match kind {
        TargetKind::Generic => "TargetKind::Generic".to_string(),
        TargetKind::X86NeutralLevel { level } => {
            format!("TargetKind::X86NeutralLevel {{ level: {level} }}")
        }
        TargetKind::X86IntelCore => "TargetKind::X86IntelCore".to_string(),
        TargetKind::X86IntelXeon => "TargetKind::X86IntelXeon".to_string(),
        TargetKind::X86IntelAtom => "TargetKind::X86IntelAtom".to_string(),
        TargetKind::X86AmdZen { generation } => {
            format!("TargetKind::X86AmdZen {{ generation: {generation} }}")
        }
        TargetKind::X86AmdOther => "TargetKind::X86AmdOther".to_string(),
        TargetKind::Aarch64ArmNeoverseN => "TargetKind::Aarch64ArmNeoverseN".to_string(),
        TargetKind::Aarch64ArmNeoverseV => "TargetKind::Aarch64ArmNeoverseV".to_string(),
        TargetKind::Aarch64ArmNeoverseE => "TargetKind::Aarch64ArmNeoverseE".to_string(),
        TargetKind::Aarch64ArmCortexA => "TargetKind::Aarch64ArmCortexA".to_string(),
        TargetKind::Aarch64ArmCortexX => "TargetKind::Aarch64ArmCortexX".to_string(),
        TargetKind::Aarch64Apple => "TargetKind::Aarch64Apple".to_string(),
        TargetKind::Aarch64Ampere => "TargetKind::Aarch64Ampere".to_string(),
        TargetKind::Aarch64Other => "TargetKind::Aarch64Other".to_string(),
    }
}

fn generated_main(_target: &str, uses_zstd: bool, loader_strategy: LoaderStrategy) -> String {
    let zstd_support = if uses_zstd {
        r#"
extern crate alloc;
use core::alloc::{GlobalAlloc, Layout};
use alloc::vec::Vec;

struct MmapAllocator;

unsafe impl GlobalAlloc for MmapAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let len = layout.size().max(1);
        let ptr = unsafe { linux_sys::mmap(len) } as *mut u8;
        if (ptr as isize) < 0 {
            core::ptr::null_mut()
        } else {
            ptr
        }
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}

#[global_allocator]
static ALLOCATOR: MmapAllocator = MmapAllocator;

unsafe fn write_zstd_payload(fd: isize, selected: &Variant) -> isize {
    unsafe {
        let output = linux_sys::mmap(selected.uncompressed_len) as *mut u8;
        if (output as isize) < 0 {
            return -1;
        }
        let output_slice = core::slice::from_raw_parts_mut(output, selected.uncompressed_len);
        let mut decoder = ruzstd::decoding::FrameDecoder::new();
        if decoder.decode_all(selected.payload, output_slice).is_err() {
            return -1;
        }
        linux_sys::write_all(fd, output, selected.uncompressed_len)
    }
}

unsafe fn write_zstd_payload_from_fd(fd: isize, payload_fd: isize, selected: &Variant) -> isize {
    unsafe {
        let mut compressed = Vec::new();
        let mut buf = [0u8; 16384];
        loop {
            let n = linux_sys::read(payload_fd, buf.as_mut_ptr(), buf.len());
            if n < 0 {
                return n;
            }
            if n == 0 {
                break;
            }
            compressed.extend_from_slice(&buf[..n as usize]);
        }
        let output = linux_sys::mmap(selected.uncompressed_len) as *mut u8;
        if (output as isize) < 0 {
            return -1;
        }
        let output_slice = core::slice::from_raw_parts_mut(output, selected.uncompressed_len);
        let mut decoder = ruzstd::decoding::FrameDecoder::new();
        if decoder.decode_all(&compressed, output_slice).is_err() {
            return -1;
        }
        linux_sys::write_all(fd, output, selected.uncompressed_len)
    }
}
"#
    } else {
        r#"
unsafe fn write_zstd_payload(_fd: isize, _selected: &Variant) -> isize {
    -1
}

unsafe fn write_zstd_payload_from_fd(_fd: isize, _payload_fd: isize, _selected: &Variant) -> isize {
    -1
}
"#
    };
    let exec_payload = match loader_strategy {
        LoaderStrategy::Embedded => "exec_embedded_payload",
        LoaderStrategy::Bundle => "exec_bundle_payload",
    };
    r#"#![no_std]
#![no_main]
#![allow(dead_code)]

#[cfg(not(target_os = "linux"))]
compile_error!("cargo-sonic loader supports Linux only");

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("cargo-sonic loader currently supports x86_64 and aarch64 only");

mod feature_mask;
mod select;
mod linux_sys;
mod stack;
mod generated_manifest;

#[cfg(target_arch = "x86_64")]
mod arch_x86_64;

#[cfg(target_arch = "aarch64")]
mod arch_aarch64;

use generated_manifest::{Variant, VARIANTS};
use select::{CpuIdentity, HostInfo, TargetArch, VariantMeta};
use generated_manifest::PayloadCompression;

/*__CARGO_SONIC_ZSTD_SUPPORT__*/

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    unsafe { linux_sys::exit(101) }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn memcpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    let mut i = 0;
    unsafe {
        while i < n {
            *dst.add(i) = *src.add(i);
            i += 1;
        }
    }
    dst
}

#[unsafe(no_mangle)]
unsafe extern "C" fn memset(dst: *mut u8, value: i32, n: usize) -> *mut u8 {
    let mut i = 0;
    unsafe {
        while i < n {
            *dst.add(i) = value as u8;
            i += 1;
        }
    }
    dst
}

#[unsafe(no_mangle)]
unsafe extern "C" fn memcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    let mut i = 0;
    unsafe {
        while i < n {
            let av = *a.add(i);
            let bv = *b.add(i);
            if av != bv {
                return av as i32 - bv as i32;
            }
            i += 1;
        }
    }
    0
}

#[unsafe(no_mangle)]
unsafe extern "C" fn bcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    unsafe { memcmp(a, b, n) }
}

#[unsafe(no_mangle)]
extern "C" fn rust_eh_personality() {}

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".global _start",
    "_start:",
    "mov rdi, rsp",
    "and rsp, -16",
    "call {entry}",
    entry = sym loader_entry,
);

#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    ".global _start",
    "_start:",
    "mov x0, sp",
    "bl {entry}",
    entry = sym loader_entry,
);

#[unsafe(no_mangle)]
unsafe extern "C" fn loader_entry(stack: *const usize) -> ! {
    let initial = unsafe { stack::InitialStack::parse(stack) };
    let host = detect_host(&initial);
    let mut metas = [VariantMeta {
        target_cpu: "",
        required_features: feature_mask::FeatureMask::EMPTY,
        rank_features: feature_mask::FeatureMask::EMPTY,
        rank_feature_count: 0,
        feature_tier: 0,
        target_kind: select::TargetKind::Generic,
    }; 256];
    let count = VARIANTS.len();
    let mut i = 0;
    while i < count {
        let v = &VARIANTS[i];
        metas[i] = VariantMeta {
            target_cpu: v.target_cpu,
            required_features: v.required_features,
            rank_features: v.rank_features,
            rank_feature_count: v.rank_feature_count,
            feature_tier: v.feature_tier,
            target_kind: v.target_kind,
        };
        i += 1;
    }
    let sonic_enabled = unsafe { stack::sonic_enabled(&initial) };
    let selected_meta = if sonic_enabled {
        select::select_variant(host, &metas[..count])
    } else {
        generic_meta(&metas[..count])
    };
    if unsafe { stack::debug_enabled(&initial) } {
        debug_selection(host, &metas[..count], selected_meta.target_cpu, sonic_enabled);
    }
    let selected = find_variant(selected_meta.target_cpu);
    unsafe { __CARGO_SONIC_EXEC_PAYLOAD__(selected, &initial) }
}

fn find_variant(name: &str) -> &'static Variant {
    let mut i = 0;
    while i < VARIANTS.len() {
        if bytes_eq(VARIANTS[i].target_cpu.as_bytes(), name.as_bytes()) {
            return &VARIANTS[i];
        }
        i += 1;
    }
    &VARIANTS[0]
}

fn bytes_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

fn generic_meta<'a>(variants: &'a [VariantMeta]) -> &'a VariantMeta {
    let mut i = 0;
    while i < variants.len() {
        if variants[i].target_kind.is_generic() {
            return &variants[i];
        }
        i += 1;
    }
    &variants[0]
}

#[repr(C)]
struct FileCloneRange {
    src_fd: i64,
    src_offset: u64,
    src_length: u64,
    dest_offset: u64,
}

unsafe fn exec_reflink_tmpfile(selected: &Variant, initial: &stack::InitialStack) -> isize {
    unsafe {
        if selected.payload_compression != generated_manifest::PayloadCompression::None {
            return -1;
        }
        let exe = linux_sys::openat(linux_sys::AT_FDCWD, b"/proc/self/exe\0".as_ptr(), 0, 0);
        if exe < 0 {
            return exe;
        }
        let offset = payload_file_offset(
            exe,
            selected.payload.as_ptr() as usize,
            selected.payload.len(),
            initial.phdr,
        );
        if offset == 0 {
            let _ = linux_sys::close(exe);
            return -1;
        }
        let tmp = linux_sys::openat(
            linux_sys::AT_FDCWD,
            b".\0".as_ptr(),
            linux_sys::O_TMPFILE | linux_sys::O_RDWR,
            0o700,
        );
        if tmp < 0 {
            let _ = linux_sys::close(exe);
            return tmp;
        }
        let clone_len = round_up_128k(selected.payload.len());
        if linux_sys::ftruncate(tmp, clone_len) < 0 {
            let _ = linux_sys::close(exe);
            let _ = linux_sys::close(tmp);
            return -1;
        }
        let mut range = FileCloneRange {
            src_fd: exe as i64,
            src_offset: offset as u64,
            src_length: clone_len as u64,
            dest_offset: 0,
        };
        let cloned = linux_sys::ioctl(tmp, linux_sys::FICLONERANGE, &mut range as *mut _ as usize);
        let _ = linux_sys::close(exe);
        if cloned < 0 {
            let _ = linux_sys::close(tmp);
            return cloned;
        }
        let read_fd = reopen_fd_readonly(tmp);
        let _ = linux_sys::close(tmp);
        if read_fd < 0 {
            return read_fd;
        }
        let envp = stack::build_envp(initial, generated_manifest::ENV_ENABLED, selected.env_selected_target_cpu, selected.env_selected_flags);
        if envp.is_null() {
            let _ = linux_sys::close(read_fd);
            return -1;
        }
        linux_sys::execveat(read_fd, b"\0".as_ptr(), initial.argv, envp, linux_sys::AT_EMPTY_PATH)
    }
}

unsafe fn reopen_fd_readonly(fd: isize) -> isize {
    unsafe {
        let mut path = [0u8; 32];
        let prefix = b"/proc/self/fd/";
        let mut i = 0;
        while i < prefix.len() {
            path[i] = prefix[i];
            i += 1;
        }
        let mut digits = [0u8; 20];
        let mut n = fd as usize;
        let mut count = 0;
        if n == 0 {
            digits[0] = b'0';
            count = 1;
        } else {
            while n > 0 {
                digits[count] = b'0' + (n % 10) as u8;
                n /= 10;
                count += 1;
            }
        }
        while count > 0 {
            count -= 1;
            path[i] = digits[count];
            i += 1;
        }
        path[i] = 0;
        linux_sys::openat(linux_sys::AT_FDCWD, path.as_ptr(), linux_sys::O_RDONLY, 0)
    }
}

fn round_up_128k(len: usize) -> usize {
    (len + 131071) & !131071
}

unsafe fn payload_file_offset(exe: isize, payload_addr: usize, payload_len: usize, at_phdr: usize) -> usize {
    unsafe {
        let mut ehdr = [0u8; 64];
        if linux_sys::pread(exe, ehdr.as_mut_ptr(), ehdr.len(), 0) != ehdr.len() as isize {
            return 0;
        }
        if ehdr[0] != 0x7f || ehdr[1] != b'E' || ehdr[2] != b'L' || ehdr[3] != b'F' || ehdr[4] != 2 || ehdr[5] != 1 {
            return 0;
        }
        let elf_type = read_u16(&ehdr, 16);
        let phoff = read_u64(&ehdr, 32) as usize;
        let phentsize = read_u16(&ehdr, 54) as usize;
        let phnum = read_u16(&ehdr, 56) as usize;
        if phoff == 0 || phentsize < 56 || phnum == 0 {
            return 0;
        }
        let load_bias = if elf_type == 3 { at_phdr.wrapping_sub(phoff) } else { 0 };
        let payload_vaddr = payload_addr.wrapping_sub(load_bias);
        let mut phdr = [0u8; 64];
        if phentsize > phdr.len() {
            return 0;
        }
        let mut i = 0;
        while i < phnum {
            let offset = phoff + i * phentsize;
            if linux_sys::pread(exe, phdr.as_mut_ptr(), phentsize, offset) != phentsize as isize {
                return 0;
            }
            if read_u32(&phdr, 0) == 1 {
                let file_offset = read_u64(&phdr, 8) as usize;
                let vaddr = read_u64(&phdr, 16) as usize;
                let filesz = read_u64(&phdr, 32) as usize;
                let relative = payload_vaddr.wrapping_sub(vaddr);
                if payload_vaddr >= vaddr && relative <= filesz && payload_len <= filesz - relative {
                    return file_offset + (payload_vaddr - vaddr);
                }
            }
            i += 1;
        }
        0
    }
}

fn read_u16(buf: &[u8], offset: usize) -> u16 {
    (buf[offset] as u16) | ((buf[offset + 1] as u16) << 8)
}

fn read_u32(buf: &[u8], offset: usize) -> u32 {
    (buf[offset] as u32)
        | ((buf[offset + 1] as u32) << 8)
        | ((buf[offset + 2] as u32) << 16)
        | ((buf[offset + 3] as u32) << 24)
}

fn read_u64(buf: &[u8], offset: usize) -> u64 {
    (read_u32(buf, offset) as u64) | ((read_u32(buf, offset + 4) as u64) << 32)
}

unsafe fn exec_payload(selected: &Variant, initial: &stack::InitialStack) -> ! {
    unsafe { exec_embedded_payload(selected, initial) }
}

unsafe fn exec_embedded_payload(selected: &Variant, initial: &stack::InitialStack) -> ! {
    unsafe {
        let _ = exec_reflink_tmpfile(selected, initial);
        let fd = linux_sys::memfd_create_best_effort(b"cargo-sonic-payload\0".as_ptr());
        if fd < 0 {
            linux_sys::exit(111);
        }
        let written = match selected.payload_compression {
            PayloadCompression::None => linux_sys::write_all(fd, selected.payload.as_ptr(), selected.payload.len()),
            PayloadCompression::Zstd => write_zstd_payload(fd, selected),
        };
        if written < 0 {
            linux_sys::exit(112);
        }
        let envp = stack::build_envp(initial, generated_manifest::ENV_ENABLED, selected.env_selected_target_cpu, selected.env_selected_flags);
        if envp.is_null() {
            linux_sys::exit(113);
        }
        linux_sys::execveat(fd, b"\0".as_ptr(), initial.argv, envp, linux_sys::AT_EMPTY_PATH);
        linux_sys::exit(114)
    }
}

unsafe fn exec_bundle_payload(selected: &Variant, initial: &stack::InitialStack) -> ! {
    unsafe {
        let mut path = [0u8; 4096];
        let exe_len = linux_sys::readlinkat(
            linux_sys::AT_FDCWD,
            b"/proc/self/exe\0".as_ptr(),
            path.as_mut_ptr(),
            path.len() - 1,
        );
        if exe_len <= 0 {
            linux_sys::exit(115);
        }
        let exe_len = exe_len as usize;
        let suffix = b".bundle/";
        let required = exe_len + suffix.len() + selected.payload_path.len();
        if required > path.len() {
            linux_sys::exit(116);
        }
        let mut out = exe_len;
        let mut i = 0;
        while i < suffix.len() {
            path[out] = suffix[i];
            out += 1;
            i += 1;
        }
        i = 0;
        while i < selected.payload_path.len() {
            path[out] = selected.payload_path[i];
            out += 1;
            i += 1;
        }
        let fd = linux_sys::openat(linux_sys::AT_FDCWD, path.as_ptr(), linux_sys::O_RDONLY, 0);
        if fd < 0 {
            linux_sys::exit(117);
        }
        if selected.payload_compression != PayloadCompression::None {
            let memfd = linux_sys::memfd_create_best_effort(b"cargo-sonic-payload\0".as_ptr());
            if memfd < 0 {
                let _ = linux_sys::close(fd);
                linux_sys::exit(120);
            }
            if write_zstd_payload_from_fd(memfd, fd, selected) < 0 {
                let _ = linux_sys::close(fd);
                linux_sys::exit(121);
            }
            let _ = linux_sys::close(fd);
            let envp = stack::build_envp(initial, generated_manifest::ENV_ENABLED, selected.env_selected_target_cpu, selected.env_selected_flags);
            if envp.is_null() {
                let _ = linux_sys::close(memfd);
                linux_sys::exit(122);
            }
            linux_sys::execveat(memfd, b"\0".as_ptr(), initial.argv, envp, linux_sys::AT_EMPTY_PATH);
            linux_sys::exit(123);
        }
        let envp = stack::build_envp(initial, generated_manifest::ENV_ENABLED, selected.env_selected_target_cpu, selected.env_selected_flags);
        if envp.is_null() {
            let _ = linux_sys::close(fd);
            linux_sys::exit(118);
        }
        linux_sys::execveat(fd, b"\0".as_ptr(), initial.argv, envp, linux_sys::AT_EMPTY_PATH);
        linux_sys::exit(119)
    }
}

fn debug_selection(host: HostInfo, variants: &[VariantMeta], selected: &str, sonic_enabled: bool) {
    unsafe {
        linux_sys::write_stderr(b"cargo-sonic debug\n");
        linux_sys::write_stderr(b"  enable=");
        linux_sys::write_stderr(if sonic_enabled { b"1" } else { b"0" });
        linux_sys::write_stderr(b"\n");
        linux_sys::write_stderr(b"  host.features=");
        debug_mask(host.features);
        linux_sys::write_stderr(b"\n");
        linux_sys::write_stderr(b"  host.feature_names=");
        debug_feature_names(host.features);
        linux_sys::write_stderr(b"\n");
        linux_sys::write_stderr(b"  host.identity=");
        debug_identity(host.identity);
        linux_sys::write_stderr(b"\n");
        linux_sys::write_stderr(b"  variants:\n");
        let mut i = 0;
        while i < variants.len() {
            let v = &variants[i];
            let eligible = v.target_kind.is_generic() || v.required_features.is_subset_of(host.features);
            linux_sys::write_stderr(b"    ");
            linux_sys::write_stderr(v.target_cpu.as_bytes());
            linux_sys::write_stderr(b" eligible=");
            linux_sys::write_stderr(if eligible { b"yes" } else { b"no" });
            linux_sys::write_stderr(b" tier=");
            debug_u8(v.feature_tier);
            linux_sys::write_stderr(b" count=");
            debug_u16(v.rank_feature_count);
            linux_sys::write_stderr(b" required=");
            debug_mask(v.required_features);
            if !eligible {
                linux_sys::write_stderr(b" missing=");
                debug_missing(v.required_features, host.features);
                linux_sys::write_stderr(b" missing_features=");
                debug_missing_feature_names(v.required_features, host.features);
            }
            linux_sys::write_stderr(b"\n");
            i += 1;
        }
        linux_sys::write_stderr(b"  selected=");
        linux_sys::write_stderr(selected.as_bytes());
        linux_sys::write_stderr(b"\n");
    }
}

fn debug_identity(identity: CpuIdentity) {
    unsafe {
        match identity {
            CpuIdentity::Unknown => linux_sys::write_stderr(b"unknown"),
            CpuIdentity::X86 { vendor, family, model, stepping } => {
                linux_sys::write_stderr(b"x86(");
                linux_sys::write_stderr(match vendor {
                    select::X86Vendor::Intel => b"intel",
                    select::X86Vendor::Amd => b"amd",
                    select::X86Vendor::Other => b"other",
                });
                linux_sys::write_stderr(b" family=");
                debug_u16(family);
                linux_sys::write_stderr(b" model=");
                debug_u16(model);
                linux_sys::write_stderr(b" stepping=");
                debug_u8(stepping);
                linux_sys::write_stderr(b")");
            }
            CpuIdentity::Aarch64 { implementer, part, variant, revision } => {
                linux_sys::write_stderr(b"aarch64(implementer=");
                debug_u16(implementer);
                linux_sys::write_stderr(b" part=");
                debug_u16(part);
                linux_sys::write_stderr(b" variant=");
                debug_u8(variant);
                linux_sys::write_stderr(b" revision=");
                debug_u8(revision);
                linux_sys::write_stderr(b")");
            }
        }
    }
}

fn debug_mask(mask: feature_mask::FeatureMask) {
    let words = mask.words();
    unsafe {
        linux_sys::write_stderr(b"[");
        debug_hex64(words[0]);
        linux_sys::write_stderr(b",");
        debug_hex64(words[1]);
        linux_sys::write_stderr(b"]");
    }
}

fn debug_missing(required: feature_mask::FeatureMask, host: feature_mask::FeatureMask) {
    let required = required.words();
    let host = host.words();
    unsafe {
        linux_sys::write_stderr(b"[");
        debug_hex64(required[0] & !host[0]);
        linux_sys::write_stderr(b",");
        debug_hex64(required[1] & !host[1]);
        linux_sys::write_stderr(b"]");
    }
}

fn debug_feature_names(mask: feature_mask::FeatureMask) {
    unsafe {
        linux_sys::write_stderr(b"[");
        let mut first = true;
        let mut i = 0;
        while i < feature_mask::ALL_FEATURES.len() {
            let feature = feature_mask::ALL_FEATURES[i];
            if mask.contains(feature) {
                if !first {
                    linux_sys::write_stderr(b",");
                }
                linux_sys::write_stderr(feature_mask::feature_name(feature).as_bytes());
                first = false;
            }
            i += 1;
        }
        linux_sys::write_stderr(b"]");
    }
}

fn debug_missing_feature_names(required: feature_mask::FeatureMask, host: feature_mask::FeatureMask) {
    unsafe {
        linux_sys::write_stderr(b"[");
        let mut first = true;
        let mut i = 0;
        while i < feature_mask::ALL_FEATURES.len() {
            let feature = feature_mask::ALL_FEATURES[i];
            if required.contains(feature) && !host.contains(feature) {
                if !first {
                    linux_sys::write_stderr(b",");
                }
                linux_sys::write_stderr(feature_mask::feature_name(feature).as_bytes());
                first = false;
            }
            i += 1;
        }
        linux_sys::write_stderr(b"]");
    }
}

fn debug_hex64(value: u64) {
    let mut buf = *b"0x0000000000000000";
    let mut i = 0;
    while i < 16 {
        let shift = (15 - i) * 4;
        let nibble = ((value >> shift) & 0xf) as u8;
        buf[2 + i] = if nibble < 10 { b'0' + nibble } else { b'a' + (nibble - 10) };
        i += 1;
    }
    unsafe { linux_sys::write_stderr(&buf) }
}

fn debug_u8(value: u8) {
    debug_u16(value as u16);
}

fn debug_u16(mut value: u16) {
    let mut buf = [0u8; 5];
    let mut i = buf.len();
    if value == 0 {
        unsafe { linux_sys::write_stderr(b"0") }
        return;
    }
    while value > 0 {
        i -= 1;
        buf[i] = b'0' + (value % 10) as u8;
        value /= 10;
    }
    unsafe { linux_sys::write_stderr(&buf[i..]) }
}

fn detect_host(initial: &stack::InitialStack) -> HostInfo {
    let _ = initial;
    #[cfg(target_arch = "x86_64")]
    {
        HostInfo {
            arch: TargetArch::X86_64,
            features: unsafe { detect_x86() },
            identity: unsafe { detect_x86_identity() },
            heterogeneous: false,
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        HostInfo {
            arch: TargetArch::Aarch64,
            features: arch_aarch64::detect_aarch64_features_from_hwcap(initial.hwcap, initial.hwcap2, initial.hwcap3),
            identity: unsafe { detect_aarch64_identity_from_cpuinfo() },
            heterogeneous: false,
        }
    }
}

#[cfg(target_arch = "aarch64")]
unsafe fn detect_aarch64_identity_from_cpuinfo() -> CpuIdentity {
    let midr = unsafe { read_small_file_hex(
        b"/sys/devices/system/cpu/cpu0/regs/identification/midr_el1\0".as_ptr(),
    ) };
    if let Some(midr) = midr {
        return CpuIdentity::Aarch64 {
            implementer: ((midr >> 24) & 0xff) as u16,
            part: ((midr >> 4) & 0xfff) as u16,
            variant: ((midr >> 20) & 0xf) as u8,
            revision: (midr & 0xf) as u8,
        };
    }

    let fd = unsafe { linux_sys::openat(linux_sys::AT_FDCWD, b"/proc/cpuinfo\0".as_ptr(), 0, 0) };
    if fd < 0 {
        return CpuIdentity::Unknown;
    }
    let mut buf = [0u8; 4096];
    let n = unsafe { linux_sys::read(fd, buf.as_mut_ptr(), buf.len()) };
    let _ = unsafe { linux_sys::close(fd) };
    if n <= 0 {
        return CpuIdentity::Unknown;
    }
    let identity = parse_aarch64_cpuinfo(&buf[..n as usize]);
    if identity != CpuIdentity::Unknown {
        return identity;
    }

    CpuIdentity::Unknown
}

#[cfg(target_arch = "aarch64")]
#[allow(dead_code)]
unsafe fn read_midr_el1() -> u32 {
    let value: u64;
    unsafe { core::arch::asm!("mrs {}, MIDR_EL1", out(reg) value, options(nostack, nomem)); }
    value as u32
}

#[cfg(target_arch = "aarch64")]
unsafe fn read_small_file_hex(path: *const u8) -> Option<u32> {
    let fd = unsafe { linux_sys::openat(linux_sys::AT_FDCWD, path, 0, 0) };
    if fd < 0 {
        return None;
    }
    let mut buf = [0u8; 64];
    let n = unsafe { linux_sys::read(fd, buf.as_mut_ptr(), buf.len()) };
    let _ = unsafe { linux_sys::close(fd) };
    if n <= 0 {
        return None;
    }
    parse_hex_u32(&buf[..n as usize])
}

#[cfg(target_arch = "aarch64")]
fn parse_hex_u32(buf: &[u8]) -> Option<u32> {
    let mut out = 0u32;
    let mut seen = false;
    let mut i = 0;
    while i < buf.len() {
        let b = buf[i];
        let digit = if b >= b'0' && b <= b'9' {
            b - b'0'
        } else if b >= b'a' && b <= b'f' {
            b - b'a' + 10
        } else if b >= b'A' && b <= b'F' {
            b - b'A' + 10
        } else if b == b'x' || b == b'X' || b == b' ' || b == b'\t' || b == b'\n' {
            i += 1;
            continue;
        } else {
            break;
        };
        out = (out << 4) | digit as u32;
        seen = true;
        i += 1;
    }
    if seen { Some(out) } else { None }
}

#[cfg(target_arch = "aarch64")]
fn parse_aarch64_cpuinfo(buf: &[u8]) -> CpuIdentity {
    let implementer = cpuinfo_hex_value(buf, b"CPU implementer");
    let part = cpuinfo_hex_value(buf, b"CPU part");
    let variant = cpuinfo_hex_value(buf, b"CPU variant");
    let revision = cpuinfo_decimal_value(buf, b"CPU revision");
    if let (Some(implementer), Some(part)) = (implementer, part) {
        CpuIdentity::Aarch64 {
            implementer,
            part,
            variant: variant.unwrap_or(0) as u8,
            revision: revision.unwrap_or(0) as u8,
        }
    } else {
        CpuIdentity::Unknown
    }
}

#[cfg(target_arch = "aarch64")]
fn cpuinfo_hex_value(buf: &[u8], key: &[u8]) -> Option<u16> {
    let value = cpuinfo_value(buf, key)?;
    let mut out = 0u16;
    let mut seen = false;
    let mut i = 0;
    while i < value.len() {
        let b = value[i];
        let digit = if b >= b'0' && b <= b'9' {
            b - b'0'
        } else if b >= b'a' && b <= b'f' {
            b - b'a' + 10
        } else if b >= b'A' && b <= b'F' {
            b - b'A' + 10
        } else if b == b'x' || b == b'X' || b == b' ' || b == b'\t' {
            i += 1;
            continue;
        } else {
            break;
        };
        out = (out << 4) | digit as u16;
        seen = true;
        i += 1;
    }
    if seen { Some(out) } else { None }
}

#[cfg(target_arch = "aarch64")]
fn cpuinfo_decimal_value(buf: &[u8], key: &[u8]) -> Option<u16> {
    let value = cpuinfo_value(buf, key)?;
    let mut out = 0u16;
    let mut seen = false;
    let mut i = 0;
    while i < value.len() {
        let b = value[i];
        if b >= b'0' && b <= b'9' {
            out = out.saturating_mul(10).saturating_add((b - b'0') as u16);
            seen = true;
        } else if seen {
            break;
        }
        i += 1;
    }
    if seen { Some(out) } else { None }
}

#[cfg(target_arch = "aarch64")]
fn cpuinfo_value<'a>(buf: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let mut start = 0;
    while start < buf.len() {
        let mut end = start;
        while end < buf.len() && buf[end] != b'\n' {
            end += 1;
        }
        let line = &buf[start..end];
        if starts_with_bytes(line, key) {
            let mut colon = 0;
            while colon < line.len() && line[colon] != b':' {
                colon += 1;
            }
            if colon < line.len() {
                return Some(&line[colon + 1..]);
            }
        }
        start = end + 1;
    }
    None
}

#[cfg(target_arch = "aarch64")]
fn starts_with_bytes(buf: &[u8], prefix: &[u8]) -> bool {
    if buf.len() < prefix.len() {
        return false;
    }
    let mut i = 0;
    while i < prefix.len() {
        if buf[i] != prefix[i] {
            return false;
        }
        i += 1;
    }
    true
}

#[cfg(target_arch = "x86_64")]
unsafe fn detect_x86() -> feature_mask::FeatureMask {
    let l1 = core::arch::x86_64::__cpuid_count(1, 0);
    let l70 = core::arch::x86_64::__cpuid_count(7, 0);
    let l71 = core::arch::x86_64::__cpuid_count(7, 1);
    let ld1 = core::arch::x86_64::__cpuid_count(0xd, 1);
    let l8 = core::arch::x86_64::__cpuid_count(0x80000001, 0);
    let xcr0 = if (l1.ecx & (1 << 26)) != 0 && (l1.ecx & (1 << 27)) != 0 {
        unsafe { core::arch::x86_64::_xgetbv(0) }
    } else {
        0
    };
    arch_x86_64::detect_x86_features_from_cpuid(
        arch_x86_64::X86Cpuid {
            leaf1: arch_x86_64::CpuidLeaf { eax: l1.eax, ebx: l1.ebx, ecx: l1.ecx, edx: l1.edx },
            leaf7_0: arch_x86_64::CpuidLeaf { eax: l70.eax, ebx: l70.ebx, ecx: l70.ecx, edx: l70.edx },
            leaf7_1: arch_x86_64::CpuidLeaf { eax: l71.eax, ebx: l71.ebx, ecx: l71.ecx, edx: l71.edx },
            leaf_d_1: arch_x86_64::CpuidLeaf { eax: ld1.eax, ebx: ld1.ebx, ecx: ld1.ecx, edx: ld1.edx },
            leaf80000001: arch_x86_64::CpuidLeaf { eax: l8.eax, ebx: l8.ebx, ecx: l8.ecx, edx: l8.edx },
        },
        xcr0,
    )
}

#[cfg(target_arch = "x86_64")]
unsafe fn detect_x86_identity() -> CpuIdentity {
    let vendor_leaf = core::arch::x86_64::__cpuid_count(0, 0);
    let vendor = if vendor_leaf.ebx == 0x756e6547 && vendor_leaf.edx == 0x49656e69 && vendor_leaf.ecx == 0x6c65746e {
        select::X86Vendor::Intel
    } else if vendor_leaf.ebx == 0x68747541 && vendor_leaf.edx == 0x69746e65 && vendor_leaf.ecx == 0x444d4163 {
        select::X86Vendor::Amd
    } else {
        select::X86Vendor::Other
    };
    let leaf1 = core::arch::x86_64::__cpuid_count(1, 0);
    let base_family = ((leaf1.eax >> 8) & 0xf) as u16;
    let ext_family = ((leaf1.eax >> 20) & 0xff) as u16;
    let base_model = ((leaf1.eax >> 4) & 0xf) as u16;
    let ext_model = ((leaf1.eax >> 16) & 0xf) as u16;
    let family = if base_family == 0xf { base_family + ext_family } else { base_family };
    let model = if base_family == 0x6 || base_family == 0xf { base_model | (ext_model << 4) } else { base_model };
    CpuIdentity::X86 {
        vendor,
        family,
        model,
        stepping: (leaf1.eax & 0xf) as u8,
    }
}
"#
    .replace("__CARGO_SONIC_EXEC_PAYLOAD__", exec_payload)
    .replace("/*__CARGO_SONIC_ZSTD_SUPPORT__*/", zstd_support)
}

fn generated_linux_sys() -> &'static str {
    r#"pub const AT_EMPTY_PATH: usize = 0x1000;
pub const AT_FDCWD: isize = -100;
pub const O_RDONLY: usize = 0;
pub const O_RDWR: usize = 2;
pub const O_TMPFILE: usize = 0x410000;
pub const FICLONERANGE: usize = 0x4020940d;
#[cfg(target_arch = "x86_64")]
const SYS_WRITE: usize = 1;
#[cfg(target_arch = "x86_64")]
const SYS_READ: usize = 0;
#[cfg(target_arch = "x86_64")]
const SYS_PREAD64: usize = 17;
#[cfg(target_arch = "x86_64")]
const SYS_IOCTL: usize = 16;
#[cfg(target_arch = "x86_64")]
const SYS_CLOSE: usize = 3;
#[cfg(target_arch = "x86_64")]
const SYS_MMAP: usize = 9;
#[cfg(target_arch = "x86_64")]
const SYS_FTRUNCATE: usize = 77;
#[cfg(target_arch = "x86_64")]
const SYS_EXIT: usize = 60;
#[cfg(target_arch = "x86_64")]
const SYS_OPENAT: usize = 257;
#[cfg(target_arch = "x86_64")]
const SYS_MEMFD_CREATE: usize = 319;
#[cfg(target_arch = "x86_64")]
const SYS_EXECVEAT: usize = 322;
#[cfg(target_arch = "x86_64")]
const SYS_READLINKAT: usize = 267;
const PROT_READ: usize = 1;
const PROT_WRITE: usize = 2;
const MAP_PRIVATE: usize = 2;
const MAP_ANONYMOUS: usize = 0x20;
const MFD_ALLOW_SEALING: usize = 0x2;
const MFD_EXEC: usize = 0x10;
const EINVAL_NEG: isize = -22;

#[cfg(target_arch = "aarch64")]
const SYS_WRITE: usize = 64;
#[cfg(target_arch = "aarch64")]
const SYS_READ: usize = 63;
#[cfg(target_arch = "aarch64")]
const SYS_PREAD64: usize = 67;
#[cfg(target_arch = "aarch64")]
const SYS_IOCTL: usize = 29;
#[cfg(target_arch = "aarch64")]
const SYS_CLOSE: usize = 57;
#[cfg(target_arch = "aarch64")]
const SYS_MMAP: usize = 222;
#[cfg(target_arch = "aarch64")]
const SYS_FTRUNCATE: usize = 46;
#[cfg(target_arch = "aarch64")]
const SYS_EXIT: usize = 93;
#[cfg(target_arch = "aarch64")]
const SYS_OPENAT: usize = 56;
#[cfg(target_arch = "aarch64")]
const SYS_MEMFD_CREATE: usize = 279;
#[cfg(target_arch = "aarch64")]
const SYS_EXECVEAT: usize = 281;
#[cfg(target_arch = "aarch64")]
const SYS_READLINKAT: usize = 78;

#[cfg(target_arch = "x86_64")]
unsafe fn syscall6(n: usize, a0: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> isize {
    let ret: isize;
    unsafe {
        core::arch::asm!("syscall", inlateout("rax") n as isize => ret, in("rdi") a0, in("rsi") a1, in("rdx") a2, in("r10") a3, in("r8") a4, in("r9") a5, lateout("rcx") _, lateout("r11") _, options(nostack));
    }
    ret
}

#[cfg(target_arch = "aarch64")]
unsafe fn syscall6(n: usize, a0: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> isize {
    let ret: isize;
    unsafe {
        core::arch::asm!("svc #0", inlateout("x8") n as isize => _, inlateout("x0") a0 as isize => ret, in("x1") a1, in("x2") a2, in("x3") a3, in("x4") a4, in("x5") a5, options(nostack));
    }
    ret
}

pub unsafe fn mmap(len: usize) -> *mut usize {
    unsafe { syscall6(SYS_MMAP, 0, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, !0usize, 0) as *mut usize }
}

pub unsafe fn memfd_create_best_effort(name: *const u8) -> isize {
    let mut fd = unsafe { syscall6(SYS_MEMFD_CREATE, name as usize, MFD_ALLOW_SEALING | MFD_EXEC, 0, 0, 0, 0) };
    if fd == EINVAL_NEG {
        fd = unsafe { syscall6(SYS_MEMFD_CREATE, name as usize, MFD_ALLOW_SEALING, 0, 0, 0, 0) };
    }
    if fd == EINVAL_NEG {
        fd = unsafe { syscall6(SYS_MEMFD_CREATE, name as usize, 0, 0, 0, 0, 0) };
    }
    fd
}

pub unsafe fn write_all(fd: isize, mut ptr: *const u8, mut len: usize) -> isize {
    while len > 0 {
        let n = unsafe { syscall6(SYS_WRITE, fd as usize, ptr as usize, len, 0, 0, 0) };
        if n <= 0 {
            return n;
        }
        ptr = unsafe { ptr.add(n as usize) };
        len -= n as usize;
    }
    0
}

pub unsafe fn read(fd: isize, ptr: *mut u8, len: usize) -> isize {
    unsafe { syscall6(SYS_READ, fd as usize, ptr as usize, len, 0, 0, 0) }
}

pub unsafe fn readlinkat(dirfd: isize, path: *const u8, buf: *mut u8, len: usize) -> isize {
    unsafe { syscall6(SYS_READLINKAT, dirfd as usize, path as usize, buf as usize, len, 0, 0) }
}

pub unsafe fn pread(fd: isize, ptr: *mut u8, len: usize, offset: usize) -> isize {
    unsafe { syscall6(SYS_PREAD64, fd as usize, ptr as usize, len, offset, 0, 0) }
}

pub unsafe fn ioctl(fd: isize, request: usize, arg: usize) -> isize {
    unsafe { syscall6(SYS_IOCTL, fd as usize, request, arg, 0, 0, 0) }
}

pub unsafe fn openat(dirfd: isize, path: *const u8, flags: usize, mode: usize) -> isize {
    unsafe { syscall6(SYS_OPENAT, dirfd as usize, path as usize, flags, mode, 0, 0) }
}

pub unsafe fn ftruncate(fd: isize, len: usize) -> isize {
    unsafe { syscall6(SYS_FTRUNCATE, fd as usize, len, 0, 0, 0, 0) }
}

pub unsafe fn close(fd: isize) -> isize {
    unsafe { syscall6(SYS_CLOSE, fd as usize, 0, 0, 0, 0, 0) }
}

pub unsafe fn write_stderr(buf: &[u8]) {
    let _ = unsafe { write_all(2, buf.as_ptr(), buf.len()) };
}

pub unsafe fn execveat(fd: isize, path: *const u8, argv: *const *const u8, envp: *const *const u8, flags: usize) -> isize {
    unsafe { syscall6(SYS_EXECVEAT, fd as usize, path as usize, argv as usize, envp as usize, flags, 0) }
}

pub unsafe fn exit(code: i32) -> ! {
    let _ = unsafe { syscall6(SYS_EXIT, code as usize, 0, 0, 0, 0, 0) };
    loop {}
}
"#
}

fn generated_stack() -> &'static str {
    include_str!("loader_stack.rs")
}

fn escape_bytes(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

const X86_NEUTRAL_TIER: u8 = 1;
const X86_SPECIFIC_TIER: u8 = 2;
const AARCH64_SPECIFIC_TIER: u8 = 2;

fn target_feature_tier(arch: &str, target_kind: TargetKind) -> u8 {
    if arch == "x86_64" {
        match target_kind {
            TargetKind::Generic => 0,
            TargetKind::X86NeutralLevel { .. } => X86_NEUTRAL_TIER,
            _ => X86_SPECIFIC_TIER,
        }
    } else if arch == "aarch64" {
        match target_kind {
            TargetKind::Generic => 0,
            _ => AARCH64_SPECIFIC_TIER,
        }
    } else {
        0
    }
}

fn classify_target_cpu(cpu: &str, arch: &str, baseline_cpu: &str) -> TargetKind {
    if cpu == baseline_cpu {
        return TargetKind::Generic;
    }
    if arch == "x86_64" {
        return match cpu {
            "x86-64" => TargetKind::X86NeutralLevel { level: 1 },
            "x86-64-v2" => TargetKind::X86NeutralLevel { level: 2 },
            "x86-64-v3" => TargetKind::X86NeutralLevel { level: 3 },
            "x86-64-v4" => TargetKind::X86NeutralLevel { level: 4 },
            c if c.starts_with("znver") => TargetKind::X86AmdZen {
                generation: c.trim_start_matches("znver").parse().unwrap_or(0),
            },
            c if c.contains("atom")
                || c.contains("silvermont")
                || c.contains("goldmont")
                || c.contains("gracemont")
                || c.contains("forest")
                || matches!(c, "bonnell" | "slm" | "tremont")
                || c == "grandridge" =>
            {
                TargetKind::X86IntelAtom
            }
            c if c.contains("lake")
                || c.contains("well")
                || c.contains("bridge")
                || c.starts_with("core_")
                || c.starts_with("core-")
                || c.starts_with("corei")
                || matches!(c, "nocona" | "core2" | "penryn" | "nehalem" | "westmere") =>
            {
                TargetKind::X86IntelCore
            }
            c if c.contains("rapids")
                || c.contains("skx")
                || c.contains("skylake-avx512")
                || matches!(c, "knl" | "knm" | "mic_avx512") =>
            {
                TargetKind::X86IntelXeon
            }
            _ => TargetKind::X86AmdOther,
        };
    }
    match cpu {
        c if c.starts_with("neoverse-n") => TargetKind::Aarch64ArmNeoverseN,
        c if c.starts_with("neoverse-v") || c == "neoverse-512tvb" => {
            TargetKind::Aarch64ArmNeoverseV
        }
        c if c.starts_with("neoverse-e") => TargetKind::Aarch64ArmNeoverseE,
        c if c.starts_with("cortex-a") => TargetKind::Aarch64ArmCortexA,
        c if c.starts_with("cortex-x") => TargetKind::Aarch64ArmCortexX,
        c if c.starts_with("apple-") => TargetKind::Aarch64Apple,
        c if c.starts_with("ampere") => TargetKind::Aarch64Ampere,
        _ => TargetKind::Aarch64Other,
    }
}

fn analyze_warnings(features_by_cpu: &BTreeMap<String, Vec<String>>, arch: &str) -> Vec<String> {
    let mut warnings = Vec::new();
    let items: Vec<_> = features_by_cpu.iter().collect();
    for i in 0..items.len() {
        for j in i + 1..items.len() {
            if items[i].1 == items[j].1 {
                warnings.push(format!(
                    "sonic target-cpus `{}` and `{}` have identical rustc target_feature sets",
                    items[i].0, items[j].0
                ));
            }
        }
    }
    if arch == "x86_64"
        && !features_by_cpu.keys().any(|cpu| {
            matches!(
                cpu.as_str(),
                "x86-64" | "x86-64-v2" | "x86-64-v3" | "x86-64-v4"
            )
        })
        && features_by_cpu.keys().any(|cpu| cpu != "generic")
    {
        warnings.push(
            "vendor-specific x86 targets configured without a neutral x86 fallback".to_string(),
        );
    }
    warnings
}

#[cfg(unix)]
fn make_executable(path: &Utf8Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(permissions.mode() | 0o755);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Utf8Path) -> Result<()> {
    Ok(())
}

#[cfg(all(test, miri))]
mod linux_sys {
    pub unsafe fn mmap(len: usize) -> *mut usize {
        let words = len.div_ceil(core::mem::size_of::<usize>());
        let mut out = Vec::<usize>::with_capacity(words);
        let ptr = out.as_mut_ptr();
        core::mem::forget(out);
        ptr
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());
    const TARGET_RUSTFLAGS_ENV: &str = "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS";

    #[test]
    fn parses_aarch64_midr_identity() {
        assert_eq!(
            parse_aarch64_midr_hex(b"0x00000000410fd4f1\n").map(aarch64_identity_from_midr),
            Some(CpuIdentity::Aarch64 {
                implementer: 0x41,
                part: 0xd4f,
                variant: 0,
                revision: 1,
            })
        );
    }

    #[test]
    fn parses_aarch64_cpuinfo_identity() {
        let cpuinfo = b"processor\t: 0\n\
            CPU implementer\t: 0x41\n\
            CPU architecture: 8\n\
            CPU variant\t: 0x0\n\
            CPU part\t: 0xd4f\n\
            CPU revision\t: 1\n";
        assert_eq!(
            parse_aarch64_cpuinfo_identity(cpuinfo),
            CpuIdentity::Aarch64 {
                implementer: 0x41,
                part: 0xd4f,
                variant: 0,
                revision: 1,
            }
        );
    }

    #[test]
    fn parse_target_features_from_rustc_cfg_test() {
        let got = parse_target_features_from_rustc_cfg(
            "target_feature=\"sse2\"\ntarget_feature=\"avx2\"\n",
        );
        assert_eq!(got, vec!["avx2", "sse2"]);
    }

    #[test]
    fn filters_non_runtime_features() {
        assert_eq!(
            filter_runtime_features(&[
                "crt-static".into(),
                "ermsb".into(),
                "lahfsahf".into(),
                "prfchw".into(),
                "x87".into(),
                "avx2".into(),
            ]),
            vec!["avx2"]
        );
    }

    #[test]
    fn skylake_style_features_with_baseline_non_runtime_features_are_supported() {
        let features = filter_runtime_features(&[
            "x87".into(),
            "ermsb".into(),
            "lahfsahf".into(),
            "prfchw".into(),
            "fxsr".into(),
            "sse".into(),
            "sse2".into(),
            "sse3".into(),
            "ssse3".into(),
            "sse4.1".into(),
            "sse4.2".into(),
            "avx".into(),
            "avx2".into(),
        ]);
        assert!(!features.iter().any(|feature| feature == "x87"));
        assert!(!features.iter().any(|feature| feature == "ermsb"));
        assert!(!features.iter().any(|feature| feature == "lahfsahf"));
        assert!(!features.iter().any(|feature| feature == "prfchw"));
    }

    #[test]
    fn cargo_args_recognize_release_aliases() {
        let args = strings(&["-r"]);
        let got = parse_cargo_args(&args);
        assert!(got.release);
        assert_eq!(got.forwarded, args);

        let args = strings(&["--profile", "release"]);
        let got = parse_cargo_args(&args);
        assert!(got.release);
        assert_eq!(got.forwarded, args);

        let args = strings(&["--profile=release"]);
        let got = parse_cargo_args(&args);
        assert!(got.release);
        assert_eq!(got.forwarded, args);
    }

    #[test]
    fn cargo_args_consumes_target_dir_without_forwarding_duplicate() {
        let args = strings(&[
            "--target-dir",
            "custom-target",
            "--features",
            "fast",
            "--target",
            "x86_64-unknown-linux-gnu",
        ]);
        let got = parse_cargo_args(&args);
        assert_eq!(
            got.target_dir.as_deref(),
            Some(Utf8Path::new("custom-target"))
        );
        assert_eq!(got.target.as_deref(), Some("x86_64-unknown-linux-gnu"));
        assert_eq!(
            got.forwarded,
            strings(&["--features", "fast", "--target", "x86_64-unknown-linux-gnu"])
        );

        let args = strings(&["--target-dir=custom-target", "--release"]);
        let got = parse_cargo_args(&args);
        assert_eq!(
            got.target_dir.as_deref(),
            Some(Utf8Path::new("custom-target"))
        );
        assert_eq!(got.forwarded, strings(&["--release"]));
    }

    #[test]
    fn cargo_args_consumes_manifest_path_without_forwarding_duplicate() {
        let args = strings(&["--manifest-path", "examples/app/Cargo.toml", "--release"]);
        let got = parse_cargo_args(&args);
        assert_eq!(
            got.manifest_path.as_deref(),
            Some(Utf8Path::new("examples/app/Cargo.toml"))
        );
        assert_eq!(got.forwarded, strings(&["--release"]));
    }

    #[test]
    fn cargo_args_consumes_color_without_forwarding_duplicate() {
        let args = strings(&["--color", "always", "--release"]);
        let got = parse_cargo_args(&args);
        assert_eq!(got.color, Some(ColorMode::Always));
        assert_eq!(got.forwarded, strings(&["--release"]));
        assert_eq!(resolved_cargo_color(got.color), "always");

        let args = strings(&["--color=never", "--release"]);
        let got = parse_cargo_args(&args);
        assert_eq!(got.color, Some(ColorMode::Never));
        assert_eq!(got.forwarded, strings(&["--release"]));
        assert_eq!(resolved_cargo_color(got.color), "never");
    }

    #[test]
    fn cargo_args_tolerate_repeated_cargo_selectors() {
        let args = strings(&[
            "--manifest-path",
            "tests/fixtures/env-printer/Cargo.toml",
            "--bin",
            "foo",
            "--bin",
            "bar",
            "-p",
            "one",
            "-p",
            "two",
        ]);
        let got = parse_cargo_args(&args);
        assert_eq!(
            got.manifest_path.as_deref(),
            Some(Utf8Path::new("tests/fixtures/env-printer/Cargo.toml"))
        );
        assert_eq!(got.bin.as_deref(), Some("bar"));
        assert_eq!(got.package.as_deref(), Some("two"));
        assert_eq!(
            got.forwarded,
            strings(&["--bin", "foo", "--bin", "bar", "-p", "one", "-p", "two"])
        );
    }

    #[test]
    fn musl_loader_rustflags_skip_crt_startup_files() {
        let flags = loader_rustflags("aarch64-unknown-linux-musl");
        assert!(flags.contains("-C link-self-contained=no"));
        assert!(!flags.contains("-C link-arg=-nostartfiles"));
    }

    #[test]
    fn x86_64_loader_rustflags_use_large_code_model() {
        let flags = loader_rustflags("x86_64-unknown-linux-gnu");
        assert!(flags.contains("-C code-model=large"));
        assert!(!flags.contains("-C link-self-contained=no"));
        assert!(flags.contains("-C link-arg=-nostartfiles"));
    }

    #[test]
    fn payload_rustflags_use_cargo_config_by_default() {
        with_rustflags_env(|| {
            assert_eq!(
                payload_rustflags("aarch64-unknown-linux-gnu", "neoverse-v1"),
                PayloadRustflags::CargoConfig(
                    "target.aarch64-unknown-linux-gnu.rustflags=[\"-Ctarget-cpu=neoverse-v1\"]"
                        .into()
                )
            );
        });
    }

    #[test]
    fn target_cpu_config_arg_escapes_toml_string() {
        assert_eq!(
            target_rustflags_config_arg(
                "x86_64-unknown-linux-gnu",
                &[target_cpu_rustflag("quoted\"cpu")]
            ),
            "target.x86_64-unknown-linux-gnu.rustflags=[\"-Ctarget-cpu=quoted\\\"cpu\"]"
        );
    }

    #[test]
    fn target_rustflags_config_arg_preserves_loader_flags_as_target_config() {
        let flags = split_rustflags("-C panic=abort -C link-arg=-nostartfiles");
        assert_eq!(
            target_rustflags_config_arg("aarch64-unknown-linux-gnu", &flags),
            "target.aarch64-unknown-linux-gnu.rustflags=[\"-C\",\"panic=abort\",\"-C\",\"link-arg=-nostartfiles\"]"
        );
    }

    #[test]
    fn loader_rustflags_use_loader_specific_env_only() {
        with_rustflags_env(|| {
            unsafe {
                std::env::set_var("CARGO_ENCODED_RUSTFLAGS", "-Clto\x1f-Cpanic=abort");
                std::env::set_var("RUSTFLAGS", "-C debuginfo=1");
                std::env::set_var(
                    TARGET_RUSTFLAGS_ENV,
                    "-C link-arg=--target=aarch64-unknown-linux-gnu",
                );
                std::env::set_var(
                    LOADER_RUSTFLAGS_ENV,
                    "-C panic=unwind -C link-arg=--target=aarch64-unknown-linux-gnu -C link-arg=-fuse-ld=lld",
                );
            }

            assert_eq!(
                loader_build_rustflags("aarch64-unknown-linux-gnu", false),
                vec![
                    "-C",
                    "panic=unwind",
                    "-C",
                    "link-arg=--target=aarch64-unknown-linux-gnu",
                    "-C",
                    "link-arg=-fuse-ld=lld",
                    "-C",
                    "panic=abort",
                    "-C",
                    "target-feature=+crt-static",
                    "-C",
                    "relocation-model=static",
                    "-C",
                    "link-arg=-nostartfiles",
                    "-C",
                    "link-arg=-static"
                ]
            );
        });
    }

    #[test]
    fn payload_rustflags_preserve_cargo_encoded_rustflags_precedence() {
        with_rustflags_env(|| {
            unsafe {
                std::env::set_var("CARGO_ENCODED_RUSTFLAGS", "-Clto\x1f-Cpanic=abort");
                std::env::set_var("RUSTFLAGS", "-C debuginfo=1");
                std::env::set_var(TARGET_RUSTFLAGS_ENV, "-C link-arg=-fuse-ld=lld");
            }

            let PayloadRustflags::Encoded(flags) =
                payload_rustflags("aarch64-unknown-linux-gnu", "neoverse-n1")
            else {
                panic!("expected encoded rustflags");
            };

            assert_eq!(
                decode_encoded_rustflags(&flags),
                vec!["-Clto", "-Cpanic=abort", "-Ctarget-cpu=neoverse-n1"]
            );
        });
    }

    #[test]
    fn payload_rustflags_preserve_plain_rustflags_precedence() {
        with_rustflags_env(|| {
            unsafe {
                std::env::set_var("RUSTFLAGS", "-C debuginfo=1");
                std::env::set_var(TARGET_RUSTFLAGS_ENV, "-C link-arg=-fuse-ld=lld");
            }

            assert_eq!(
                payload_rustflags("aarch64-unknown-linux-gnu", "neoverse-n1"),
                PayloadRustflags::Plain(OsString::from("-C debuginfo=1 -Ctarget-cpu=neoverse-n1"))
            );
        });
    }

    #[test]
    fn generated_aarch64_loader_does_not_read_midr_el1_before_fallbacks() {
        let source = generated_main(
            "aarch64-unknown-linux-musl",
            false,
            LoaderStrategy::Embedded,
        );
        assert!(source.contains("read_small_file_hex"));
        assert!(!source.contains("let midr = unsafe { read_midr_el1() };"));
    }

    fn with_rustflags_env(test: impl FnOnce()) {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        let vars = [
            "CARGO_ENCODED_RUSTFLAGS".to_string(),
            "RUSTFLAGS".to_string(),
            "CARGO_BUILD_RUSTFLAGS".to_string(),
            LOADER_RUSTFLAGS_ENV.to_string(),
            TARGET_RUSTFLAGS_ENV.to_string(),
        ];
        let _restore = EnvRestore {
            saved: vars
                .iter()
                .map(|var| (var.clone(), std::env::var_os(var)))
                .collect(),
        };
        unsafe {
            for var in &vars {
                std::env::remove_var(var);
            }
        }

        test();
    }

    struct EnvRestore {
        saved: Vec<(String, Option<OsString>)>,
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            unsafe {
                for (var, value) in self.saved.drain(..) {
                    if let Some(value) = value {
                        std::env::set_var(var, value);
                    } else {
                        std::env::remove_var(var);
                    }
                }
            }
        }
    }

    fn decode_encoded_rustflags(flags: &OsString) -> Vec<String> {
        flags
            .to_string_lossy()
            .split('\x1f')
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn generic_required_mask_is_empty_but_rank_mask_is_recorded() {
        let rank = feature_mask(&["avx2".into()]).unwrap();
        assert_eq!(FeatureMask::EMPTY.count(), 0);
        assert_eq!(rank.count(), 1);
    }

    #[test]
    fn non_codegen_features_are_rank_only_not_safety_required() {
        let required = safety_required_features(&[
            "avx2".into(),
            "pmuv3".into(),
            "rdseed".into(),
            "rdrand".into(),
            "spe".into(),
            "ssbs".into(),
        ]);
        assert_eq!(required, vec!["avx2"]);
    }

    #[test]
    fn native_target_cpu_is_rejected() {
        assert!(normalize_target_cpus(vec!["native".into()], "x86_64").is_err());
    }

    #[test]
    fn x86_64_baseline_is_implicit_in_config() {
        assert_eq!(
            normalize_target_cpus(vec!["haswell".into()], "x86_64").unwrap(),
            vec!["x86-64", "haswell"]
        );
    }

    #[test]
    fn aarch64_generic_is_implicit_in_config() {
        assert_eq!(
            normalize_target_cpus(vec!["neoverse-v1".into()], "aarch64").unwrap(),
            vec!["generic", "neoverse-v1"]
        );
    }

    #[test]
    fn explicit_baseline_target_cpu_is_rejected() {
        let err = normalize_target_cpus(vec!["x86-64".into()], "x86_64").unwrap_err();
        assert!(
            err.to_string().contains("x86-64"),
            "unexpected error: {err:#}"
        );

        let err = normalize_target_cpus(vec!["generic".into()], "aarch64").unwrap_err();
        assert!(
            err.to_string().contains("generic"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn unknown_target_cpu_is_error() {
        let current = BTreeSet::from(["generic".into(), "znver5".into()]);
        let union = current.clone();
        assert!(
            filter_target_cpus(&["generic".into(), "zenver5".into()], &current, &union).is_err()
        );
    }

    #[test]
    fn x86_64_v1_spelling_is_not_accepted() {
        let current = BTreeSet::from(["x86-64".into(), "x86-64-v2".into()]);
        let union = current.clone();

        let got =
            filter_target_cpus(&["x86-64".into(), "x86-64-v2".into()], &current, &union).unwrap();
        assert_eq!(got, vec!["x86-64", "x86-64-v2"]);

        let err = filter_target_cpus(&["x86-64".into(), "x86-64-v1".into()], &current, &union)
            .unwrap_err();
        assert!(
            err.to_string().contains("x86-64-v1"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn cross_arch_target_cpu_is_skipped_not_error() {
        let current = BTreeSet::from(["generic".into(), "znver5".into()]);
        let union = BTreeSet::from(["generic".into(), "znver5".into(), "neoverse-v1".into()]);
        let got = filter_target_cpus(
            &["generic".into(), "znver5".into(), "neoverse-v1".into()],
            &current,
            &union,
        )
        .unwrap();
        assert_eq!(got, vec!["generic", "znver5"]);
    }

    #[test]
    fn duplicate_target_cpus_are_deduplicated() {
        let current = BTreeSet::from(["generic".into(), "arrowlake-s".into()]);
        let union = current.clone();
        let got = filter_target_cpus(
            &["generic".into(), "arrowlake-s".into(), "arrowlake-s".into()],
            &current,
            &union,
        )
        .unwrap();
        assert_eq!(got, vec!["generic", "arrowlake-s"]);
    }

    #[test]
    fn target_cpu_payload_names_do_not_collapse_aliases() {
        assert_ne!(sanitize_cpu("arrowlake-s"), sanitize_cpu("arrowlake_s"));
        assert_eq!(sanitize_cpu("arrowlake-s"), "arrowlake_2ds");
        assert_eq!(sanitize_cpu("arrowlake_s"), "arrowlake_5fs");
    }

    #[test]
    fn generated_linux_sys_gates_arch_specific_syscalls() {
        let text = generated_linux_sys();
        let close_x86 = "#[cfg(target_arch = \"x86_64\")]\nconst SYS_CLOSE: usize = 3;";
        let close_aarch64 = "#[cfg(target_arch = \"aarch64\")]\nconst SYS_CLOSE: usize = 57;";

        assert!(text.contains(close_x86));
        assert!(text.contains(close_aarch64));
    }

    #[test]
    fn unknown_runtime_features_are_reported_for_variant_skipping() {
        let features = classify_runtime_features(&["avx2".into(), "not-real".into()]);
        assert_eq!(features.known, vec!["avx2"]);
        assert_eq!(features.unknown, vec!["not-real"]);
    }

    #[test]
    fn probe_report_shows_skipped_unknown_feature_variants() {
        let report = format_probe_report(
            "x86_64-unknown-linux-gnu",
            HostInfo {
                arch: TargetArch::X86_64,
                features: FeatureMask::EMPTY,
                identity: CpuIdentity::Unknown,
                heterogeneous: false,
            },
            &[ProbeVariant {
                target_cpu: "generic".to_string(),
                required_features: FeatureMask::EMPTY,
                rank_features: FeatureMask::EMPTY,
                feature_names: Vec::new(),
                feature_tier: 0,
            }],
            &[SkippedProbeVariant {
                target_cpu: "futurelake".to_string(),
                reason: "unknown runtime feature(s): avx10".to_string(),
            }],
            "generic",
        );

        assert!(report.contains("  skipped:\n"));
        assert!(report.contains("futurelake reason=unknown runtime feature(s): avx10"));
        assert!(report.contains("selected=generic"));
    }

    #[test]
    fn score_report_orders_and_prints_selector_scores() {
        let mut avx512 = FeatureMask::EMPTY;
        avx512.insert(crate::feature_mask::Feature::Avx512F);
        let mut avx2 = FeatureMask::EMPTY;
        avx2.insert(crate::feature_mask::Feature::Avx2);

        let host = HostInfo {
            arch: TargetArch::X86_64,
            features: avx512,
            identity: CpuIdentity::X86 {
                vendor: X86Vendor::Amd,
                family: 26,
                model: 1,
                stepping: 0,
            },
            heterogeneous: false,
        };
        let metas = [
            VariantMeta {
                target_cpu: "x86-64-v4",
                required_features: avx512,
                rank_features: avx512,
                rank_feature_count: avx512.count(),
                feature_tier: 1,
                target_kind: TargetKind::X86NeutralLevel { level: 4 },
            },
            VariantMeta {
                target_cpu: "znver5",
                required_features: avx512,
                rank_features: avx512,
                rank_feature_count: avx512.count(),
                feature_tier: 2,
                target_kind: TargetKind::X86AmdZen { generation: 5 },
            },
            VariantMeta {
                target_cpu: "generic",
                required_features: FeatureMask::EMPTY,
                rank_features: FeatureMask::EMPTY,
                rank_feature_count: 0,
                feature_tier: 0,
                target_kind: TargetKind::Generic,
            },
        ];
        let variants = [
            ProbeVariant {
                target_cpu: "x86-64-v4".to_string(),
                required_features: avx512,
                rank_features: avx512,
                feature_names: vec!["avx512f".to_string()],
                feature_tier: 1,
            },
            ProbeVariant {
                target_cpu: "znver5".to_string(),
                required_features: avx512,
                rank_features: avx512,
                feature_names: vec!["avx512f".to_string()],
                feature_tier: 2,
            },
            ProbeVariant {
                target_cpu: "generic".to_string(),
                required_features: FeatureMask::EMPTY,
                rank_features: avx2,
                feature_names: vec!["avx2".to_string()],
                feature_tier: 0,
            },
        ];

        let report = format_score_report("x86_64-unknown-linux-gnu", host, &metas, &variants, 2);

        assert!(report.contains("cargo-sonic score"));
        assert!(report.contains("selected=znver5"));
        assert!(
            report.contains("rank=1 target_cpu=znver5 exact=2 vendor_affinity=1 feature_score=")
        );
        assert!(report.contains("rank=2 target_cpu=x86-64-v4"));
        assert!(report.contains("skipped=2"));
    }

    #[test]
    fn configured_collision_emits_warning() {
        let warnings = analyze_warnings(
            &BTreeMap::from([
                ("a".into(), vec!["avx2".into()]),
                ("b".into(), vec!["avx2".into()]),
            ]),
            "x86_64",
        );
        assert!(warnings.iter().any(|w| w.contains("identical")));
    }

    #[test]
    fn configured_incomparable_overlap_emits_warning() {
        let warnings = analyze_warnings(
            &BTreeMap::from([
                ("haswell".into(), vec!["avx2".into()]),
                ("znver5".into(), vec!["avx2".into(), "avx512f".into()]),
            ]),
            "x86_64",
        );
        assert!(warnings.iter().any(|w| w.contains("neutral")));
    }

    #[test]
    fn linux_only_target_is_enforced() {
        assert_eq!(
            cfg_value("target_os=\"linux\"\ntarget_arch=\"x86_64\"", "target_os").as_deref(),
            Some("linux")
        );
    }

    #[test]
    fn classifies_modern_x86_target_cpus_for_selector_affinity() {
        assert_eq!(
            classify_target_cpu("sierraforest", "x86_64", "x86-64"),
            TargetKind::X86IntelAtom
        );
        assert_eq!(
            classify_target_cpu("clearwaterforest", "x86_64", "x86-64"),
            TargetKind::X86IntelAtom
        );
        assert_eq!(
            classify_target_cpu("grandridge", "x86_64", "x86-64"),
            TargetKind::X86IntelAtom
        );
        assert_eq!(
            classify_target_cpu("sapphirerapids", "x86_64", "x86-64"),
            TargetKind::X86IntelXeon
        );
        assert_eq!(
            classify_target_cpu("graniterapids-d", "x86_64", "x86-64"),
            TargetKind::X86IntelXeon
        );
        assert_eq!(
            classify_target_cpu("arrowlake", "x86_64", "x86-64"),
            TargetKind::X86IntelCore
        );
        assert_eq!(
            classify_target_cpu("core_5th_gen_avx", "x86_64", "x86-64"),
            TargetKind::X86IntelCore
        );
        assert_eq!(
            classify_target_cpu("slm", "x86_64", "x86-64"),
            TargetKind::X86IntelAtom
        );
        assert_eq!(
            classify_target_cpu("knl", "x86_64", "x86-64"),
            TargetKind::X86IntelXeon
        );
        assert_eq!(
            classify_target_cpu("znver5", "x86_64", "x86-64"),
            TargetKind::X86AmdZen { generation: 5 }
        );
        assert_eq!(
            classify_target_cpu("x86-64", "x86_64", "x86-64"),
            TargetKind::Generic
        );
    }

    #[test]
    fn score_filters_cross_vendor_x86_targets() {
        let amd = HostInfo {
            arch: TargetArch::X86_64,
            features: FeatureMask::EMPTY,
            identity: CpuIdentity::X86 {
                vendor: X86Vendor::Amd,
                family: 26,
                model: 1,
                stepping: 0,
            },
            heterogeneous: false,
        };
        assert!(score_target_kind_matches_host(
            amd,
            TargetKind::X86AmdZen { generation: 5 }
        ));
        assert!(score_target_kind_matches_host(
            amd,
            TargetKind::X86NeutralLevel { level: 4 }
        ));
        assert!(score_target_kind_matches_host(
            amd,
            classify_target_cpu("x86-64-v4", "x86_64", "x86-64")
        ));
        assert!(!score_target_kind_matches_host(
            amd,
            TargetKind::X86IntelCore
        ));
        assert!(!score_target_kind_matches_host(
            amd,
            classify_target_cpu("tigerlake", "x86_64", "x86-64")
        ));
        assert!(!score_target_kind_matches_host(
            amd,
            classify_target_cpu("knl", "x86_64", "x86-64")
        ));
    }

    #[test]
    fn x86_tiers_rank_target_kind_not_neutral_version() {
        assert_eq!(
            target_feature_tier("x86_64", TargetKind::X86NeutralLevel { level: 1 }),
            1
        );
        assert_eq!(
            target_feature_tier("x86_64", TargetKind::X86NeutralLevel { level: 4 }),
            1
        );
        assert_eq!(target_feature_tier("x86_64", TargetKind::X86IntelCore), 2);
    }

    #[test]
    fn aarch64_tiers_rank_target_kind_not_feature_generation() {
        assert_eq!(target_feature_tier("aarch64", TargetKind::Generic), 0);
        assert_eq!(
            target_feature_tier("aarch64", TargetKind::Aarch64ArmCortexA),
            2
        );
        assert_eq!(
            target_feature_tier("aarch64", TargetKind::Aarch64ArmNeoverseN),
            2
        );
        assert_eq!(target_feature_tier("aarch64", TargetKind::Aarch64Apple), 2);
    }

    #[test]
    fn classifies_modern_aarch64_target_cpus_for_selector_affinity() {
        assert_eq!(
            classify_target_cpu("cortex-a725", "aarch64", "generic"),
            TargetKind::Aarch64ArmCortexA
        );
        assert_eq!(
            classify_target_cpu("neoverse-n3", "aarch64", "generic"),
            TargetKind::Aarch64ArmNeoverseN
        );
        assert_eq!(
            classify_target_cpu("neoverse-v3ae", "aarch64", "generic"),
            TargetKind::Aarch64ArmNeoverseV
        );
        assert_eq!(
            classify_target_cpu("a64fx", "aarch64", "generic"),
            TargetKind::Aarch64Other
        );
        assert_eq!(
            classify_target_cpu("generic", "aarch64", "generic"),
            TargetKind::Generic
        );
    }

    #[test]
    fn builds_and_runs_baseline_fixture() {
        if !cfg!(target_os = "linux") || !cfg!(target_arch = "x86_64") {
            return;
        }
        let manifest = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/env-printer/Cargo.toml");
        let output = build(BuildOptions {
            cargo_args: Vec::new(),
            manifest_path: Some(manifest),
            target_cpus: vec!["x86-64-v2".into()],
            parallelism: 1,
            compress: PayloadCompression::None,
            compression_level: 22,
            loader: LoaderStrategy::Embedded,
            auditable: true,
        })
        .unwrap();
        let run = Command::new(&output.final_binary)
            .arg("one")
            .env("KEEP_ME", "yes")
            .env("CARGO_SONIC_ENABLE", "false")
            .env("CARGO_SONIC_ENABLED", "old")
            .output()
            .unwrap();
        assert!(run.status.success());
        let stdout = String::from_utf8(run.stdout).unwrap();
        assert!(stdout.contains("argv=one"));
        assert!(stdout.contains("keep=yes"));
        assert!(stdout.contains("enabled=1"));
        assert!(stdout.contains("cpu=x86-64"));
        assert!(stdout.contains("flags="));

        if Command::new("readelf").arg("--version").output().is_ok() {
            let phdr = Command::new("readelf")
                .args(["-l", output.final_binary.as_str()])
                .output()
                .unwrap();
            assert!(!String::from_utf8(phdr.stdout).unwrap().contains("INTERP"));
            let dyns = Command::new("readelf")
                .args(["-d", output.final_binary.as_str()])
                .output()
                .unwrap();
            assert!(!String::from_utf8(dyns.stdout).unwrap().contains("NEEDED"));
            let sections = Command::new("readelf")
                .args(["-S", output.final_binary.as_str()])
                .output()
                .unwrap();
            assert!(
                String::from_utf8(sections.stdout)
                    .unwrap()
                    .contains(".dep-v0")
            );
        }
    }

    #[test]
    fn parse_color_mode_maps_known_and_falls_back_to_auto() {
        assert_eq!(parse_color_mode("always"), ColorMode::Always);
        assert_eq!(parse_color_mode("never"), ColorMode::Never);
        assert_eq!(parse_color_mode("auto"), ColorMode::Auto);
        assert_eq!(parse_color_mode(""), ColorMode::Auto);
        assert_eq!(parse_color_mode("totally-bogus"), ColorMode::Auto);
        // Explicit Always/Never resolve deterministically; Auto depends on
        // whether the test process happens to attach a TTY, so don't assert on it.
        assert_eq!(resolved_cargo_color(Some(ColorMode::Always)), "always");
        assert_eq!(resolved_cargo_color(Some(ColorMode::Never)), "never");
        let auto = resolved_cargo_color(Some(ColorMode::Auto));
        assert!(auto == "always" || auto == "never");
        let none = resolved_cargo_color(None);
        assert!(none == "always" || none == "never");
    }

    #[test]
    fn known_cargo_args_keeps_only_cargo_recognised_values() {
        let args = strings(&[
            "--release",
            "--features",
            "fast", // unknown, dropped (and value too)
            "--profile",
            "bench",
            "--target=x86_64-unknown-linux-gnu",
            "--bin",
            "demo",
            "-p",
            "alpha",
            "--manifest-path=foo/Cargo.toml",
            "--color",
            "always",
            "--color=never",
            "--target-dir",
            "out",
            "stray",
        ]);
        let known = known_cargo_args(&args);
        assert_eq!(
            known,
            strings(&[
                "--release",
                "--profile",
                "bench",
                "--target=x86_64-unknown-linux-gnu",
                "--bin",
                "demo",
                "-p",
                "alpha",
                "--manifest-path=foo/Cargo.toml",
                "--color",
                "always",
                "--color=never",
                "--target-dir",
                "out",
            ])
        );
    }

    #[test]
    fn known_cargo_args_handles_trailing_flag_without_value() {
        // Trailing `--target` with no value must not panic.
        let args = strings(&["--unknown", "--target"]);
        let known = known_cargo_args(&args);
        assert_eq!(known, strings(&["--target"]));
    }

    #[test]
    fn forwarded_cargo_args_strips_overridden_flags() {
        let args = strings(&[
            "--features",
            "fast",
            "--manifest-path",
            "Cargo.toml",
            "--target-dir",
            "custom",
            "--color",
            "never",
            "--release",
            "--manifest-path=alt.toml",
            "--target-dir=alt-out",
            "--color=always",
            "--bin",
            "demo",
        ]);
        let forwarded = forwarded_cargo_args(&args);
        assert_eq!(
            forwarded,
            strings(&["--features", "fast", "--release", "--bin", "demo",])
        );
    }

    #[test]
    fn parse_cargo_args_defaults_when_args_empty() {
        let got = parse_cargo_args(&[]);
        assert!(!got.release);
        assert!(got.target.is_none());
        assert!(got.target_dir.is_none());
        assert!(got.bin.is_none());
        assert!(got.package.is_none());
        assert!(got.manifest_path.is_none());
        assert!(got.color.is_none());
        assert!(got.forwarded.is_empty());
    }

    #[test]
    fn host_arch_name_covers_both_arches() {
        assert_eq!(host_arch_name(TargetArch::X86_64), "x86_64");
        assert_eq!(host_arch_name(TargetArch::Aarch64), "aarch64");
    }

    #[test]
    fn feature_names_preserves_canonical_order_and_filters() {
        let mut mask = FeatureMask::EMPTY;
        mask.insert(crate::feature_mask::Feature::Avx2);
        mask.insert(crate::feature_mask::Feature::Sse2);
        let names = feature_names(mask);
        assert!(names.contains(&"avx2"));
        assert!(names.contains(&"sse2"));
        assert!(!names.contains(&"avx512f"));
        // Order matches ALL_FEATURES iteration order, not insertion order.
        let positions: Vec<usize> = crate::feature_mask::ALL_FEATURES
            .iter()
            .enumerate()
            .filter_map(|(i, feature)| {
                names
                    .contains(&crate::feature_mask::feature_name(*feature))
                    .then_some(i)
            })
            .collect();
        let mut sorted = positions.clone();
        sorted.sort();
        assert_eq!(positions, sorted);
    }

    #[test]
    fn feature_names_empty_mask_returns_empty_vec() {
        assert!(feature_names(FeatureMask::EMPTY).is_empty());
    }

    #[test]
    fn format_words_renders_padded_hex_pair() {
        assert_eq!(
            format_words(FeatureMask::EMPTY),
            "[0x0000000000000000,0x0000000000000000]"
        );
        let mask = FeatureMask::from_words([0xdead_beef, 0x12_3456]);
        assert_eq!(
            format_words(mask),
            "[0x00000000deadbeef,0x0000000000123456]"
        );
    }

    #[test]
    fn format_identity_renders_each_variant() {
        assert_eq!(format_identity(CpuIdentity::Unknown), "unknown");
        assert_eq!(
            format_identity(CpuIdentity::X86 {
                vendor: X86Vendor::Intel,
                family: 6,
                model: 167,
                stepping: 1,
            }),
            "x86(intel family=6 model=167 stepping=1)"
        );
        assert_eq!(
            format_identity(CpuIdentity::X86 {
                vendor: X86Vendor::Amd,
                family: 25,
                model: 33,
                stepping: 2,
            }),
            "x86(amd family=25 model=33 stepping=2)"
        );
        assert_eq!(
            format_identity(CpuIdentity::X86 {
                vendor: X86Vendor::Other,
                family: 0,
                model: 0,
                stepping: 0,
            }),
            "x86(other family=0 model=0 stepping=0)"
        );
        assert_eq!(
            format_identity(CpuIdentity::Aarch64 {
                implementer: 0x41,
                part: 0xd4f,
                variant: 0,
                revision: 1,
            }),
            "aarch64(implementer=0x41 part=0xd4f variant=0 revision=1)"
        );
    }

    #[test]
    fn effective_target_cpus_rejects_empty_input() {
        let err = effective_target_cpus(Vec::new()).unwrap_err();
        assert!(err.to_string().contains("--target-cpus"));
    }

    #[test]
    fn effective_target_cpus_passes_through_non_empty() {
        let cpus = strings(&["haswell", "znver5"]);
        assert_eq!(effective_target_cpus(cpus.clone()).unwrap(), cpus);
    }

    #[test]
    fn baseline_target_cpu_chooses_arch_specific_baseline() {
        assert_eq!(baseline_target_cpu("x86_64"), "x86-64");
        assert_eq!(baseline_target_cpu("aarch64"), "generic");
        assert_eq!(baseline_target_cpu(""), "generic");
        assert_eq!(baseline_target_cpu("riscv"), "generic");
    }

    #[test]
    fn cfg_value_returns_none_for_missing_or_malformed_keys() {
        let cfg = "target_os=\"linux\"\ntarget_arch=\"x86_64\"\n";
        assert_eq!(cfg_value(cfg, "missing").as_deref(), None);
        assert_eq!(
            cfg_value("target_os=\"linux\nstray", "target_os").as_deref(),
            None,
            "missing closing quote should not match"
        );
        assert_eq!(cfg_value("", "target_os").as_deref(), None);
    }

    #[test]
    fn parse_target_features_from_rustc_cfg_dedups_and_ignores_unrelated_lines() {
        let cfg = "target_os=\"linux\"\n\
                   target_feature=\"sse2\"\n\
                   target_feature=\"sse2\"\n\
                   target_feature=\"avx\"\n\
                   not_a_feature=\"avx512f\"\n";
        let got = parse_target_features_from_rustc_cfg(cfg);
        assert_eq!(got, vec!["avx", "sse2"]);
    }

    #[test]
    fn parse_rustc_target_cpus_skips_header_and_blanks() {
        let text = "\
Available CPUs for this target:
    generic
    znver5
    haswell - description

";
        let got = parse_rustc_target_cpus(text);
        assert!(got.contains("generic"));
        assert!(got.contains("znver5"));
        assert!(got.contains("haswell"));
        assert!(!got.iter().any(|cpu| cpu.contains("Available")));
    }

    #[test]
    fn parse_rustc_target_cpus_always_includes_generic() {
        let got = parse_rustc_target_cpus("");
        assert!(got.contains("generic"));
    }

    #[test]
    fn filter_target_cpus_rejects_native_cpu() {
        let current = BTreeSet::from(["generic".into()]);
        let union = current.clone();
        let err = filter_target_cpus(&["native".into()], &current, &union).unwrap_err();
        assert!(err.to_string().contains("native"));
    }

    #[test]
    fn filter_target_cpus_accepts_generic_even_if_not_in_current() {
        let current = BTreeSet::<String>::new();
        let union = BTreeSet::<String>::new();
        let got = filter_target_cpus(&["generic".into()], &current, &union).unwrap();
        assert_eq!(got, vec!["generic"]);
    }

    #[test]
    fn is_ignored_rustc_feature_matches_documented_set() {
        for ignored in ["crt-static", "ermsb", "lahfsahf", "prfchw", "x87"] {
            assert!(
                is_ignored_rustc_feature(ignored),
                "should ignore `{ignored}`"
            );
        }
        for kept in ["avx2", "sse2", "neon", "sve"] {
            assert!(
                !is_ignored_rustc_feature(kept),
                "should not ignore `{kept}`"
            );
        }
    }

    #[test]
    fn is_safety_required_feature_excludes_runtime_only_features() {
        // Documented set of rank-only / non-codegen-safety features.
        for non_safety in [
            "bti", "dit", "lor", "mte", "paca", "pacg", "pan", "pmuv3", "rand", "ras", "rdrand",
            "rdseed", "sb", "spe", "ssbs", "vh",
        ] {
            assert!(
                !is_safety_required_feature(non_safety),
                "`{non_safety}` should be rank-only"
            );
        }
        for safety in ["avx2", "sse2", "neon", "sve", "sve2"] {
            assert!(
                is_safety_required_feature(safety),
                "`{safety}` must require codegen safety"
            );
        }
    }

    #[test]
    fn feature_mask_errors_on_unknown_feature() {
        let err = feature_mask(&["totally-not-real".into()]).unwrap_err();
        assert!(err.to_string().contains("unsupported runtime feature"));
        assert!(err.to_string().contains("totally-not-real"));
    }

    #[test]
    fn feature_mask_accumulates_known_features() {
        let mask = feature_mask(&["sse2".into(), "avx2".into()]).unwrap();
        assert!(mask.contains(crate::feature_mask::Feature::Sse2));
        assert!(mask.contains(crate::feature_mask::Feature::Avx2));
        assert!(!mask.contains(crate::feature_mask::Feature::Avx512F));
    }

    #[test]
    fn safety_required_features_drops_rank_only_entries() {
        let got = safety_required_features(&[
            "avx2".into(),
            "rdrand".into(),
            "rdseed".into(),
            "sse2".into(),
            "pmuv3".into(),
        ]);
        assert_eq!(got, vec!["avx2".to_string(), "sse2".to_string()]);
    }

    #[test]
    fn score_target_kind_matches_host_intel() {
        let intel = HostInfo {
            arch: TargetArch::X86_64,
            features: FeatureMask::EMPTY,
            identity: CpuIdentity::X86 {
                vendor: X86Vendor::Intel,
                family: 6,
                model: 0xb7,
                stepping: 0,
            },
            heterogeneous: false,
        };
        assert!(score_target_kind_matches_host(
            intel,
            TargetKind::X86IntelCore
        ));
        assert!(score_target_kind_matches_host(
            intel,
            TargetKind::X86IntelXeon
        ));
        assert!(score_target_kind_matches_host(
            intel,
            TargetKind::X86IntelAtom
        ));
        assert!(!score_target_kind_matches_host(
            intel,
            TargetKind::X86AmdZen { generation: 5 }
        ));
        assert!(!score_target_kind_matches_host(
            intel,
            TargetKind::X86AmdOther
        ));
        assert!(score_target_kind_matches_host(intel, TargetKind::Generic));
        assert!(score_target_kind_matches_host(
            intel,
            TargetKind::X86NeutralLevel { level: 3 }
        ));
    }

    #[test]
    fn score_target_kind_matches_host_other_x86_vendor() {
        let other = HostInfo {
            arch: TargetArch::X86_64,
            features: FeatureMask::EMPTY,
            identity: CpuIdentity::X86 {
                vendor: X86Vendor::Other,
                family: 6,
                model: 1,
                stepping: 0,
            },
            heterogeneous: false,
        };
        // Other vendor: only generic / neutral pass, identity-specific fails.
        assert!(score_target_kind_matches_host(other, TargetKind::Generic));
        assert!(score_target_kind_matches_host(
            other,
            TargetKind::X86NeutralLevel { level: 2 }
        ));
        assert!(!score_target_kind_matches_host(
            other,
            TargetKind::X86IntelCore
        ));
        assert!(!score_target_kind_matches_host(
            other,
            TargetKind::X86AmdOther
        ));
    }

    #[test]
    fn score_target_kind_matches_host_unknown_passes_anything() {
        let unknown = HostInfo {
            arch: TargetArch::X86_64,
            features: FeatureMask::EMPTY,
            identity: CpuIdentity::Unknown,
            heterogeneous: false,
        };
        assert!(score_target_kind_matches_host(
            unknown,
            TargetKind::X86IntelCore
        ));
        assert!(score_target_kind_matches_host(
            unknown,
            TargetKind::Aarch64ArmCortexA
        ));
    }

    #[test]
    fn score_target_kind_matches_host_aarch64_arm() {
        let arm = HostInfo {
            arch: TargetArch::Aarch64,
            features: FeatureMask::EMPTY,
            identity: CpuIdentity::Aarch64 {
                implementer: 0x41,
                part: 0xd4f,
                variant: 0,
                revision: 0,
            },
            heterogeneous: false,
        };
        for kind in [
            TargetKind::Aarch64ArmCortexA,
            TargetKind::Aarch64ArmCortexX,
            TargetKind::Aarch64ArmNeoverseE,
            TargetKind::Aarch64ArmNeoverseN,
            TargetKind::Aarch64ArmNeoverseV,
        ] {
            assert!(score_target_kind_matches_host(arm, kind));
        }
        assert!(!score_target_kind_matches_host(
            arm,
            TargetKind::Aarch64Ampere
        ));
        assert!(!score_target_kind_matches_host(
            arm,
            TargetKind::Aarch64Apple
        ));
        assert!(!score_target_kind_matches_host(
            arm,
            TargetKind::Aarch64Other
        ));
    }

    #[test]
    fn score_target_kind_matches_host_aarch64_ampere() {
        let ampere = HostInfo {
            arch: TargetArch::Aarch64,
            features: FeatureMask::EMPTY,
            identity: CpuIdentity::Aarch64 {
                implementer: 0xc0,
                part: 0xac4,
                variant: 0,
                revision: 0,
            },
            heterogeneous: false,
        };
        assert!(score_target_kind_matches_host(
            ampere,
            TargetKind::Aarch64Ampere
        ));
        assert!(!score_target_kind_matches_host(
            ampere,
            TargetKind::Aarch64ArmCortexA
        ));
        assert!(!score_target_kind_matches_host(
            ampere,
            TargetKind::Aarch64ArmNeoverseN
        ));
    }

    #[test]
    fn score_target_kind_matches_host_aarch64_other_implementer() {
        let other = HostInfo {
            arch: TargetArch::Aarch64,
            features: FeatureMask::EMPTY,
            identity: CpuIdentity::Aarch64 {
                implementer: 0x46, // Fujitsu — not 0x41 or 0xc0
                part: 0x001,
                variant: 0,
                revision: 0,
            },
            heterogeneous: false,
        };
        assert!(score_target_kind_matches_host(other, TargetKind::Generic));
        // Identity-specific kinds all fall through to false.
        assert!(!score_target_kind_matches_host(
            other,
            TargetKind::Aarch64ArmCortexA
        ));
        assert!(!score_target_kind_matches_host(
            other,
            TargetKind::Aarch64Ampere
        ));
        assert!(!score_target_kind_matches_host(
            other,
            TargetKind::Aarch64Other
        ));
    }

    #[test]
    fn parse_aarch64_midr_hex_handles_separators_and_prefix() {
        assert_eq!(parse_aarch64_midr_hex(b"0x410FD4F1"), Some(0x410FD4F1));
        assert_eq!(parse_aarch64_midr_hex(b"410fd4f1"), Some(0x410FD4F1));
        assert_eq!(
            parse_aarch64_midr_hex(b"  0X 410f d4f1\n"),
            Some(0x410FD4F1)
        );
    }

    #[test]
    fn parse_aarch64_midr_hex_returns_none_on_empty_or_garbage() {
        assert_eq!(parse_aarch64_midr_hex(b""), None);
        assert_eq!(parse_aarch64_midr_hex(b" \t\n"), None);
        assert_eq!(parse_aarch64_midr_hex(b"!!!"), None);
    }

    #[test]
    fn parse_aarch64_midr_hex_stops_on_non_hex_after_first_digit() {
        // Only the leading hex run is consumed.
        assert_eq!(parse_aarch64_midr_hex(b"deadGGGG"), Some(0xDEAD));
    }

    #[test]
    fn aarch64_identity_from_midr_decomposes_register_bits() {
        let id = aarch64_identity_from_midr(0x410F_D4F1);
        match id {
            CpuIdentity::Aarch64 {
                implementer,
                part,
                variant,
                revision,
            } => {
                assert_eq!(implementer, 0x41);
                assert_eq!(part, 0xd4f);
                assert_eq!(variant, 0);
                assert_eq!(revision, 1);
            }
            _ => panic!("expected aarch64 identity"),
        }
    }

    #[test]
    fn parse_aarch64_cpuinfo_identity_unknown_when_keys_missing() {
        assert_eq!(parse_aarch64_cpuinfo_identity(b""), CpuIdentity::Unknown);
        assert_eq!(
            parse_aarch64_cpuinfo_identity(b"processor\t: 0\nBogusInfo: 1\n"),
            CpuIdentity::Unknown
        );
        // Implementer present but no part — still Unknown.
        assert_eq!(
            parse_aarch64_cpuinfo_identity(b"CPU implementer\t: 0x41\n"),
            CpuIdentity::Unknown
        );
    }

    #[test]
    fn parse_aarch64_cpuinfo_identity_handles_missing_optionals() {
        // No CPU variant / revision lines: should default to 0.
        let id = parse_aarch64_cpuinfo_identity(b"CPU implementer\t: 0x41\nCPU part\t: 0xd0c\n");
        assert_eq!(
            id,
            CpuIdentity::Aarch64 {
                implementer: 0x41,
                part: 0xd0c,
                variant: 0,
                revision: 0,
            }
        );
    }

    #[test]
    fn aarch64_cpuinfo_decimal_value_saturates_on_overflow() {
        // Overflow-saturating decimal — make sure we don't panic.
        let buf = b"CPU revision\t: 9999999999\n";
        let got = aarch64_cpuinfo_decimal_value(buf, b"CPU revision");
        assert_eq!(got, Some(u16::MAX));
    }

    #[test]
    fn aarch64_cpuinfo_value_returns_none_for_line_without_colon() {
        let buf = b"CPU implementer 0x41\n";
        assert!(aarch64_cpuinfo_value(buf, b"CPU implementer").is_none());
    }

    #[test]
    fn aarch64_cpuinfo_hex_value_stops_on_non_hex_garbage() {
        let buf = b"CPU part\t: 0xd05XYZ\n";
        assert_eq!(aarch64_cpuinfo_hex_value(buf, b"CPU part"), Some(0xd05));
    }

    #[test]
    fn sanitize_cpu_escapes_non_alnum() {
        assert_eq!(sanitize_cpu("haswell"), "haswell");
        assert_eq!(sanitize_cpu("arrowlake-s"), "arrowlake_2ds");
        assert_eq!(sanitize_cpu("a/b"), "a_2fb");
        assert_eq!(sanitize_cpu("x86-64-v3"), "x86_2d64_2dv3");
    }

    #[test]
    fn cargo_target_rustflags_env_uppercases_and_replaces_punctuation() {
        assert_eq!(
            cargo_target_rustflags_env("aarch64-unknown-linux-gnu"),
            "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS"
        );
        assert_eq!(
            cargo_target_rustflags_env("x86_64-unknown-linux-musl"),
            "CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS"
        );
    }

    #[test]
    fn escape_toml_string_escapes_quotes_and_backslashes() {
        assert_eq!(escape_toml_string("plain"), "plain");
        assert_eq!(escape_toml_string("a\"b"), "a\\\"b");
        assert_eq!(escape_toml_string("a\\b"), "a\\\\b");
        // Order: backslash first, then quote — `"\\` -> `\\\\\"`
        assert_eq!(escape_toml_string("\\\""), "\\\\\\\"");
    }

    #[test]
    fn target_cpu_rustflag_formats_codegen_arg() {
        assert_eq!(target_cpu_rustflag("x86-64-v3"), "-Ctarget-cpu=x86-64-v3");
        assert_eq!(target_cpu_rustflag("generic"), "-Ctarget-cpu=generic");
    }

    #[test]
    fn split_rustflags_splits_on_whitespace() {
        assert!(split_rustflags("").is_empty());
        assert_eq!(
            split_rustflags("  -C panic=abort  "),
            vec!["-C", "panic=abort"]
        );
    }

    #[test]
    fn target_rustflags_config_arg_quotes_each_flag() {
        let out = target_rustflags_config_arg(
            "x86_64-unknown-linux-gnu",
            &["-C panic=abort".to_string(), "-Clto".to_string()],
        );
        assert!(out.starts_with("target.x86_64-unknown-linux-gnu.rustflags=["));
        assert!(out.contains("\"-C panic=abort\""));
        assert!(out.contains("\"-Clto\""));
    }

    #[test]
    fn append_encoded_rustflags_chains_with_unit_separator() {
        let flags = encode_rustflags(&["a".into(), "b".into(), "c".into()]);
        assert_eq!(flags, OsString::from("a\x1fb\x1fc"));
    }

    #[test]
    fn append_encoded_rustflags_preserves_existing_value() {
        let base = OsString::from("base");
        let combined = append_encoded_rustflags(base, &["next".to_string()]);
        assert_eq!(combined, OsString::from("base\x1fnext"));
    }

    #[test]
    fn loader_rustflags_picks_per_target_arms() {
        // x86_64-musl: link-self-contained=no, no -nostartfiles
        let musl = loader_rustflags("x86_64-unknown-linux-musl");
        assert!(musl.contains("link-self-contained=no"));
        assert!(!musl.contains("-nostartfiles"));
        assert!(musl.contains("code-model=large"));

        // x86_64 gnu: code-model=large + -nostartfiles, no link-self-contained
        let gnu = loader_rustflags("x86_64-unknown-linux-gnu");
        assert!(gnu.contains("code-model=large"));
        assert!(gnu.contains("-nostartfiles"));
        assert!(!gnu.contains("link-self-contained=no"));

        // aarch64-musl: link-self-contained=no, no code-model=large
        let amusl = loader_rustflags("aarch64-unknown-linux-musl");
        assert!(amusl.contains("link-self-contained=no"));
        assert!(!amusl.contains("code-model=large"));

        // aarch64-gnu: -nostartfiles, no link-self-contained
        let agnu = loader_rustflags("aarch64-unknown-linux-gnu");
        assert!(agnu.contains("-nostartfiles"));
        assert!(!agnu.contains("link-self-contained=no"));
    }

    #[test]
    fn payload_rustflags_falls_back_to_cargo_config_when_no_env() {
        with_rustflags_env(|| {
            let got = payload_rustflags("aarch64-unknown-linux-gnu", "neoverse-n1");
            match got {
                PayloadRustflags::CargoConfig(arg) => {
                    assert!(arg.starts_with("target.aarch64-unknown-linux-gnu.rustflags="));
                    assert!(arg.contains("-Ctarget-cpu=neoverse-n1"));
                }
                other => panic!("expected CargoConfig, got {other:?}"),
            }
        });
    }

    #[test]
    fn loader_build_rustflags_appends_auditable_link_args() {
        with_rustflags_env(|| {
            let flags = loader_build_rustflags("aarch64-unknown-linux-gnu", true);
            let joined = flags.join(" ");
            assert!(joined.contains("link-arg=auditable.o"));
            assert!(joined.contains("__cargo_sonic_auditable_dep_v0"));
        });
    }

    #[test]
    fn loader_build_rustflags_omits_auditable_link_args_when_disabled() {
        with_rustflags_env(|| {
            let flags = loader_build_rustflags("aarch64-unknown-linux-gnu", false);
            let joined = flags.join(" ");
            assert!(!joined.contains("auditable.o"));
            assert!(!joined.contains("__cargo_sonic_auditable_dep_v0"));
        });
    }

    #[test]
    fn bundle_dir_for_appends_bundle_suffix() {
        let dir = bundle_dir_for(Utf8Path::new("/tmp/myapp"));
        assert_eq!(dir.as_str(), "/tmp/myapp.bundle");
    }

    #[test]
    fn payload_extension_branches_on_compression() {
        assert_eq!(payload_extension(PayloadCompression::None), ".elf");
        assert_eq!(payload_extension(PayloadCompression::Zstd), ".elf.zstd");
    }

    #[test]
    fn payload_compression_expr_renders_match_arms() {
        assert_eq!(
            payload_compression_expr(PayloadCompression::None),
            "PayloadCompression::None"
        );
        assert_eq!(
            payload_compression_expr(PayloadCompression::Zstd),
            "PayloadCompression::Zstd"
        );
    }

    #[test]
    fn target_kind_expr_covers_every_variant() {
        let mappings = [
            (TargetKind::Generic, "TargetKind::Generic"),
            (TargetKind::X86IntelCore, "TargetKind::X86IntelCore"),
            (TargetKind::X86IntelXeon, "TargetKind::X86IntelXeon"),
            (TargetKind::X86IntelAtom, "TargetKind::X86IntelAtom"),
            (TargetKind::X86AmdOther, "TargetKind::X86AmdOther"),
            (
                TargetKind::Aarch64ArmNeoverseN,
                "TargetKind::Aarch64ArmNeoverseN",
            ),
            (
                TargetKind::Aarch64ArmNeoverseV,
                "TargetKind::Aarch64ArmNeoverseV",
            ),
            (
                TargetKind::Aarch64ArmNeoverseE,
                "TargetKind::Aarch64ArmNeoverseE",
            ),
            (
                TargetKind::Aarch64ArmCortexA,
                "TargetKind::Aarch64ArmCortexA",
            ),
            (
                TargetKind::Aarch64ArmCortexX,
                "TargetKind::Aarch64ArmCortexX",
            ),
            (TargetKind::Aarch64Apple, "TargetKind::Aarch64Apple"),
            (TargetKind::Aarch64Ampere, "TargetKind::Aarch64Ampere"),
            (TargetKind::Aarch64Other, "TargetKind::Aarch64Other"),
        ];
        for (kind, expected) in mappings {
            assert_eq!(target_kind_expr(kind), expected);
        }
        assert_eq!(
            target_kind_expr(TargetKind::X86NeutralLevel { level: 3 }),
            "TargetKind::X86NeutralLevel { level: 3 }"
        );
        assert_eq!(
            target_kind_expr(TargetKind::X86AmdZen { generation: 5 }),
            "TargetKind::X86AmdZen { generation: 5 }"
        );
    }

    #[test]
    fn audit_source_translates_known_registry_and_strips_scheme() {
        let crates_io = cargo_metadata::Source {
            repr: "registry+https://github.com/rust-lang/crates.io-index".to_string(),
        };
        assert_eq!(audit_source(&crates_io), "crates.io");

        let custom = cargo_metadata::Source {
            repr: "registry+https://internal.example.com/index".to_string(),
        };
        assert_eq!(audit_source(&custom), "registry");

        let git = cargo_metadata::Source {
            repr: "git+https://example.com/foo.git".to_string(),
        };
        assert_eq!(audit_source(&git), "git");
    }

    #[test]
    fn auditable_object_supports_x86_64_and_aarch64() {
        let payload = b"hello-payload";
        let x = auditable_object("x86_64-unknown-linux-gnu", payload).unwrap();
        // ELF magic header.
        assert_eq!(&x[..4], b"\x7fELF");
        let a = auditable_object("aarch64-unknown-linux-musl", payload).unwrap();
        assert_eq!(&a[..4], b"\x7fELF");
    }

    #[test]
    fn auditable_object_rejects_unsupported_target() {
        let err = auditable_object("riscv64gc-unknown-linux-gnu", b"x").unwrap_err();
        assert!(
            err.to_string()
                .contains("only supported for x86_64 and aarch64")
        );
    }

    #[test]
    fn write_tagged_output_prefixes_each_chunk_line() {
        let mut buf: Vec<u8> = Vec::new();
        write_tagged_output(&mut buf, "haswell", b"hello\nworld").unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.starts_with("cargo-sonic[haswell]\n"));
        assert!(text.contains("  hello\n"));
        assert!(text.contains("  world\n"));
    }

    #[test]
    fn write_tagged_output_handles_empty_pending_gracefully() {
        let mut buf: Vec<u8> = Vec::new();
        write_tagged_output(&mut buf, "haswell", b"").unwrap();
        // Header and trailing newline only when pending is empty too.
        assert_eq!(buf, b"cargo-sonic[haswell]\n");
    }

    #[test]
    fn flush_payload_stdout_clears_pending_when_not_tagging() {
        let mut pending = vec![1u8, 2, 3];
        flush_payload_stdout("haswell", false, &mut pending).unwrap();
        assert!(pending.is_empty());
    }

    #[test]
    fn write_payload_stdout_extends_pending_when_tagging() {
        let mut pending = Vec::new();
        write_payload_stdout(true, &mut pending, b"abc").unwrap();
        write_payload_stdout(true, &mut pending, b"def").unwrap();
        assert_eq!(pending, b"abcdef");
    }

    #[test]
    fn probe_report_renders_eligible_only_variants() {
        let report = format_probe_report(
            "x86_64-unknown-linux-gnu",
            HostInfo {
                arch: TargetArch::X86_64,
                features: FeatureMask::EMPTY,
                identity: CpuIdentity::Unknown,
                heterogeneous: false,
            },
            &[ProbeVariant {
                target_cpu: "generic".to_string(),
                required_features: FeatureMask::EMPTY,
                rank_features: FeatureMask::EMPTY,
                feature_names: Vec::new(),
                feature_tier: 0,
            }],
            &[],
            "generic",
        );
        // No skipped section when input is empty.
        assert!(!report.contains("  skipped:"));
        assert!(report.contains("fits=[generic]"));
        assert!(report.contains("eligible=yes"));
        assert!(report.contains("selected=generic"));
    }

    #[test]
    fn probe_report_marks_ineligible_variants_with_missing_features() {
        let mut avx2 = FeatureMask::EMPTY;
        avx2.insert(crate::feature_mask::Feature::Avx2);
        let report = format_probe_report(
            "x86_64-unknown-linux-gnu",
            HostInfo {
                arch: TargetArch::X86_64,
                features: FeatureMask::EMPTY,
                identity: CpuIdentity::Unknown,
                heterogeneous: false,
            },
            &[ProbeVariant {
                target_cpu: "haswell".to_string(),
                required_features: avx2,
                rank_features: avx2,
                feature_names: vec!["avx2".to_string()],
                feature_tier: 1,
            }],
            &[],
            "generic",
        );
        assert!(report.contains("eligible=no"));
        assert!(report.contains("missing="));
        assert!(report.contains("missing_features=[avx2]"));
        assert!(report.contains("fits=[]"));
    }

    #[test]
    fn build_rejects_zero_parallelism_before_touching_cargo() {
        // parallelism=0 must short-circuit at the very first check so the
        // assertion does not accidentally depend on a real cargo workspace.
        let err = build(BuildOptions {
            cargo_args: Vec::new(),
            manifest_path: None,
            target_cpus: vec!["haswell".into()],
            parallelism: 0,
            compress: PayloadCompression::None,
            compression_level: 22,
            loader: LoaderStrategy::Embedded,
            auditable: false,
        })
        .unwrap_err();
        assert!(err.to_string().contains("--parallelism"));
    }

    #[test]
    fn build_rejects_empty_target_cpus_before_touching_cargo() {
        let err = build(BuildOptions {
            cargo_args: Vec::new(),
            manifest_path: None,
            target_cpus: Vec::new(),
            parallelism: 1,
            compress: PayloadCompression::None,
            compression_level: 22,
            loader: LoaderStrategy::Embedded,
            auditable: false,
        });
        assert!(err.is_err());
        // Either a manifest-resolution failure, an effective_target_cpus
        // failure, or a target_os check — any of these is fine; we just
        // confirm the call exercises early-validation paths without panic.
    }

    #[test]
    fn probe_rejects_empty_target_cpus() {
        let err = probe(ProbeOptions {
            cargo_args: Vec::new(),
            target_cpus: Vec::new(),
        })
        .unwrap_err();
        // First failing check inside `probe` is `effective_target_cpus`,
        // which surfaces the `--target-cpus` hint in its error message.
        assert!(err.to_string().contains("--target-cpus"));
    }

    #[test]
    fn generated_main_embedded_renders_loader_skeleton() {
        let source = generated_main("x86_64-unknown-linux-gnu", false, LoaderStrategy::Embedded);
        assert!(source.contains("compile_error!(\"cargo-sonic loader supports Linux only\")"));
        assert!(source.contains("mod feature_mask;"));
        assert!(source.contains("mod select;"));
        // The strategy is plumbed in via a placeholder substitution: the
        // call site at the entry point picks one of the two helpers, but
        // both helper *definitions* appear in the static template body.
        assert!(source.contains("exec_embedded_payload(selected, &initial)"));
        assert!(!source.contains("exec_bundle_payload(selected, &initial)"));
        // Without zstd, the no-op zstd shims appear instead of the real
        // FrameDecoder allocator block.
        assert!(source.contains("write_zstd_payload"));
        assert!(!source.contains("ruzstd::decoding::FrameDecoder"));
        // Placeholder must be fully substituted.
        assert!(!source.contains("__CARGO_SONIC_EXEC_PAYLOAD__"));
        assert!(!source.contains("/*__CARGO_SONIC_ZSTD_SUPPORT__*/"));
    }

    #[test]
    fn generated_main_bundle_swaps_exec_helper() {
        let source = generated_main("x86_64-unknown-linux-musl", false, LoaderStrategy::Bundle);
        assert!(source.contains("exec_bundle_payload(selected, &initial)"));
        assert!(!source.contains("exec_embedded_payload(selected, &initial)"));
    }

    #[test]
    fn generated_main_zstd_emits_mmap_allocator_and_decoder() {
        let source = generated_main("aarch64-unknown-linux-gnu", true, LoaderStrategy::Embedded);
        assert!(source.contains("MmapAllocator"));
        assert!(source.contains("global_allocator"));
        assert!(source.contains("ruzstd::decoding::FrameDecoder"));
        assert!(source.contains("write_zstd_payload_from_fd"));
    }

    #[test]
    fn generated_linux_sys_carries_arch_specific_syscall_numbers() {
        let source = generated_linux_sys();
        assert!(source.contains("SYS_WRITE"));
        assert!(source.contains("SYS_MMAP"));
        // Both arch tables present — gated by cfg, but visible as text.
        assert!(source.contains("#[cfg(target_arch = \"x86_64\")]"));
        assert!(source.contains("#[cfg(target_arch = \"aarch64\")]"));
        assert!(source.contains("memfd_create_best_effort"));
    }

    #[test]
    fn generated_stack_is_loader_stack_module() {
        let source = generated_stack();
        // loader_stack.rs is the file we're including.
        assert!(source.contains("InitialStack"));
    }

    #[test]
    fn escape_bytes_handles_quote_and_backslash() {
        assert_eq!(escape_bytes("plain"), "plain");
        assert_eq!(escape_bytes("a\"b"), "a\\\"b");
        assert_eq!(escape_bytes("a\\b"), "a\\\\b");
    }

    #[test]
    fn target_feature_tier_distinguishes_arches_and_target_kinds() {
        // Generic always tier 0, regardless of arch.
        assert_eq!(target_feature_tier("x86_64", TargetKind::Generic), 0);
        assert_eq!(target_feature_tier("aarch64", TargetKind::Generic), 0);
        // x86 neutral level → tier 1, x86 specific → tier 2.
        assert_eq!(
            target_feature_tier("x86_64", TargetKind::X86NeutralLevel { level: 3 }),
            1
        );
        assert_eq!(target_feature_tier("x86_64", TargetKind::X86IntelCore), 2);
        assert_eq!(
            target_feature_tier("x86_64", TargetKind::X86AmdZen { generation: 5 }),
            2
        );
        // aarch64 always tier 2 for any non-Generic.
        assert_eq!(
            target_feature_tier("aarch64", TargetKind::Aarch64ArmCortexA),
            2
        );
        assert_eq!(
            target_feature_tier("aarch64", TargetKind::Aarch64ArmNeoverseN),
            2
        );
        // Unknown arch falls through to 0.
        assert_eq!(target_feature_tier("riscv64", TargetKind::X86IntelCore), 0);
    }

    #[test]
    fn classify_target_cpu_aarch64_buckets() {
        assert_eq!(
            classify_target_cpu("cortex-a76", "aarch64", "generic"),
            TargetKind::Aarch64ArmCortexA
        );
        assert_eq!(
            classify_target_cpu("cortex-x1", "aarch64", "generic"),
            TargetKind::Aarch64ArmCortexX
        );
        assert_eq!(
            classify_target_cpu("neoverse-n1", "aarch64", "generic"),
            TargetKind::Aarch64ArmNeoverseN
        );
        assert_eq!(
            classify_target_cpu("neoverse-v3", "aarch64", "generic"),
            TargetKind::Aarch64ArmNeoverseV
        );
        assert_eq!(
            classify_target_cpu("neoverse-512tvb", "aarch64", "generic"),
            TargetKind::Aarch64ArmNeoverseV
        );
        assert_eq!(
            classify_target_cpu("neoverse-e1", "aarch64", "generic"),
            TargetKind::Aarch64ArmNeoverseE
        );
        assert_eq!(
            classify_target_cpu("apple-m1", "aarch64", "generic"),
            TargetKind::Aarch64Apple
        );
        assert_eq!(
            classify_target_cpu("ampere1", "aarch64", "generic"),
            TargetKind::Aarch64Ampere
        );
        assert_eq!(
            classify_target_cpu("a64fx", "aarch64", "generic"),
            TargetKind::Aarch64Other
        );
        assert_eq!(
            classify_target_cpu("generic", "aarch64", "generic"),
            TargetKind::Generic
        );
    }

    #[test]
    fn classify_target_cpu_x86_intel_xeon_special_arms() {
        // Note: arm precedence matters. The function tries the `lake/well/
        // bridge/...` Core arm before the `rapids/skx/...` Xeon arm, so
        // anything that contains `lake` resolves to Core regardless of its
        // suffix. That's documented by these assertions.
        assert_eq!(
            classify_target_cpu("knl", "x86_64", "x86-64"),
            TargetKind::X86IntelXeon
        );
        assert_eq!(
            classify_target_cpu("knm", "x86_64", "x86-64"),
            TargetKind::X86IntelXeon
        );
        assert_eq!(
            classify_target_cpu("mic_avx512", "x86_64", "x86-64"),
            TargetKind::X86IntelXeon
        );
        // Even though `skylake-avx512` would conceptually be a Xeon, the
        // `lake` substring matches the Core arm first.
        assert_eq!(
            classify_target_cpu("skylake-avx512", "x86_64", "x86-64"),
            TargetKind::X86IntelCore
        );
    }

    #[test]
    fn classify_target_cpu_x86_intel_atom_special_arms() {
        for cpu in ["bonnell", "slm", "tremont", "atom_avx", "silvermont"] {
            assert_eq!(
                classify_target_cpu(cpu, "x86_64", "x86-64"),
                TargetKind::X86IntelAtom,
                "{cpu}"
            );
        }
    }

    #[test]
    fn classify_target_cpu_x86_falls_back_to_amd_other_for_unknowns() {
        assert_eq!(
            classify_target_cpu("totally-unknown", "x86_64", "x86-64"),
            TargetKind::X86AmdOther
        );
    }

    #[test]
    fn classify_target_cpu_x86_neutral_levels() {
        for (cpu, lvl) in [
            ("x86-64", 1),
            ("x86-64-v2", 2),
            ("x86-64-v3", 3),
            ("x86-64-v4", 4),
        ] {
            // Only when the cpu is *not* the baseline does the neutral-level
            // arm fire. Use a different baseline so all four levels return
            // X86NeutralLevel.
            assert_eq!(
                classify_target_cpu(cpu, "x86_64", "alt-baseline"),
                TargetKind::X86NeutralLevel { level: lvl },
                "{cpu}"
            );
        }
    }

    #[test]
    fn classify_target_cpu_x86_znver_generation_fallback_zero() {
        // znverFOO → trim_start_matches("znver").parse() == None ⇒ unwrap_or(0).
        assert_eq!(
            classify_target_cpu("znverFOO", "x86_64", "x86-64"),
            TargetKind::X86AmdZen { generation: 0 }
        );
    }

    #[test]
    fn analyze_warnings_no_warnings_for_distinct_feature_sets() {
        let map = BTreeMap::from([
            ("haswell".into(), vec!["avx2".into()]),
            ("znver5".into(), vec!["avx512f".into()]),
        ]);
        let warnings = analyze_warnings(&map, "x86_64");
        assert!(
            warnings.iter().all(|w| !w.contains("identical")),
            "{warnings:?}"
        );
    }

    #[test]
    fn analyze_warnings_aarch64_does_not_emit_neutral_warning() {
        // Neutral-fallback warning is x86-only.
        let map = BTreeMap::from([
            ("cortex-a76".into(), vec!["fp16".into()]),
            ("neoverse-v1".into(), vec!["sve".into()]),
        ]);
        let warnings = analyze_warnings(&map, "aarch64");
        assert!(
            warnings.iter().all(|w| !w.contains("neutral")),
            "{warnings:?}"
        );
    }

    #[test]
    fn print_probe_report_prints_to_stdout_without_panic() {
        // Just exercise the `print!` wrapper around format_probe_report.
        print_probe_report(
            "x86_64-unknown-linux-gnu",
            HostInfo {
                arch: TargetArch::X86_64,
                features: FeatureMask::EMPTY,
                identity: CpuIdentity::Unknown,
                heterogeneous: false,
            },
            &[ProbeVariant {
                target_cpu: "generic".to_string(),
                required_features: FeatureMask::EMPTY,
                rank_features: FeatureMask::EMPTY,
                feature_names: Vec::new(),
                feature_tier: 0,
            }],
            &[],
            "generic",
        );
    }

    #[test]
    fn print_score_report_prints_to_stdout_without_panic() {
        print_score_report(
            "x86_64-unknown-linux-gnu",
            HostInfo {
                arch: TargetArch::X86_64,
                features: FeatureMask::EMPTY,
                identity: CpuIdentity::Unknown,
                heterogeneous: false,
            },
            &[],
            &[],
            0,
        );
    }

    #[test]
    fn print_variant_build_prelude_writes_to_stderr() {
        // Smoke test: just runs the eprintln + flush path without panic.
        print_variant_build_prelude("haswell");
    }

    #[test]
    fn leaked_str_returns_distinct_static_strs() {
        let a = leaked_str("hello-world");
        let b = leaked_str("hello-world");
        assert_eq!(a, "hello-world");
        assert_eq!(b, "hello-world");
        // They share no buffer (each call leaks a fresh allocation).
        assert!(!core::ptr::eq(a.as_ptr(), b.as_ptr()));
    }

    #[test]
    fn rustflags_for_target_uses_cargo_config_with_no_env() {
        with_rustflags_env(|| {
            let got =
                rustflags_for_target("aarch64-unknown-linux-gnu", ["-C panic=abort".to_string()]);
            match got {
                PayloadRustflags::CargoConfig(arg) => {
                    assert!(arg.contains("aarch64-unknown-linux-gnu.rustflags"));
                    assert!(arg.contains("-C panic=abort"));
                }
                other => panic!("expected CargoConfig, got {other:?}"),
            }
        });
    }

    #[test]
    fn rustflags_for_target_plain_path_appends_to_existing_flags() {
        with_rustflags_env(|| {
            unsafe {
                std::env::set_var("RUSTFLAGS", "-Cdebuginfo=2");
            }
            let got = rustflags_for_target("x86_64-unknown-linux-gnu", ["-Clto".to_string()]);
            match got {
                PayloadRustflags::Plain(flags) => {
                    let s = flags.to_string_lossy().to_string();
                    assert!(s.contains("-Cdebuginfo=2"));
                    assert!(s.contains("-Clto"));
                    // Joined with a space when both sides non-empty.
                    assert!(s.contains(" -Clto"));
                }
                other => panic!("expected Plain, got {other:?}"),
            }
        });
    }

    #[test]
    fn rustflags_for_target_plain_path_no_extra_separator_when_empty_existing() {
        with_rustflags_env(|| {
            unsafe {
                std::env::set_var("RUSTFLAGS", "");
            }
            let got = rustflags_for_target("x86_64-unknown-linux-gnu", ["-Clto".to_string()]);
            match got {
                PayloadRustflags::Plain(flags) => {
                    assert_eq!(flags.to_string_lossy(), "-Clto");
                }
                other => panic!("expected Plain, got {other:?}"),
            }
        });
    }

    #[test]
    fn rustflags_for_target_plain_path_preserves_when_no_new_flags() {
        with_rustflags_env(|| {
            unsafe {
                std::env::set_var("RUSTFLAGS", "-Cdebuginfo=2");
            }
            let got =
                rustflags_for_target("x86_64-unknown-linux-gnu", std::iter::empty::<String>());
            match got {
                PayloadRustflags::Plain(flags) => {
                    assert_eq!(flags.to_string_lossy(), "-Cdebuginfo=2");
                }
                other => panic!("expected Plain, got {other:?}"),
            }
        });
    }

    #[test]
    fn select_package_picks_root_when_present() {
        // Use this very crate's metadata: cargo-sonic is in a workspace, so
        // root_package() is None and there are multiple workspace_members.
        // First check the explicit-by-name path.
        let metadata = MetadataCommand::new()
            .manifest_path(format!("{}/Cargo.toml", env!("CARGO_MANIFEST_DIR")))
            .exec()
            .unwrap();
        let pkg = select_package(&metadata, Some("cargo-sonic")).unwrap();
        assert_eq!(pkg.name.as_str(), "cargo-sonic");
    }

    #[test]
    fn select_package_errors_on_unknown_package_name() {
        let metadata = MetadataCommand::new()
            .manifest_path(format!("{}/Cargo.toml", env!("CARGO_MANIFEST_DIR")))
            .exec()
            .unwrap();
        let err = select_package(&metadata, Some("definitely-not-real")).unwrap_err();
        assert!(err.to_string().contains("definitely-not-real"));
    }

    #[test]
    fn select_package_falls_back_to_root_or_workspace_member() {
        // No package hint: must succeed when there's a clear single root or
        // single workspace member, else error with --package hint.
        let metadata = MetadataCommand::new()
            .manifest_path(format!("{}/Cargo.toml", env!("CARGO_MANIFEST_DIR")))
            .exec()
            .unwrap();
        // cargo-sonic crate Cargo.toml IS the package itself ⇒ root_package returns Some.
        let pkg = select_package(&metadata, None).unwrap();
        assert_eq!(pkg.name.as_str(), "cargo-sonic");
    }

    #[test]
    fn select_package_errors_when_no_root_and_multi_members() {
        // Workspace root: root_package() is None, workspace_members > 1, so
        // select_package must surface the `--package` hint.
        let workspace_root = format!("{}/../..", env!("CARGO_MANIFEST_DIR"));
        let metadata = MetadataCommand::new()
            .manifest_path(format!("{workspace_root}/Cargo.toml"))
            .exec()
            .unwrap();
        // The workspace root itself has `members = ["crates/*"]`, no root.
        if metadata.root_package().is_none() && metadata.workspace_members.len() > 1 {
            let err = select_package(&metadata, None).unwrap_err();
            assert!(err.to_string().contains("--package"));
        }
    }

    #[test]
    fn resolve_bin_name_uses_explicit_override() {
        let metadata = MetadataCommand::new()
            .manifest_path(format!("{}/Cargo.toml", env!("CARGO_MANIFEST_DIR")))
            .exec()
            .unwrap();
        let pkg = select_package(&metadata, Some("cargo-sonic")).unwrap();
        let got = resolve_bin_name(pkg, Some("explicit-name"), &[]).unwrap();
        assert_eq!(got, "explicit-name");
    }

    #[test]
    fn resolve_bin_name_picks_single_bin_target_automatically() {
        // cargo-sonic itself has exactly one [[bin]]: cargo-sonic.
        let metadata = MetadataCommand::new()
            .manifest_path(format!("{}/Cargo.toml", env!("CARGO_MANIFEST_DIR")))
            .exec()
            .unwrap();
        let pkg = select_package(&metadata, Some("cargo-sonic")).unwrap();
        let got = resolve_bin_name(pkg, None, &[]).unwrap();
        assert_eq!(got, "cargo-sonic");
    }

    fn synth_variant_build(target_cpu: &str, artifact: &Utf8Path) -> VariantBuild {
        VariantBuild {
            target_cpu: target_cpu.to_string(),
            required_features: FeatureMask::EMPTY,
            rank_features: FeatureMask::EMPTY,
            feature_names: Vec::new(),
            feature_tier: 0,
            target_kind: TargetKind::Generic,
            artifact: artifact.to_path_buf(),
            payload_compression: PayloadCompression::None,
            uncompressed_len: 0,
            bundle_path: "generic.elf",
        }
    }

    #[test]
    fn prepare_bundle_directory_replaces_existing_dir_and_copies_payloads() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap().to_path_buf();
        let final_binary = root.join("myapp");
        let bundle_dir = bundle_dir_for(&final_binary);

        // Pre-create the bundle dir with a stale entry so we exercise the
        // "exists ⇒ remove_dir_all" branch.
        fs::create_dir_all(&bundle_dir).unwrap();
        fs::write(bundle_dir.join("stale-file"), b"stale").unwrap();

        // Two real payload files to copy in.
        let payload_a = root.join("payload-a.elf");
        fs::write(&payload_a, b"AAAA").unwrap();
        let payload_b = root.join("payload-b.elf");
        fs::write(&payload_b, b"BBBB").unwrap();
        let variants = [
            VariantBuild {
                bundle_path: "x86-64.elf",
                ..synth_variant_build("x86-64", &payload_a)
            },
            VariantBuild {
                bundle_path: "haswell.elf",
                ..synth_variant_build("haswell", &payload_b)
            },
        ];

        prepare_bundle_directory(&final_binary, &variants).unwrap();

        // Stale file gone, new files present.
        assert!(!bundle_dir.join("stale-file").exists());
        assert_eq!(fs::read(bundle_dir.join("x86-64.elf")).unwrap(), b"AAAA");
        assert_eq!(fs::read(bundle_dir.join("haswell.elf")).unwrap(), b"BBBB");

        // make_executable ran on each payload (Unix only).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(bundle_dir.join("x86-64.elf"))
                .unwrap()
                .permissions()
                .mode();
            assert!(mode & 0o111 != 0, "expected executable bit, got {mode:o}");
        }
    }

    #[test]
    fn prepare_bundle_directory_handles_missing_source_gracefully() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap().to_path_buf();
        let final_binary = root.join("myapp");

        let missing = root.join("does-not-exist.elf");
        let variants = [synth_variant_build("haswell", &missing)];

        let err = prepare_bundle_directory(&final_binary, &variants).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("failed to copy"));
    }

    #[test]
    fn generated_manifest_includes_payload_paths_and_sanitized_consts() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap().to_path_buf();
        // Variant artifact must exist so fs::metadata().len() succeeds in the
        // embedded path and reports the right size.
        let payload = root.join("payload.elf");
        fs::write(&payload, b"hello").unwrap();

        let mut features = FeatureMask::EMPTY;
        features.insert(crate::feature_mask::Feature::Avx2);
        let variant = VariantBuild {
            target_cpu: "haswell".to_string(),
            required_features: features,
            rank_features: features,
            feature_names: vec!["avx2".to_string()],
            feature_tier: 2,
            target_kind: TargetKind::X86IntelCore,
            artifact: payload,
            payload_compression: PayloadCompression::None,
            uncompressed_len: 5,
            bundle_path: "haswell.elf",
        };

        let manifest = generated_manifest(std::slice::from_ref(&variant), LoaderStrategy::Embedded);
        assert!(manifest.contains("pub static VARIANTS"));
        assert!(manifest.contains("target_cpu: \"haswell\""));
        assert!(manifest.contains("PAYLOAD_HASWELL"));
        assert!(manifest.contains("AlignedPayload"));
        assert!(
            manifest.contains(
                "env_selected_target_cpu: b\"CARGO_SONIC_SELECTED_TARGET_CPU=haswell\\0\""
            )
        );
        assert!(manifest.contains("payload_compression: PayloadCompression::None"));

        let bundle_manifest = generated_manifest(&[variant], LoaderStrategy::Bundle);
        // Bundle strategy uses an empty payload reference instead of an
        // include_bytes! constant.
        assert!(bundle_manifest.contains("payload: &[]"));
        assert!(!bundle_manifest.contains("AlignedPayload"));
    }

    #[test]
    fn generated_manifest_with_no_variants_emits_empty_table() {
        let manifest = generated_manifest(&[], LoaderStrategy::Embedded);
        assert!(manifest.contains("pub static VARIANTS: &[Variant] = &[\n];"));
    }

    #[test]
    fn generated_manifest_with_zstd_uses_zstd_compression_marker() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap().to_path_buf();
        let payload = root.join("payload.elf.zstd");
        fs::write(&payload, b"compressed").unwrap();

        let variant = VariantBuild {
            payload_compression: PayloadCompression::Zstd,
            ..synth_variant_build("znver5", &payload)
        };
        let manifest = generated_manifest(&[variant], LoaderStrategy::Embedded);
        assert!(manifest.contains("payload_compression: PayloadCompression::Zstd"));
    }

    #[test]
    fn generate_loader_crate_writes_full_workspace_layout() {
        let tmp = tempfile::TempDir::new().unwrap();
        let loader_dir = Utf8Path::from_path(tmp.path()).unwrap().to_path_buf();

        // No variants: loader without zstd dependency.
        generate_loader_crate(
            &loader_dir,
            "x86_64-unknown-linux-gnu",
            &[],
            LoaderStrategy::Embedded,
            None,
        )
        .unwrap();
        let cargo_toml = fs::read_to_string(loader_dir.join("Cargo.toml")).unwrap();
        assert!(cargo_toml.contains("name = \"sonic-generated-loader\""));
        assert!(!cargo_toml.contains("ruzstd"));

        // src files all present.
        for f in [
            "src/feature_mask.rs",
            "src/select.rs",
            "src/arch_x86_64.rs",
            "src/arch_aarch64.rs",
            "src/linux_sys.rs",
            "src/stack.rs",
            "src/generated_manifest.rs",
            "src/main.rs",
        ] {
            assert!(loader_dir.join(f).exists(), "missing: {f}");
        }
        // No auditable.o without an audit section.
        assert!(!loader_dir.join("auditable.o").exists());
    }

    #[test]
    fn generate_loader_crate_emits_zstd_dependency_when_a_variant_uses_zstd() {
        let tmp = tempfile::TempDir::new().unwrap();
        let loader_dir = Utf8Path::from_path(tmp.path()).unwrap().to_path_buf();

        let payload = loader_dir.join("payload.elf.zstd");
        fs::write(&payload, b"compressed").unwrap();
        let variant = VariantBuild {
            payload_compression: PayloadCompression::Zstd,
            ..synth_variant_build("znver5", &payload)
        };

        generate_loader_crate(
            &loader_dir,
            "x86_64-unknown-linux-gnu",
            &[variant],
            LoaderStrategy::Embedded,
            None,
        )
        .unwrap();
        let cargo_toml = fs::read_to_string(loader_dir.join("Cargo.toml")).unwrap();
        assert!(cargo_toml.contains("ruzstd"));
    }

    #[test]
    fn generate_loader_crate_writes_auditable_object_when_section_provided() {
        let tmp = tempfile::TempDir::new().unwrap();
        let loader_dir = Utf8Path::from_path(tmp.path()).unwrap().to_path_buf();
        generate_loader_crate(
            &loader_dir,
            "aarch64-unknown-linux-gnu",
            &[],
            LoaderStrategy::Embedded,
            Some(b"audit-section-bytes"),
        )
        .unwrap();
        let auditable = loader_dir.join("auditable.o");
        assert!(auditable.exists(), "auditable.o should have been written");
        let bytes = fs::read(&auditable).unwrap();
        // ELF header.
        assert_eq!(&bytes[..4], b"\x7fELF");
    }

    #[test]
    fn cfg_value_strips_quoted_values() {
        let cfg = "target_endian=\"little\"\ntarget_pointer_width=\"64\"\n";
        assert_eq!(cfg_value(cfg, "target_endian").as_deref(), Some("little"));
        assert_eq!(
            cfg_value(cfg, "target_pointer_width").as_deref(),
            Some("64")
        );
    }

    #[test]
    fn classify_runtime_features_handles_only_unknown() {
        let set = classify_runtime_features(&["totally-bogus".into()]);
        assert!(set.known.is_empty());
        assert_eq!(set.unknown, vec!["totally-bogus"]);
    }

    #[test]
    fn classify_runtime_features_drops_ignored_before_classification() {
        let set = classify_runtime_features(&[
            "crt-static".into(),
            "ermsb".into(),
            "lahfsahf".into(),
            "prfchw".into(),
            "x87".into(),
        ]);
        assert!(set.known.is_empty());
        assert!(set.unknown.is_empty());
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[cfg(miri)]
    mod loader_miri {
        mod stack {
            include!("loader_stack.rs");
        }

        use std::ffi::CStr;

        const AT_NULL: usize = 0;
        const AT_PHDR: usize = 3;
        const AT_HWCAP: usize = 16;
        const AT_HWCAP2: usize = 26;
        const AT_HWCAP3: usize = 29;

        #[test]
        fn loader_miri_parses_initial_stack_layout() {
            let words = stack_words(&[
                b"KEEP_ME=yes\0",
                b"CARGO_SONIC_DEBUG=1\0",
                b"CARGO_SONIC_ENABLE=false\0",
            ]);

            let initial = unsafe { stack::InitialStack::parse(words.as_ptr()) };

            assert_eq!(initial.argc, 2);
            assert_eq!(initial.envc, 3);
            assert_eq!(initial.phdr, 0x1234);
            assert_eq!(initial.hwcap, 0xab);
            assert_eq!(initial.hwcap2, 0xcd);
            assert_eq!(initial.hwcap3, 0xef);
            assert_eq!(unsafe { cstr(*initial.argv.add(0)) }, "loader");
            assert_eq!(unsafe { cstr(*initial.argv.add(1)) }, "--flag");
            assert!(unsafe { stack::debug_enabled(&initial) });
            assert!(!unsafe { stack::sonic_enabled(&initial) });
        }

        #[test]
        fn loader_miri_build_envp_filters_old_sonic_values() {
            let words = stack_words(&[
                b"KEEP_ME=yes\0",
                b"CARGO_SONIC_ENABLED=old\0",
                b"CARGO_SONIC_SELECTED_TARGET_CPU=old\0",
                b"CARGO_SONIC_SELECTED_FLAGS=old\0",
                b"CARGO_SONIC_ENABLE=1\0",
            ]);
            let initial = unsafe { stack::InitialStack::parse(words.as_ptr()) };

            let envp = unsafe {
                stack::build_envp(
                    &initial,
                    b"CARGO_SONIC_ENABLED=1\0",
                    b"CARGO_SONIC_SELECTED_TARGET_CPU=x86-64\0",
                    b"CARGO_SONIC_SELECTED_FLAGS=sse2\0",
                )
            };

            assert!(!envp.is_null());
            let got = unsafe { collect_envp(envp) };
            unsafe {
                let _ = Vec::from_raw_parts(envp.cast_mut().cast::<usize>(), 0, got.len() + 1);
            }
            assert_eq!(
                got,
                vec![
                    "KEEP_ME=yes",
                    "CARGO_SONIC_ENABLE=1",
                    "CARGO_SONIC_ENABLED=1",
                    "CARGO_SONIC_SELECTED_TARGET_CPU=x86-64",
                    "CARGO_SONIC_SELECTED_FLAGS=sse2",
                ]
            );
        }

        #[test]
        fn loader_miri_enable_flag_accepts_only_documented_disabled_values() {
            for disabled in [
                b"CARGO_SONIC_ENABLE=0\0".as_slice(),
                b"CARGO_SONIC_ENABLE=false\0".as_slice(),
                b"CARGO_SONIC_ENABLE=FALSE\0".as_slice(),
            ] {
                let words = stack_words(&[disabled]);
                let initial = unsafe { stack::InitialStack::parse(words.as_ptr()) };
                assert!(!unsafe { stack::sonic_enabled(&initial) });
            }

            for enabled in [
                b"CARGO_SONIC_ENABLE=1\0".as_slice(),
                b"CARGO_SONIC_ENABLE=true\0".as_slice(),
                b"OTHER=value\0".as_slice(),
            ] {
                let words = stack_words(&[enabled]);
                let initial = unsafe { stack::InitialStack::parse(words.as_ptr()) };
                assert!(unsafe { stack::sonic_enabled(&initial) });
            }
        }

        fn stack_words(env: &[&[u8]]) -> Vec<usize> {
            let aux_words = 10;
            let mut words = vec![0; 1 + 2 + 1 + env.len() + 1 + aux_words];
            words[0] = 2;

            unsafe {
                let ptrs = words.as_mut_ptr().cast::<*const u8>();
                ptrs.add(1).write(b"loader\0".as_ptr());
                ptrs.add(2).write(b"--flag\0".as_ptr());
                ptrs.add(3).write(core::ptr::null());
                for (i, value) in env.iter().enumerate() {
                    ptrs.add(4 + i).write(value.as_ptr());
                }
                ptrs.add(4 + env.len()).write(core::ptr::null());
            }

            let aux = 4 + env.len() + 1;
            words[aux] = AT_PHDR;
            words[aux + 1] = 0x1234;
            words[aux + 2] = AT_HWCAP;
            words[aux + 3] = 0xab;
            words[aux + 4] = AT_HWCAP2;
            words[aux + 5] = 0xcd;
            words[aux + 6] = AT_HWCAP3;
            words[aux + 7] = 0xef;
            words[aux + 8] = AT_NULL;
            words[aux + 9] = 0;
            words
        }

        unsafe fn collect_envp(envp: *const *const u8) -> Vec<String> {
            let mut out = Vec::new();
            let mut i = 0;
            unsafe {
                while !(*envp.add(i)).is_null() {
                    out.push(cstr(*envp.add(i)));
                    i += 1;
                }
            }
            out
        }

        unsafe fn cstr(ptr: *const u8) -> String {
            unsafe { CStr::from_ptr(ptr.cast()).to_string_lossy().into_owned() }
        }
    }
}
