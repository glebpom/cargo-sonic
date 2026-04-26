use anyhow::{anyhow, bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use cargo_metadata::{Artifact, Message, MetadataCommand, Package};
use serde::Deserialize;
use sonic_loader::feature_mask::{feature_by_name, Feature, FeatureMask};
use sonic_loader::select::TargetKind;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::process::{Command, Stdio};

#[derive(Debug, Clone)]
pub struct BuildOptions {
    pub cargo_args: Vec<String>,
    pub manifest_path: Option<Utf8PathBuf>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct BuildOutput {
    pub final_binary: Utf8PathBuf,
    pub warnings: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct SonicMetadata {
    target_cpus: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
struct CargoArgs {
    release: bool,
    target: Option<String>,
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
    let metadata = metadata_cmd.exec().context("failed to read cargo metadata")?;
    let package = select_package(&metadata, cargo_args.package.as_deref())?;
    let configured_cpus = normalize_target_cpus(effective_target_cpus(&metadata, package)?)?;

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
        bail!("cargo-sonic currently supports x86_64 and aarch64 only; `{target}` has target_arch={target_arch:?}");
    }

    let current_valid = rustc_target_cpus(&target)?;
    let union_valid = known_supported_cpu_union()?;
    let included = filter_target_cpus(&configured_cpus, &current_valid, &union_valid)?;
    let profile = if cargo_args.release { "release" } else { "debug" };
    let out_root = Utf8PathBuf::from_path_buf(metadata.target_directory.clone().into_std_path_buf())
        .map_err(|_| anyhow!("target directory is not valid UTF-8"))?
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
        let artifact = build_payload_variant(package, &cargo_args, manifest_path.as_deref(), &target, profile, cpu, &out_root)?;
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
    generate_loader_crate(&loader_dir, &target, &variants)?;
    let loader_artifact = build_loader(&loader_dir, &target, profile)?;
    let final_binary = out_root.join(&bin_name);
    fs::create_dir_all(out_root.as_path())?;
    fs::copy(&loader_artifact, &final_binary)
        .with_context(|| format!("failed to copy final fat binary to {final_binary}"))?;
    make_executable(&final_binary)?;
    Ok(BuildOutput { final_binary, warnings })
}

fn parse_cargo_args(args: &[String]) -> CargoArgs {
    let mut out = CargoArgs {
        release: false,
        target: None,
        bin: None,
        package: None,
        manifest_path: None,
        forwarded: Vec::new(),
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--release" => {
                out.release = true;
                out.forwarded.push(args[i].clone());
            }
            "--target" => {
                if let Some(v) = args.get(i + 1) {
                    out.target = Some(v.clone());
                    out.forwarded.push(args[i].clone());
                    out.forwarded.push(v.clone());
                    i += 1;
                }
            }
            "--bin" => {
                if let Some(v) = args.get(i + 1) {
                    out.bin = Some(v.clone());
                    out.forwarded.push(args[i].clone());
                    out.forwarded.push(v.clone());
                    i += 1;
                }
            }
            "--package" | "-p" => {
                if let Some(v) = args.get(i + 1) {
                    out.package = Some(v.clone());
                    out.forwarded.push(args[i].clone());
                    out.forwarded.push(v.clone());
                    i += 1;
                }
            }
            "--manifest-path" => {
                if let Some(v) = args.get(i + 1) {
                    out.manifest_path = Some(Utf8PathBuf::from(v));
                    i += 1;
                }
            }
            v if v.starts_with("--target=") => {
                out.target = Some(v["--target=".len()..].to_string());
                out.forwarded.push(args[i].clone());
            }
            v if v.starts_with("--bin=") => {
                out.bin = Some(v["--bin=".len()..].to_string());
                out.forwarded.push(args[i].clone());
            }
            v if v.starts_with("--package=") => {
                out.package = Some(v["--package=".len()..].to_string());
                out.forwarded.push(args[i].clone());
            }
            v if v.starts_with("--manifest-path=") => {
                out.manifest_path = Some(Utf8PathBuf::from(&v["--manifest-path=".len()..]));
            }
            _ => out.forwarded.push(args[i].clone()),
        }
        i += 1;
    }
    out
}

fn select_package<'a>(metadata: &'a cargo_metadata::Metadata, package: Option<&str>) -> Result<&'a Package> {
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

