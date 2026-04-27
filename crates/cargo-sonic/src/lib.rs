use anyhow::{Context, Result, anyhow, bail};
use camino::{Utf8Path, Utf8PathBuf};
use cargo_metadata::{Artifact, Message, MetadataCommand, Package};
use clap::{Arg, ArgAction, Command as ClapCommand};
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

use crate::feature_mask::{Feature, FeatureMask, feature_by_name};
use crate::select::{
    CpuIdentity, HostInfo, TargetArch, TargetKind, VariantMeta, X86Vendor, select_variant,
};
use std::collections::{BTreeMap, BTreeSet};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

#[derive(Debug, Clone)]
pub struct BuildOptions {
    pub cargo_args: Vec<String>,
    pub manifest_path: Option<Utf8PathBuf>,
    pub target_cpus: Vec<String>,
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
struct CargoArgs {
    release: bool,
    target: Option<String>,
    target_dir: Option<Utf8PathBuf>,
    bin: Option<String>,
    package: Option<String>,
    manifest_path: Option<Utf8PathBuf>,
    forwarded: Vec<String>,
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
}

struct ProbeVariant {
    target_cpu: String,
    required_features: FeatureMask,
    rank_features: FeatureMask,
    feature_names: Vec<String>,
    feature_tier: u8,
}

pub fn build(options: BuildOptions) -> Result<BuildOutput> {
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
    let configured_cpus = normalize_target_cpus(effective_target_cpus(options.target_cpus)?)?;

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
        let filtered = filter_runtime_features(&features);
        if cpu != "generic" {
            unsupported_runtime_features(&filtered)?;
        }
        features_by_cpu.insert(cpu.clone(), filtered);
    }
    warnings.extend(analyze_warnings(&features_by_cpu, &target_arch));
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }

    let mut variants = Vec::new();
    for cpu in &included {
        let feature_names = features_by_cpu.get(cpu).cloned().unwrap_or_default();
        let rank_features = feature_mask(&feature_names)?;
        let required_feature_names = safety_required_features(&feature_names);
        let required_features = if cpu == "generic" {
            FeatureMask::EMPTY
        } else {
            feature_mask(&required_feature_names)?
        };
        print_variant_build_prelude(cpu);
        let artifact = build_payload_variant(
            package,
            &cargo_args,
            manifest_path.as_deref(),
            &target,
            profile,
            cpu,
            &out_root,
        )?;
        variants.push(VariantBuild {
            target_cpu: cpu.clone(),
            required_features,
            rank_features,
            feature_names,
            feature_tier: feature_tier(&target_arch, rank_features),
            target_kind: classify_target_cpu(cpu, &target_arch),
            artifact,
        });
    }

    let bin_name = resolve_bin_name(package, cargo_args.bin.as_deref(), &variants)?;
    let loader_dir = out_root.join("loader");
    generate_loader_crate(
        &loader_dir,
        &target,
        &variants,
        auditable_section.as_deref(),
    )?;
    let loader_artifact = build_loader(&loader_dir, &target, profile)?;
    let final_binary = out_root.join(&bin_name);
    fs::create_dir_all(out_root.as_path())?;
    fs::copy(&loader_artifact, &final_binary)
        .with_context(|| format!("failed to copy final fat binary to {final_binary}"))?;
    make_executable(&final_binary)?;
    Ok(BuildOutput {
        final_binary,
        warnings,
    })
}