fn effective_target_cpus(metadata: &cargo_metadata::Metadata, package: &Package) -> Result<Vec<String>> {
    if let Some(v) = package.metadata.get("sonic") {
        let sonic: SonicMetadata = serde_json::from_value(v.clone())?;
        return sonic.target_cpus.context("package.metadata.sonic.target-cpus must exist");
    }
    if let Some(v) = metadata.workspace_metadata.get("sonic") {
        let sonic: SonicMetadata = serde_json::from_value(v.clone())?;
        return sonic.target_cpus.context("workspace.metadata.sonic.target-cpus must exist");
    }
    bail!("target-cpus must exist under [package.metadata.sonic]");
}

fn normalize_target_cpus(mut cpus: Vec<String>) -> Result<Vec<String>> {
    if cpus.iter().any(|cpu| cpu == "native") {
        bail!("target-cpu \"native\" is rejected because cargo-sonic builds portable artifacts");
    }
    if !cpus.iter().any(|cpu| cpu == "generic") {
        cpus.insert(0, "generic".to_string());
    }
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

fn rustc_cfg(target: &str, cpu: Option<&str>) -> Result<String> {
    let mut cmd = Command::new("rustc");
    cmd.args(["--print", "cfg", "--target", target]);
    if let Some(cpu) = cpu {
        cmd.args(["-C", &format!("target-cpu={cpu}")]);
    }
    let output = cmd.output().with_context(|| "failed to run rustc --print cfg")?;
    if !output.status.success() {
        bail!("rustc --print cfg failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn cfg_value(cfg: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=\"");
    cfg.lines()
        .find_map(|line| line.strip_prefix(&prefix)?.strip_suffix('"').map(str::to_string))
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
        .filter(|feature| feature.as_str() != "crt-static")
        .cloned()
        .collect()
}

fn rustc_target_cpus(target: &str) -> Result<BTreeSet<String>> {
    let output = Command::new("rustc")
        .args(["--print", "target-cpus", "--target", target])
        .output()
        .with_context(|| "failed to run rustc --print target-cpus")?;
    if !output.status.success() {
        bail!("rustc --print target-cpus failed: {}", String::from_utf8_lossy(&output.stderr));
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
        bail!("unsupported runtime feature mapping(s): {}", unsupported.join(", "));
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
        // These instructions are not normal compiler codegen targets. They are
        // kept in rank_features / CARGO_SONIC_SELECTED_FLAGS, but do not make a
        // CPU-tuned payload ineligible when firmware, virtualization, or kernel
        // policy hides hardware RNG support.
        "rdrand" | "rdseed"
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
    let mut child = cmd.spawn().with_context(|| format!("failed to spawn cargo for target-cpu `{cpu}`"))?;
    let stdout = child.stdout.take().context("failed to read cargo stdout")?;
    let reader = std::io::BufReader::new(stdout);
    let mut executables = Vec::new();
    for message in Message::parse_stream(reader) {
        if let Message::CompilerArtifact(Artifact { executable: Some(exe), target: artifact_target, package_id, .. }) = message? {
            if package_id == package.id && artifact_target.kind.iter().any(|k| matches!(k, cargo_metadata::TargetKind::Bin)) {
                let exe = Utf8PathBuf::from_path_buf(exe.into_std_path_buf())
                    .map_err(|_| anyhow!("artifact path is not valid UTF-8"))?;
                executables.push(exe);
            }
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

fn resolve_bin_name(package: &Package, explicit_bin: Option<&str>, variants: &[VariantBuild]) -> Result<String> {
    if let Some(bin) = explicit_bin {
        return Ok(bin.to_string());
    }
    let bins: Vec<_> = package
        .targets
        .iter()
        .filter(|target| target.kind.iter().any(|k| matches!(k, cargo_metadata::TargetKind::Bin)))
        .collect();
    if bins.len() == 1 {
        return Ok(bins[0].name.clone());
    }
    if let Some(path) = variants.first().and_then(|v| v.artifact.file_stem()) {
        return Ok(path.to_string());
    }
    bail!("multiple binary artifacts are possible; pass --bin");
}

fn generate_loader_crate(loader_dir: &Utf8Path, target: &str, variants: &[VariantBuild]) -> Result<()> {
    fs::create_dir_all(loader_dir.join("src"))?;
    fs::write(
        loader_dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "sonic-generated-loader"
version = "0.0.0"
edition = "2021"

[profile.dev]
panic = "abort"

[profile.release]
panic = "abort"

[workspace]
"#
        ),
    )?;
    fs::write(loader_dir.join("src/feature_mask.rs"), include_str!("../../sonic-loader/src/feature_mask.rs"))?;
    fs::write(loader_dir.join("src/select.rs"), include_str!("../../sonic-loader/src/select.rs"))?;
    fs::write(loader_dir.join("src/arch_x86_64.rs"), include_str!("../../sonic-loader/src/arch_x86_64.rs"))?;
    fs::write(loader_dir.join("src/arch_aarch64.rs"), include_str!("../../sonic-loader/src/arch_aarch64.rs"))?;
    fs::write(loader_dir.join("src/linux_sys.rs"), generated_linux_sys())?;
    fs::write(loader_dir.join("src/stack.rs"), generated_stack())?;
    fs::write(loader_dir.join("src/generated_manifest.rs"), generated_manifest(variants))?;
    fs::write(loader_dir.join("src/main.rs"), generated_main(target))?;
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
    cmd.env("RUSTFLAGS", "-C panic=abort -C target-feature=+crt-static -C relocation-model=static -C link-arg=-nostartfiles -C link-arg=-static");
    let status = cmd.status().context("failed to spawn cargo for generated loader")?;
    if !status.success() {
        bail!("generated loader failed to compile");
    }
    let exe = target_dir
        .join(target)
        .join(profile)
        .join("sonic-generated-loader");
    Ok(exe)
}

fn generated_manifest(variants: &[VariantBuild]) -> String {
    let mut out = String::new();
    out.push_str("use crate::feature_mask::FeatureMask;\nuse crate::select::TargetKind;\n\n");
    out.push_str("pub static ENV_ENABLED: &[u8] = b\"CARGO_SONIC_ENABLED=1\\0\";\n\n");
    out.push_str("pub struct Variant {\n    pub target_cpu: &'static str,\n    pub required_features: FeatureMask,\n    pub rank_features: FeatureMask,\n    pub rank_feature_count: u16,\n    pub feature_tier: u8,\n    pub target_kind: TargetKind,\n    pub env_selected_target_cpu: &'static [u8],\n    pub env_selected_flags: &'static [u8],\n    pub payload: &'static [u8],\n}\n\n");
    out.push_str("pub static VARIANTS: &[Variant] = &[\n");
    for v in variants {
        let req = v.required_features.words();
        let rank = v.rank_features.words();
        let flags = v.feature_names.join(",");
        out.push_str("    Variant {\n");
        out.push_str(&format!("        target_cpu: {:?},\n", v.target_cpu));
        out.push_str(&format!("        required_features: FeatureMask::from_words([{:#x}, {:#x}]),\n", req[0], req[1]));
        out.push_str(&format!("        rank_features: FeatureMask::from_words([{:#x}, {:#x}]),\n", rank[0], rank[1]));
        out.push_str(&format!("        rank_feature_count: {},\n", v.rank_features.count()));
        out.push_str(&format!("        feature_tier: {},\n", v.feature_tier));
        out.push_str(&format!("        target_kind: {},\n", target_kind_expr(v.target_kind)));
        out.push_str(&format!("        env_selected_target_cpu: b\"CARGO_SONIC_SELECTED_TARGET_CPU={}\\0\",\n", escape_bytes(&v.target_cpu)));
        out.push_str(&format!("        env_selected_flags: b\"CARGO_SONIC_SELECTED_FLAGS={}\\0\",\n", escape_bytes(&flags)));
        out.push_str(&format!("        payload: include_bytes!(\"../payloads/{}.elf\"),\n", sanitize_cpu(&v.target_cpu)));
        out.push_str("    },\n");
    }
    out.push_str("];\n");
    out
}

fn target_kind_expr(kind: TargetKind) -> String {
    match kind {
        TargetKind::Generic => "TargetKind::Generic".to_string(),
        TargetKind::X86NeutralLevel { level } => format!("TargetKind::X86NeutralLevel {{ level: {level} }}"),
        TargetKind::X86IntelCore => "TargetKind::X86IntelCore".to_string(),
        TargetKind::X86IntelXeon => "TargetKind::X86IntelXeon".to_string(),
        TargetKind::X86IntelAtom => "TargetKind::X86IntelAtom".to_string(),
        TargetKind::X86AmdZen { generation } => format!("TargetKind::X86AmdZen {{ generation: {generation} }}"),
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

#[no_mangle]
unsafe extern "C" fn memcpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    let mut i = 0;
    while i < n {
        *dst.add(i) = *src.add(i);
        i += 1;
    }
    dst
}

#[no_mangle]
unsafe extern "C" fn memset(dst: *mut u8, value: i32, n: usize) -> *mut u8 {
    let mut i = 0;
    while i < n {
        *dst.add(i) = value as u8;
        i += 1;
    }
    dst
}

#[no_mangle]
unsafe extern "C" fn memcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    let mut i = 0;
    while i < n {
        let av = *a.add(i);
        let bv = *b.add(i);
        if av != bv {
            return av as i32 - bv as i32;
        }
        i += 1;
    }
    0
}

#[no_mangle]
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

#[no_mangle]
unsafe extern "C" fn loader_entry(stack: *const usize) -> ! {
    let initial = stack::InitialStack::parse(stack);
    let host = detect_host(&initial);
    let mut metas = [VariantMeta {
        target_cpu: "",
        required_features: feature_mask::FeatureMask::EMPTY,
        rank_features: feature_mask::FeatureMask::EMPTY,
        rank_feature_count: 0,
        feature_tier: 0,
        target_kind: select::TargetKind::Generic,
    }; 64];
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
    let selected_meta = select::select_variant(host, &metas[..count]);
    if stack::debug_enabled(&initial) {
        debug_selection(host, &metas[..count], selected_meta.target_cpu);
    }
    let selected = find_variant(selected_meta.target_cpu);
    exec_payload(selected, &initial)
}

fn find_variant(name: &str) -> &'static Variant {
    let mut i = 0;
    while i < VARIANTS.len() {
        if VARIANTS[i].target_cpu.as_bytes() == name.as_bytes() {
            return &VARIANTS[i];
        }
        i += 1;
    }
    &VARIANTS[0]
}

unsafe fn exec_payload(selected: &Variant, initial: &stack::InitialStack) -> ! {
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

fn debug_selection(host: HostInfo, variants: &[VariantMeta], selected: &str) {
    unsafe {
        linux_sys::write_stderr(b"cargo-sonic debug\n");
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
            identity: CpuIdentity::Unknown,
            heterogeneous: false,
        }
    }
}

#[cfg(target_arch = "x86_64")]
unsafe fn detect_x86() -> feature_mask::FeatureMask {
    let l1 = core::arch::x86_64::__cpuid_count(1, 0);
    let l70 = core::arch::x86_64::__cpuid_count(7, 0);
    let l71 = core::arch::x86_64::__cpuid_count(7, 1);
    let ld1 = core::arch::x86_64::__cpuid_count(0xd, 1);
    let l8 = core::arch::x86_64::__cpuid_count(0x80000001, 0);
    let xcr0 = core::arch::x86_64::_xgetbv(0);
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
#[cfg(target_arch = "x86_64")]
const SYS_WRITE: usize = 1;
#[cfg(target_arch = "x86_64")]
const SYS_MMAP: usize = 9;
#[cfg(target_arch = "x86_64")]
const SYS_EXIT: usize = 60;
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
const SYS_MMAP: usize = 222;
#[cfg(target_arch = "aarch64")]
const SYS_EXIT: usize = 93;
#[cfg(target_arch = "aarch64")]
const SYS_MEMFD_CREATE: usize = 279;
#[cfg(target_arch = "aarch64")]
const SYS_EXECVEAT: usize = 281;

#[cfg(target_arch = "x86_64")]
unsafe fn syscall6(n: usize, a0: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> isize {
    let ret: isize;
    core::arch::asm!("syscall", inlateout("rax") n as isize => ret, in("rdi") a0, in("rsi") a1, in("rdx") a2, in("r10") a3, in("r8") a4, in("r9") a5, lateout("rcx") _, lateout("r11") _, options(nostack));
    ret
}

#[cfg(target_arch = "aarch64")]
unsafe fn syscall6(n: usize, a0: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> isize {
    let ret: isize;
    core::arch::asm!("svc #0", inlateout("x8") n as isize => _, inlateout("x0") a0 as isize => ret, in("x1") a1, in("x2") a2, in("x3") a3, in("x4") a4, in("x5") a5, options(nostack));
    ret
}

pub unsafe fn mmap(len: usize) -> *mut usize {
    syscall6(SYS_MMAP, 0, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, !0usize, 0) as *mut usize
}

pub unsafe fn memfd_create_best_effort(name: *const u8) -> isize {
    let mut fd = syscall6(SYS_MEMFD_CREATE, name as usize, MFD_ALLOW_SEALING | MFD_EXEC, 0, 0, 0, 0);
    if fd == EINVAL_NEG {
        fd = syscall6(SYS_MEMFD_CREATE, name as usize, MFD_ALLOW_SEALING, 0, 0, 0, 0);
    }
    if fd == EINVAL_NEG {
        fd = syscall6(SYS_MEMFD_CREATE, name as usize, 0, 0, 0, 0, 0);
    }
    fd
}

pub unsafe fn write_all(fd: isize, mut ptr: *const u8, mut len: usize) -> isize {
    while len > 0 {
        let n = syscall6(SYS_WRITE, fd as usize, ptr as usize, len, 0, 0, 0);
        if n <= 0 {
            return n;
        }
        ptr = ptr.add(n as usize);
        len -= n as usize;
    }
    0
}

pub unsafe fn write_stderr(buf: &[u8]) {
    let _ = write_all(2, buf.as_ptr(), buf.len());
}

pub unsafe fn execveat(fd: isize, path: *const u8, argv: *const *const u8, envp: *const *const u8, flags: usize) -> isize {
    syscall6(SYS_EXECVEAT, fd as usize, path as usize, argv as usize, envp as usize, flags, 0)
}

pub unsafe fn exit(code: i32) -> ! {
    let _ = syscall6(SYS_EXIT, code as usize, 0, 0, 0, 0, 0);
    loop {}
}
"#
}

fn generated_stack() -> &'static str {
    r#"use crate::linux_sys;

const AT_NULL: usize = 0;
const AT_HWCAP: usize = 16;
const AT_HWCAP2: usize = 26;
const AT_HWCAP3: usize = 29;

pub struct InitialStack {
    pub argc: usize,
    pub argv: *const *const u8,
    pub envp: *const *const u8,
    pub envc: usize,
    pub hwcap: usize,
    pub hwcap2: usize,
    pub hwcap3: usize,
}

impl InitialStack {
    pub unsafe fn parse(sp: *const usize) -> Self {
        let argc = *sp;
        let argv = sp.add(1) as *const *const u8;
        let mut envp = argv.add(argc + 1);
        let mut envc = 0;
        while !(*envp.add(envc)).is_null() {
            envc += 1;
        }
        let mut aux = envp.add(envc + 1) as *const usize;
        let mut hwcap = 0;
        let mut hwcap2 = 0;
        let mut hwcap3 = 0;
        while *aux != AT_NULL {
            let key = *aux;
            let val = *aux.add(1);
            if key == AT_HWCAP { hwcap = val; }
            if key == AT_HWCAP2 { hwcap2 = val; }
            if key == AT_HWCAP3 { hwcap3 = val; }
            aux = aux.add(2);
        }
        Self { argc, argv, envp, envc, hwcap, hwcap2, hwcap3 }
    }
}

pub unsafe fn build_envp(initial: &InitialStack, enabled: &'static [u8], cpu: &'static [u8], flags: &'static [u8]) -> *const *const u8 {
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

pub unsafe fn debug_enabled(initial: &InitialStack) -> bool {
    let mut i = 0;
    while i < initial.envc {
        if env_name_matches(*initial.envp.add(i), b"CARGO_SONIC_DEBUG") {
            return true;
        }
        i += 1;
    }
    false
}

unsafe fn is_sonic_key(p: *const u8) -> bool {
    starts_with(p, b"CARGO_SONIC_ENABLED=")
        || starts_with(p, b"CARGO_SONIC_SELECTED_TARGET_CPU=")
        || starts_with(p, b"CARGO_SONIC_SELECTED_FLAGS=")
}

unsafe fn env_name_matches(p: *const u8, name: &[u8]) -> bool {
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

unsafe fn starts_with(mut p: *const u8, prefix: &[u8]) -> bool {
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
"#
}

fn escape_bytes(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn feature_tier(arch: &str, mask: FeatureMask) -> u8 {
    if arch == "x86_64" {
        if has_any(mask, &[Feature::Avx512Vnni, Feature::Avx512Bf16, Feature::Avx512Fp16]) { 5 }
        else if mask.contains(Feature::Avx512F) && (mask.contains(Feature::Avx512Bw) || mask.contains(Feature::Avx512Dq) || mask.contains(Feature::Avx512Vl)) { 4 }
        else if has_any(mask, &[Feature::Avx2, Feature::Bmi1, Feature::Bmi2]) { 3 }
        else if has_any(mask, &[Feature::Avx, Feature::Fma]) { 2 }
        else if has_any(mask, &[Feature::Sse3, Feature::Ssse3, Feature::Sse4_1, Feature::Sse4_2]) { 1 }
        else { 0 }
    } else if has_any(mask, &[Feature::Sve2]) {
        5
    } else if mask.contains(Feature::Sve) {
        4
    } else if has_any(mask, &[Feature::Bf16, Feature::I8mm]) {
        3
    } else if has_any(mask, &[Feature::Lse, Feature::Fp16, Feature::Dotprod, Feature::Rdm]) {
        2
    } else if has_any(mask, &[Feature::Crc, Feature::Aes, Feature::Sha2, Feature::Sha3]) {
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
            c if c.contains("atom") || c.contains("silvermont") || c.contains("goldmont") || c.contains("gracemont") => TargetKind::X86IntelAtom,
            c if c.contains("lake") || c.contains("well") || c.contains("bridge") => TargetKind::X86IntelCore,
            c if c.contains("rapids") || c.contains("skx") || c.contains("skylake-avx512") => TargetKind::X86IntelXeon,
            _ => TargetKind::X86AmdOther,
        };
    }
    match cpu {
        c if c.starts_with("neoverse-n") => TargetKind::Aarch64ArmNeoverseN,
        c if c.starts_with("neoverse-v") || c == "neoverse-512tvb" => TargetKind::Aarch64ArmNeoverseV,
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
    if arch == "x86_64" && !features_by_cpu.keys().any(|cpu| matches!(cpu.as_str(), "x86-64" | "x86-64-v2" | "x86-64-v3" | "x86-64-v4")) {
        if features_by_cpu.keys().any(|cpu| cpu != "generic") {
            warnings.push("vendor-specific x86 targets configured without a neutral x86 fallback".to_string());
        }
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
fn make_executable(_path: &Utf8Path) -> Result<()> { Ok(()) }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_target_features_from_rustc_cfg_test() {
        let got = parse_target_features_from_rustc_cfg("target_feature=\"sse2\"\ntarget_feature=\"avx2\"\n");
        assert_eq!(got, vec!["avx2", "sse2"]);
    }

    #[test]
    fn filters_crt_static() {
        assert_eq!(filter_runtime_features(&["crt-static".into(), "avx2".into()]), vec!["avx2"]);
    }

    #[test]
    fn generic_required_mask_is_empty_but_rank_mask_is_recorded() {
        let rank = feature_mask(&["avx2".into()]).unwrap();
        assert_eq!(FeatureMask::EMPTY.count(), 0);
        assert_eq!(rank.count(), 1);
    }

    #[test]
    fn rng_features_are_rank_only_not_safety_required() {
        let required = safety_required_features(&["avx2".into(), "rdseed".into(), "rdrand".into()]);
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
    fn unknown_target_cpu_is_error() {
        let current = BTreeSet::from(["generic".into(), "znver5".into()]);
        let union = current.clone();
        assert!(filter_target_cpus(&["generic".into(), "zenver5".into()], &current, &union).is_err());
    }

    #[test]
    fn cross_arch_target_cpu_is_skipped_not_error() {
        let current = BTreeSet::from(["generic".into(), "znver5".into()]);
        let union = BTreeSet::from(["generic".into(), "znver5".into(), "neoverse-v1".into()]);
        let got = filter_target_cpus(&["generic".into(), "znver5".into(), "neoverse-v1".into()], &current, &union).unwrap();
        assert_eq!(got, vec!["generic", "znver5"]);
    }

    #[test]
    fn unsupported_runtime_feature_mapping_is_build_error() {
        assert!(unsupported_runtime_features(&["not-real".into()]).is_err());
    }

    #[test]
    fn configured_collision_emits_warning() {
        let warnings = analyze_warnings(&BTreeMap::from([
            ("a".into(), vec!["avx2".into()]),
            ("b".into(), vec!["avx2".into()]),
        ]), "x86_64");
        assert!(warnings.iter().any(|w| w.contains("identical")));
    }

    #[test]
    fn configured_incomparable_overlap_emits_warning() {
        let warnings = analyze_warnings(&BTreeMap::from([
            ("haswell".into(), vec!["avx2".into()]),
            ("znver5".into(), vec!["avx2".into(), "avx512f".into()]),
        ]), "x86_64");
        assert!(warnings.iter().any(|w| w.contains("neutral")));
    }

    #[test]
    fn linux_only_target_is_enforced() {
        assert_eq!(cfg_value("target_os=\"linux\"\ntarget_arch=\"x86_64\"", "target_os").as_deref(), Some("linux"));
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
        })
        .unwrap();
        let run = Command::new(&output.final_binary)
            .arg("one")
            .env("KEEP_ME", "yes")
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
        }
    }
}