pub fn probe(options: ProbeOptions) -> Result<()> {
    let cargo_args = parse_cargo_args(&options.cargo_args);
    let configured_cpus = normalize_target_cpus(effective_target_cpus(options.target_cpus)?)?;

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
    let current_valid = rustc_target_cpus(&target)?;
    let union_valid = known_supported_cpu_union()?;
    let included = filter_target_cpus(&configured_cpus, &current_valid, &union_valid)?;

    let mut metas = Vec::new();
    let mut display = Vec::new();
    for cpu in &included {
        let features = parse_target_features_from_rustc_cfg(&rustc_cfg(&target, Some(cpu))?);
        let feature_names = filter_runtime_features(&features);
        if cpu != "generic" {
            unsupported_runtime_features(&feature_names)?;
        }
        let rank_features = feature_mask(&feature_names)?;
        let required_feature_names = safety_required_features(&feature_names);
        let required_features = if cpu == "generic" {
            FeatureMask::EMPTY
        } else {
            feature_mask(&required_feature_names)?
        };
        let target_kind = classify_target_cpu(cpu, &target_arch);
        metas.push(VariantMeta {
            target_cpu: leaked_str(cpu),
            required_features,
            rank_features,
            rank_feature_count: rank_features.count(),
            feature_tier: feature_tier(&target_arch, rank_features),
            target_kind,
        });
        display.push(ProbeVariant {
            target_cpu: cpu.clone(),
            required_features,
            rank_features,
            feature_names,
            feature_tier: feature_tier(&target_arch, rank_features),
        });
    }

    let selected = select_variant(host, &metas);
    print_probe_report(&target, host, &display, selected.target_cpu);
    Ok(())
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
        forwarded: forwarded_cargo_args(args),
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
            | "--manifest-path" => {
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
                    || value.starts_with("--manifest-path=") =>
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
}

fn repeated_arg(name: &'static str) -> Arg {
    Arg::new(name).num_args(1).action(ArgAction::Append)
}

fn forwarded_cargo_args(args: &[String]) -> Vec<String> {
    let mut forwarded = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--manifest-path" | "--target-dir" => i += 1,
            v if v.starts_with("--manifest-path=") || v.starts_with("--target-dir=") => {}
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

fn print_probe_report(target: &str, host: HostInfo, variants: &[ProbeVariant], selected: &str) {
    println!("cargo-sonic probe");
    println!("  target={target}");
    println!("  host.arch={}", host_arch_name(host.arch));
    println!("  host.features={}", format_words(host.features));
    println!(
        "  host.feature_names=[{}]",
        feature_names(host.features).join(",")
    );
    println!("  host.identity={}", format_identity(host.identity));
    println!("  variants:");
    for variant in variants {
        let eligible = variant.target_cpu == "generic"
            || variant.required_features.is_subset_of(host.features);
        let missing = FeatureMask::from_words([
            variant.required_features.words()[0] & !host.features.words()[0],
            variant.required_features.words()[1] & !host.features.words()[1],
        ]);
        print!(
            "    {} eligible={} tier={} count={} required={}",
            variant.target_cpu,
            if eligible { "yes" } else { "no" },
            variant.feature_tier,
            variant.rank_features.count(),
            format_words(variant.required_features)
        );
        if !eligible {
            print!(
                " missing={} missing_features=[{}]",
                format_words(missing),
                feature_names(missing).join(",")
            );
        }
        println!(" flags=[{}]", variant.feature_names.join(","));
    }
    let eligible = variants
        .iter()
        .filter(|variant| {
            variant.target_cpu == "generic" || variant.required_features.is_subset_of(host.features)
        })
        .map(|variant| variant.target_cpu.as_str())
        .collect::<Vec<_>>();
    println!("  fits=[{}]", eligible.join(","));
    println!("  selected={selected}");
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
        identity: CpuIdentity::Unknown,
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

fn normalize_target_cpus(mut cpus: Vec<String>) -> Result<Vec<String>> {
    if cpus.iter().any(|cpu| cpu == "native") {
        bail!("target-cpu \"native\" is rejected because cargo-sonic builds portable artifacts");
    }
    if cpus.iter().any(|cpu| cpu == "generic") {
        bail!("target-cpu \"generic\" is implicit; remove it from --target-cpus");
    }
    cpus.insert(0, "generic".to_string());
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
    features
        .iter()
        .filter(|feature| !matches!(feature.as_str(), "crt-static" | "x87"))
        .cloned()
        .collect()
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
    for cpu in configured {
        if cpu == "generic" {
            out.push(cpu.clone());
        } else if cpu == "native" {
            bail!("target-cpu \"native\" is rejected");
        } else if current_valid.contains(cpu) {
            out.push(cpu.clone());
        } else if known_union.contains(cpu) {
            continue;
        } else {
            bail!("unknown target-cpu spelling `{cpu}`");
        }
    }
    Ok(out)
}

fn unsupported_runtime_features(features: &[String]) -> Result<()> {
    let unsupported: Vec<_> = features
        .iter()
        .filter(|feature| feature_by_name(feature).is_none())
        .cloned()
        .collect();
    if !unsupported.is_empty() {
        bail!(
            "unsupported runtime feature mapping(s): {}",
            unsupported.join(", ")
        );
    }
    Ok(())
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

fn build_payload_variant(
    package: &Package,
    cargo_args: &CargoArgs,
    manifest_path: Option<&Utf8Path>,
    target: &str,
    _profile: &str,
    cpu: &str,
    out_root: &Utf8Path,
) -> Result<Utf8PathBuf> {
    let target_dir = out_root.join("variants").join(sanitize_cpu(cpu));
    fs::create_dir_all(&target_dir)?;
    let mut cmd = Command::new("cargo");
    cmd.arg("build");
    cmd.args(&cargo_args.forwarded);
    if let Some(manifest_path) = manifest_path {
        cmd.args(["--manifest-path", manifest_path.as_str()]);
    }
    if cargo_args.target.is_none() {
        cmd.args(["--target", target]);
    }
    cmd.args(["--message-format", "json-render-diagnostics"]);
    cmd.args(["--target-dir", target_dir.as_str()]);
    cmd.env("CARGO_ENCODED_RUSTFLAGS", encoded_rustflags(cpu));
    cmd.stdout(Stdio::piped());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn cargo for target-cpu `{cpu}`"))?;
    let stdout = child.stdout.take().context("failed to read cargo stdout")?;
    let reader = std::io::BufReader::new(stdout);
    let mut executables = Vec::new();
    for message in Message::parse_stream(reader) {
        if let Message::CompilerArtifact(Artifact {
            executable: Some(exe),
            target: artifact_target,
            package_id,
            ..
        }) = message?
            && package_id == package.id
            && artifact_target
                .kind
                .iter()
                .any(|k| matches!(k, cargo_metadata::TargetKind::Bin))
        {
            let exe = Utf8PathBuf::from_path_buf(exe.into_std_path_buf())
                .map_err(|_| anyhow!("artifact path is not valid UTF-8"))?;
            executables.push(exe);
        }
    }
    let status = child.wait()?;
    if !status.success() {
        bail!("cargo build failed for target-cpu `{cpu}`");
    }
    if executables.len() != 1 {
        bail!(
            "cannot identify exactly one executable artifact for target-cpu `{cpu}` (found {}); pass --bin",
            executables.len()
        );
    }
    let payload_dir = out_root.join("loader").join("payloads");
    fs::create_dir_all(&payload_dir)?;
    let payload = payload_dir.join(format!("{}.elf", sanitize_cpu(cpu)));
    fs::copy(&executables[0], &payload)?;
    Ok(payload)
}

fn encoded_rustflags(cpu: &str) -> OsString {
    let sep = '\x1f';
    let mut flags = std::env::var_os("CARGO_ENCODED_RUSTFLAGS").unwrap_or_default();
    if !flags.is_empty() {
        flags.push(sep.to_string());
    }
    flags.push(format!("-Ctarget-cpu={cpu}"));
    flags
}

fn sanitize_cpu(cpu: &str) -> String {
    cpu.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
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
    auditable_section: Option<&[u8]>,
) -> Result<()> {
    fs::create_dir_all(loader_dir.join("src"))?;
    fs::write(
        loader_dir.join("Cargo.toml"),
        r#"[package]
name = "sonic-generated-loader"
version = "0.0.0"
edition = "2024"

[profile.dev]
panic = "abort"

[profile.release]
panic = "abort"

[workspace]
"#,
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
        generated_manifest(variants),
    )?;
    fs::write(loader_dir.join("src/main.rs"), generated_main(target))?;
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
    let mut rustflags = loader_rustflags(target).to_string();
    if loader_dir.join("auditable.o").exists() {
        rustflags
            .push_str(" -C link-arg=auditable.o -C link-arg=-Wl,-u,__cargo_sonic_auditable_dep_v0");
    }
    cmd.env("RUSTFLAGS", rustflags);
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

fn loader_rustflags(target: &str) -> &'static str {
    if target.starts_with("x86_64-") {
        "-C panic=abort -C code-model=large -C target-feature=+crt-static -C relocation-model=static -C link-self-contained=no -C link-arg=-nostartfiles -C link-arg=-static"
    } else {
        "-C panic=abort -C target-feature=+crt-static -C relocation-model=static -C link-arg=-nostartfiles -C link-arg=-static"
    }
}

fn generated_manifest(variants: &[VariantBuild]) -> String {
    let mut out = String::new();
    out.push_str("use crate::feature_mask::FeatureMask;\nuse crate::select::TargetKind;\n\n");
    out.push_str("pub static ENV_ENABLED: &[u8] = b\"CARGO_SONIC_ENABLED=1\\0\";\n\n");
    out.push_str(
        "#[repr(align(131072))]\npub struct AlignedPayload<const N: usize>(pub [u8; N]);\n\n",
    );
    for v in variants {
        let payload_len = fs::metadata(&v.artifact)
            .map(|metadata| metadata.len())
            .unwrap_or_default();
        out.push_str(&format!(
            "static PAYLOAD_{}: AlignedPayload<{}> = AlignedPayload(*include_bytes!(\"../payloads/{}.elf\"));\n",
            sanitize_cpu(&v.target_cpu).to_ascii_uppercase(),
            payload_len,
            sanitize_cpu(&v.target_cpu)
        ));
    }
    out.push('\n');
    out.push_str("pub struct Variant {\n    pub target_cpu: &'static str,\n    pub required_features: FeatureMask,\n    pub rank_features: FeatureMask,\n    pub rank_feature_count: u16,\n    pub feature_tier: u8,\n    pub target_kind: TargetKind,\n    pub env_selected_target_cpu: &'static [u8],\n    pub env_selected_flags: &'static [u8],\n    pub payload: &'static [u8],\n}\n\n");
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
        out.push_str(&format!(
            "        payload: &PAYLOAD_{}.0,\n",
            sanitize_cpu(&v.target_cpu).to_ascii_uppercase()
        ));
        out.push_str("    },\n");
    }
    out.push_str("];\n");
    out
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

fn generated_main(_target: &str) -> &'static str {
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
    unsafe { exec_payload(selected, &initial) }
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
    unsafe {
        let _ = exec_reflink_tmpfile(selected, initial);
        let fd = linux_sys::memfd_create_best_effort(b"cargo-sonic-payload\0".as_ptr());
        if fd < 0 {
            linux_sys::exit(111);
        }
        if linux_sys::write_all(fd, selected.payload.as_ptr(), selected.payload.len()) < 0 {
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
    r#"use crate::linux_sys;

const AT_NULL: usize = 0;
const AT_PHDR: usize = 3;
const AT_HWCAP: usize = 16;
const AT_HWCAP2: usize = 26;
const AT_HWCAP3: usize = 29;

pub struct InitialStack {
    pub argc: usize,
    pub argv: *const *const u8,
    pub envp: *const *const u8,
    pub envc: usize,
    pub phdr: usize,
    pub hwcap: usize,
    pub hwcap2: usize,
    pub hwcap3: usize,
}

impl InitialStack {
    pub unsafe fn parse(sp: *const usize) -> Self {
        unsafe {
            let argc = *sp;
            let argv = sp.add(1) as *const *const u8;
            let envp = argv.add(argc + 1);
            let mut envc = 0;
            while !(*envp.add(envc)).is_null() {
                envc += 1;
            }
            let mut aux = envp.add(envc + 1) as *const usize;
            let mut phdr = 0;
            let mut hwcap = 0;
            let mut hwcap2 = 0;
            let mut hwcap3 = 0;
            while *aux != AT_NULL {
                let key = *aux;
                let val = *aux.add(1);
                if key == AT_PHDR { phdr = val; }
                if key == AT_HWCAP { hwcap = val; }
                if key == AT_HWCAP2 { hwcap2 = val; }
                if key == AT_HWCAP3 { hwcap3 = val; }
                aux = aux.add(2);
            }
            Self { argc, argv, envp, envc, phdr, hwcap, hwcap2, hwcap3 }
        }
    }
}

pub unsafe fn build_envp(initial: &InitialStack, enabled: &'static [u8], cpu: &'static [u8], flags: &'static [u8]) -> *const *const u8 {
    unsafe {
        let mut kept = 0;
        let mut i = 0;
        while i < initial.envc {
            let p = *initial.envp.add(i);
            if !is_sonic_key(p) {
                kept += 1;
            }
            i += 1;
        }
        let total = kept + 3 + 1;
        let bytes = total * core::mem::size_of::<*const u8>();
        let out = linux_sys::mmap(bytes) as *mut *const u8;
        if out.is_null() || out as isize == -1 {
            return core::ptr::null();
        }
        let mut j = 0;
        i = 0;
        while i < initial.envc {
            let p = *initial.envp.add(i);
            if !is_sonic_key(p) {
                *out.add(j) = p;
                j += 1;
            }
            i += 1;
        }
        *out.add(j) = enabled.as_ptr(); j += 1;
        *out.add(j) = cpu.as_ptr(); j += 1;
        *out.add(j) = flags.as_ptr(); j += 1;
        *out.add(j) = core::ptr::null();
        out
    }
}

pub unsafe fn debug_enabled(initial: &InitialStack) -> bool {
    unsafe {
        let mut i = 0;
        while i < initial.envc {
            if env_name_matches(*initial.envp.add(i), b"CARGO_SONIC_DEBUG") {
                return true;
            }
            i += 1;
        }
        false
    }
}

pub unsafe fn sonic_enabled(initial: &InitialStack) -> bool {
    unsafe {
        let mut i = 0;
        while i < initial.envc {
            let p = *initial.envp.add(i);
            if starts_with(p, b"CARGO_SONIC_ENABLE=") {
                let value = p.add(b"CARGO_SONIC_ENABLE=".len());
                return !is_disabled_value(value);
            }
            i += 1;
        }
        true
    }
}

unsafe fn is_sonic_key(p: *const u8) -> bool {
    unsafe {
        starts_with(p, b"CARGO_SONIC_ENABLED=")
            || starts_with(p, b"CARGO_SONIC_SELECTED_TARGET_CPU=")
            || starts_with(p, b"CARGO_SONIC_SELECTED_FLAGS=")
    }
}

unsafe fn env_name_matches(p: *const u8, name: &[u8]) -> bool {
    unsafe {
        let mut i = 0;
        while i < name.len() {
            if *p.add(i) != name[i] {
                return false;
            }
            i += 1;
        }
        let next = *p.add(i);
        next == 0 || next == b'='
    }
}

unsafe fn is_disabled_value(p: *const u8) -> bool {
    unsafe {
        if *p == b'0' && *p.add(1) == 0 {
            return true;
        }
        (*p == b'f' || *p == b'F')
            && (*p.add(1) == b'a' || *p.add(1) == b'A')
            && (*p.add(2) == b'l' || *p.add(2) == b'L')
            && (*p.add(3) == b's' || *p.add(3) == b'S')
            && (*p.add(4) == b'e' || *p.add(4) == b'E')
            && *p.add(5) == 0
    }
}

unsafe fn starts_with(mut p: *const u8, prefix: &[u8]) -> bool {
    unsafe {
        let mut i = 0;
        while i < prefix.len() {
            if *p != prefix[i] {
                return false;
            }
            p = p.add(1);
            i += 1;
        }
        true
    }
}
"#
}

fn escape_bytes(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn feature_tier(arch: &str, mask: FeatureMask) -> u8 {
    if arch == "x86_64" {
        if has_any(
            mask,
            &[
                Feature::Avx512Vnni,
                Feature::Avx512Bf16,
                Feature::Avx512Fp16,
            ],
        ) {
            5
        } else if mask.contains(Feature::Avx512F)
            && (mask.contains(Feature::Avx512Bw)
                || mask.contains(Feature::Avx512Dq)
                || mask.contains(Feature::Avx512Vl))
        {
            4
        } else if has_any(mask, &[Feature::Avx2, Feature::Bmi1, Feature::Bmi2]) {
            3
        } else if has_any(mask, &[Feature::Avx, Feature::Fma]) {
            2
        } else if has_any(
            mask,
            &[
                Feature::Sse3,
                Feature::Ssse3,
                Feature::Sse4_1,
                Feature::Sse4_2,
            ],
        ) {
            1
        } else {
            0
        }
    } else if has_any(mask, &[Feature::Sve2]) {
        5
    } else if mask.contains(Feature::Sve) {
        4
    } else if has_any(mask, &[Feature::Bf16, Feature::I8mm]) {
        3
    } else if has_any(
        mask,
        &[Feature::Lse, Feature::Fp16, Feature::Dotprod, Feature::Rdm],
    ) {
        2
    } else if has_any(
        mask,
        &[Feature::Crc, Feature::Aes, Feature::Sha2, Feature::Sha3],
    ) {
        1
    } else {
        0
    }
}

fn has_any(mask: FeatureMask, features: &[Feature]) -> bool {
    features.iter().any(|feature| mask.contains(*feature))
}

fn classify_target_cpu(cpu: &str, arch: &str) -> TargetKind {
    if cpu == "generic" {
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
                || c == "grandridge" =>
            {
                TargetKind::X86IntelAtom
            }
            c if c.contains("lake")
                || c.contains("well")
                || c.contains("bridge")
                || matches!(c, "nocona" | "core2" | "penryn" | "nehalem" | "westmere") =>
            {
                TargetKind::X86IntelCore
            }
            c if c.contains("rapids") || c.contains("skx") || c.contains("skylake-avx512") => {
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

#[cfg(test)]
mod tests {
    use super::*;

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
            filter_runtime_features(&["crt-static".into(), "x87".into(), "avx2".into()]),
            vec!["avx2"]
        );
    }

    #[test]
    fn skylake_style_features_with_x87_are_supported() {
        let features = filter_runtime_features(&[
            "x87".into(),
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
        unsupported_runtime_features(&features).unwrap();
        assert!(!features.iter().any(|feature| feature == "x87"));
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
    fn musl_loader_rustflags_skip_startup_files() {
        let flags = loader_rustflags("aarch64-unknown-linux-musl");
        assert!(flags.contains("-C link-arg=-nostartfiles"));
    }

    #[test]
    fn x86_64_loader_rustflags_use_large_code_model() {
        let flags = loader_rustflags("x86_64-unknown-linux-gnu");
        assert!(flags.contains("-C code-model=large"));
    }

    #[test]
    fn generated_aarch64_loader_does_not_read_midr_el1_before_fallbacks() {
        let source = generated_main("aarch64-unknown-linux-musl");
        assert!(source.contains("read_small_file_hex"));
        assert!(!source.contains("let midr = unsafe { read_midr_el1() };"));
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
        assert!(normalize_target_cpus(vec!["native".into()]).is_err());
    }

    #[test]
    fn generic_is_implicit_in_config() {
        assert_eq!(
            normalize_target_cpus(vec!["haswell".into()]).unwrap(),
            vec!["generic", "haswell"]
        );
    }

    #[test]
    fn explicit_generic_target_cpu_is_rejected() {
        let err = normalize_target_cpus(vec!["generic".into()]).unwrap_err();
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
        let current = BTreeSet::from(["generic".into(), "x86-64".into(), "x86-64-v2".into()]);
        let union = current.clone();

        let got =
            filter_target_cpus(&["generic".into(), "x86-64".into()], &current, &union).unwrap();
        assert_eq!(got, vec!["generic", "x86-64"]);

        let err = filter_target_cpus(&["generic".into(), "x86-64-v1".into()], &current, &union)
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
    fn unsupported_runtime_feature_mapping_is_build_error() {
        assert!(unsupported_runtime_features(&["not-real".into()]).is_err());
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
            classify_target_cpu("sierraforest", "x86_64"),
            TargetKind::X86IntelAtom
        );
        assert_eq!(
            classify_target_cpu("clearwaterforest", "x86_64"),
            TargetKind::X86IntelAtom
        );
        assert_eq!(
            classify_target_cpu("grandridge", "x86_64"),
            TargetKind::X86IntelAtom
        );
        assert_eq!(
            classify_target_cpu("sapphirerapids", "x86_64"),
            TargetKind::X86IntelXeon
        );
        assert_eq!(
            classify_target_cpu("graniterapids-d", "x86_64"),
            TargetKind::X86IntelXeon
        );
        assert_eq!(
            classify_target_cpu("arrowlake", "x86_64"),
            TargetKind::X86IntelCore
        );
        assert_eq!(
            classify_target_cpu("znver5", "x86_64"),
            TargetKind::X86AmdZen { generation: 5 }
        );
    }

    #[test]
    fn classifies_modern_aarch64_target_cpus_for_selector_affinity() {
        assert_eq!(
            classify_target_cpu("cortex-a725", "aarch64"),
            TargetKind::Aarch64ArmCortexA
        );
        assert_eq!(
            classify_target_cpu("neoverse-n3", "aarch64"),
            TargetKind::Aarch64ArmNeoverseN
        );
        assert_eq!(
            classify_target_cpu("neoverse-v3ae", "aarch64"),
            TargetKind::Aarch64ArmNeoverseV
        );
        assert_eq!(
            classify_target_cpu("a64fx", "aarch64"),
            TargetKind::Aarch64Other
        );
    }

    #[test]
    fn builds_and_runs_generic_fixture() {
        if !cfg!(target_os = "linux") || !cfg!(target_arch = "x86_64") {
            return;
        }
        let manifest = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/env-printer/Cargo.toml");
        let output = build(BuildOptions {
            cargo_args: Vec::new(),
            manifest_path: Some(manifest),
            target_cpus: vec!["x86-64".into()],
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
        assert!(stdout.contains("cpu=generic"));
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

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }
}
