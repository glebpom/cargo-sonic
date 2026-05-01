use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

const QEMU_ASSET_SUBDIR: &str = "sonic-qemu-system";

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("qemu") => qemu_system(),
        Some("qemu-prepare") => qemu_prepare(),
        Some("integration") => integration(),
        _ => {
            eprintln!("usage: cargo xtask <qemu|qemu-prepare|integration>");
            Ok(())
        }
    }
}

fn qemu_prepare() -> Result<()> {
    let root = repo_root();
    let asset_dir = qemu_asset_dir(&root)?;
    fs::create_dir_all(&asset_dir)
        .with_context(|| format!("failed to create {}", asset_dir.display()))?;

    let manifest = read_targets(&root.join("tests/qemu/system.toml"))?;
    prepare_qemu(&asset_dir, &manifest)?;
    prepare_guests(&asset_dir, &manifest)?;
    write_qemu_readme(&asset_dir, &manifest)?;

    println!("prepared {}", asset_dir.display());
    Ok(())
}

fn qemu_system() -> Result<()> {
    let root = repo_root();
    let asset_dir = qemu_asset_dir(&root)?;
    let manifest = read_targets(&root.join("tests/qemu/system.toml"))?;
    let case_filter = std::env::var("SONIC_QEMU_CASE").ok();

    let missing = missing_system_assets(&asset_dir, &manifest);
    if !missing.is_empty() {
        bail!(
            "system-mode QEMU assets are incomplete under {}\nmissing:\n{}\n\nrun `just qemu-prepare` after tests/qemu/system.toml contains pinned downloadable qemu/guest image URLs",
            asset_dir.display(),
            missing
                .iter()
                .map(|item| format!("  - {item}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    prepare_runtime_rootfs(&asset_dir, &manifest)?;

    let jobs = qemu_jobs();
    println!("qemu jobs: {jobs}");

    let host_target = host_target_triple()?;
    let cargo_sonic = build_cargo_sonic_driver(&root, &asset_dir)?;
    let mut prepared = Vec::new();
    for arch in &manifest.arch {
        let cases = filtered_cases(arch, case_filter.as_deref());
        if cases.is_empty() {
            continue;
        }
        let variants = rustc_payload_target_cpus(&asset_dir, arch, &cases)?;
        ensure_qemu_build_variants_available(arch, &host_target)?;
        let mut apps = Vec::new();
        for build_variant in qemu_build_variants(arch, &host_target) {
            apps.push(build_qemu_test_app(
                &root,
                &asset_dir,
                &cargo_sonic,
                arch,
                build_variant,
                &variants,
            )?);
        }
        if apps.is_empty() {
            bail!("no qemu build variants are available for {}", arch.name);
        }
        let initrd = build_test_initramfs(&asset_dir, arch, &apps)?;
        prepared.push(PreparedQemuArch {
            arch,
            variant: "matrix".to_string(),
            initrd,
        });
    }

    let mut case_jobs = Vec::new();
    for prepared_arch in &prepared {
        for case in filtered_cases(prepared_arch.arch, case_filter.as_deref()) {
            case_jobs.push(QemuCaseJob {
                arch: prepared_arch.arch,
                variant: &prepared_arch.variant,
                case,
                initrd: &prepared_arch.initrd,
            });
        }
    }
    if case_jobs.is_empty() {
        bail!(
            "no qemu cases matched{}",
            case_filter
                .as_deref()
                .map(|filter| format!(" SONIC_QEMU_CASE={filter:?}"))
                .unwrap_or_default()
        );
    }

    let mut passed = 0usize;
    let mut failed_reports = Vec::new();
    for result in run_qemu_cases_parallel(&asset_dir, &case_jobs, jobs) {
        match result.outcome {
            Ok(()) => {
                passed += 1;
                println!("ok: {} {} {}", result.arch, result.variant, result.cpu);
            }
            Err(err) => {
                let report =
                    format_qemu_failure_report(&result.arch, &result.variant, &result.cpu, &err);
                println!("fail: {report}");
                failed_reports.push(report);
            }
        }
    }

    println!(
        "qemu summary: {} passed, {} failed",
        passed,
        failed_reports.len()
    );
    if !failed_reports.is_empty() {
        println!(
            "failed comparisons:\n{}",
            failed_reports
                .iter()
                .map(|item| format!("  - {item}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        bail!(
            "qemu failed: {} failed comparisons; see log paths above",
            failed_reports.len()
        );
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct SystemManifest {
    qemu: QemuManifest,
    arch: Vec<SystemArch>,
}

#[derive(Debug, Deserialize)]
struct QemuManifest {
    version: String,
    source_url: String,
    source_sha256: String,
    install_dir: String,
}

#[derive(Debug, Deserialize)]
struct SystemArch {
    name: String,
    target: String,
    qemu_system: String,
    qemu_binary: String,
    kernel: String,
    initrd: String,
    guest_archive_url: String,
    guest_archive_sha256: String,
    guest_kernel_member: String,
    guest_initrd_member: String,
    rootfs_archive_url: String,
    rootfs_archive_sha256: String,
    rust_archive_url: String,
    rust_archive_sha256: String,
    case: Vec<SystemCase>,
    #[serde(default)]
    exception: Vec<SystemException>,
}

#[derive(Debug, Deserialize)]
struct SystemCase {
    cpu: String,
}

#[derive(Debug, Deserialize)]
struct SystemException {
    cpu: String,
    reason: String,
}

struct PreparedQemuArch<'a> {
    arch: &'a SystemArch,
    variant: String,
    initrd: PathBuf,
}

struct QemuCaseJob<'a> {
    arch: &'a SystemArch,
    variant: &'a str,
    case: &'a SystemCase,
    initrd: &'a Path,
}

struct QemuCaseResult {
    arch: String,
    variant: String,
    cpu: String,
    outcome: std::result::Result<(), Box<QemuCaseFailure>>,
}

struct QemuCaseFailure {
    comparison: Option<GuestComparison>,
    status: String,
    message: String,
    log_path: Option<PathBuf>,
}

struct GuestComparison {
    native: String,
    selected: String,
    result: String,
    failed_variant: Option<String>,
    failed_app_status: Option<String>,
    failed_reason: Option<String>,
}

struct QemuTestApp {
    variant: String,
    binary: PathBuf,
}

#[derive(Clone, Copy)]
struct QemuBuildVariant {
    runtime: QemuRuntime,
    compression: QemuCompression,
    loader: QemuLoader,
    compilation: QemuCompilation,
}

#[derive(Clone, Copy)]
enum QemuRuntime {
    GlibcDynamic,
    GlibcStatic,
    MuslDynamic,
    MuslStatic,
    NoStd,
}

#[derive(Clone, Copy)]
enum QemuCompression {
    Plain,
    Zstd,
}

#[derive(Clone, Copy)]
enum QemuLoader {
    Embedded,
    Bundle,
}

#[derive(Clone, Copy)]
enum QemuCompilation {
    Normal,
    Cross,
}

impl QemuBuildVariant {
    fn id(self) -> String {
        format!(
            "{}-{}-{}-{}",
            self.runtime.label(),
            self.compression.label(),
            self.loader.label(),
            self.compilation.label()
        )
    }
}

impl QemuRuntime {
    fn label(self) -> &'static str {
        match self {
            QemuRuntime::GlibcDynamic => "glibc-dynamic",
            QemuRuntime::GlibcStatic => "glibc-static",
            QemuRuntime::MuslDynamic => "musl-dynamic",
            QemuRuntime::MuslStatic => "musl-static",
            QemuRuntime::NoStd => "nostd",
        }
    }
}

impl QemuCompression {
    fn label(self) -> &'static str {
        match self {
            QemuCompression::Plain => "plain",
            QemuCompression::Zstd => "zstd",
        }
    }
}

impl QemuLoader {
    fn label(self) -> &'static str {
        match self {
            QemuLoader::Embedded => "embedded",
            QemuLoader::Bundle => "bundle",
        }
    }
}

impl QemuCompilation {
    fn label(self) -> &'static str {
        match self {
            QemuCompilation::Normal => "normal",
            QemuCompilation::Cross => "cross",
        }
    }
}

fn qemu_build_variants(arch: &SystemArch, host_target: &str) -> Vec<QemuBuildVariant> {
    let runtimes: &[QemuRuntime] = if arch.target.contains("-musl") {
        &[
            QemuRuntime::MuslDynamic,
            QemuRuntime::MuslStatic,
            QemuRuntime::NoStd,
        ]
    } else {
        &[
            QemuRuntime::GlibcDynamic,
            QemuRuntime::GlibcStatic,
            QemuRuntime::NoStd,
        ]
    };
    let compressions = [QemuCompression::Plain, QemuCompression::Zstd];
    let loaders = [QemuLoader::Embedded, QemuLoader::Bundle];
    let compilations: &[QemuCompilation] = if arch.target == host_target {
        &[QemuCompilation::Normal, QemuCompilation::Cross]
    } else {
        &[QemuCompilation::Cross]
    };

    let mut variants = Vec::new();
    for runtime in runtimes {
        for compression in compressions {
            for loader in loaders {
                for compilation in compilations {
                    variants.push(QemuBuildVariant {
                        runtime: *runtime,
                        compression,
                        loader,
                        compilation: *compilation,
                    });
                }
            }
        }
    }
    variants
}

fn ensure_qemu_build_variants_available(arch: &SystemArch, host_target: &str) -> Result<()> {
    for build_variant in qemu_build_variants(arch, host_target) {
        if matches!(build_variant.runtime, QemuRuntime::MuslDynamic) {
            musl_dynamic_lib_dir(&arch.target).with_context(|| {
                format!(
                    "qemu build variant {} {} requires dynamic musl libc/libgcc_s; set SONIC_QEMU_MUSL_DYNAMIC_LIB_DIR to a directory containing libc.so and libgcc_s.so",
                    arch.name,
                    build_variant.id()
                )
            })?;
        }
    }
    Ok(())
}

fn format_qemu_failure_report(
    arch: &str,
    variant: &str,
    cpu: &str,
    err: &QemuCaseFailure,
) -> String {
    let (native, selected, result) = err
        .comparison
        .as_ref()
        .map(|comparison| {
            (
                comparison.native.as_str(),
                comparison.selected.as_str(),
                comparison.result.as_str(),
            )
        })
        .unwrap_or(("<unavailable>", "<unavailable>", "<unavailable>"));
    let log = err
        .log_path
        .as_ref()
        .map(|path| format!(" log={}", path.display()))
        .unwrap_or_default();
    let failed_variant = err
        .comparison
        .as_ref()
        .and_then(|comparison| comparison.failed_variant.as_deref())
        .map(|variant| format!(" failed_variant={variant}"))
        .unwrap_or_default();
    let failed_app_status = err
        .comparison
        .as_ref()
        .and_then(|comparison| comparison.failed_app_status.as_deref())
        .map(|status| format!(" app_status={status}"))
        .unwrap_or_default();
    let failed_reason = err
        .comparison
        .as_ref()
        .and_then(|comparison| comparison.failed_reason.as_deref())
        .map(|reason| format!(" reason={reason}"))
        .unwrap_or_default();
    format!(
        "{arch} {variant} {cpu}: native={native} selected={selected} result={result}{failed_variant}{failed_app_status}{failed_reason} status={} message={}{}",
        err.status, err.message, log
    )
}

fn filtered_cases<'a>(arch: &'a SystemArch, case_filter: Option<&str>) -> Vec<&'a SystemCase> {
    arch.case
        .iter()
        .filter(|case| {
            case_filter.is_none_or(|filter| {
                format!("{} {}", arch.name, case.cpu).contains(filter) || case.cpu == filter
            })
        })
        .collect()
}

fn run_qemu_cases_parallel(
    asset_dir: &Path,
    jobs_to_run: &[QemuCaseJob<'_>],
    jobs: usize,
) -> Vec<QemuCaseResult> {
    if jobs_to_run.is_empty() {
        return Vec::new();
    }

    let worker_count = jobs.max(1).min(jobs_to_run.len());
    let next = AtomicUsize::new(0);
    let results = Mutex::new(Vec::with_capacity(jobs_to_run.len()));

    thread::scope(|scope| {
        for _ in 0..worker_count {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= jobs_to_run.len() {
                        break;
                    }
                    let job = &jobs_to_run[index];
                    let outcome =
                        run_qemu_case(asset_dir, job.arch, job.variant, job.case, job.initrd);
                    results.lock().expect("qemu result mutex poisoned").push((
                        index,
                        QemuCaseResult {
                            arch: job.arch.name.clone(),
                            variant: job.variant.to_string(),
                            cpu: job.case.cpu.clone(),
                            outcome,
                        },
                    ));
                }
            });
        }
    });

    let mut results = results.into_inner().expect("qemu result mutex poisoned");
    results.sort_by_key(|(index, _)| *index);
    results.into_iter().map(|(_, result)| result).collect()
}

fn read_targets(path: &Path) -> Result<SystemManifest> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
}

fn missing_system_assets(asset_dir: &Path, manifest: &SystemManifest) -> Vec<String> {
    let mut missing = Vec::new();
    for arch in &manifest.arch {
        for rel in [&arch.qemu_binary, &arch.kernel, &arch.initrd] {
            if !asset_dir.join(rel).exists() {
                missing.push(format!("{}: {}", arch.name, asset_dir.join(rel).display()));
            }
        }
    }
    missing
}

fn prepare_qemu(asset_dir: &Path, manifest: &SystemManifest) -> Result<()> {
    let install_dir = asset_dir.join(&manifest.qemu.install_dir);
    let wanted = manifest
        .arch
        .iter()
        .map(|arch| asset_dir.join(&arch.qemu_binary))
        .collect::<Vec<_>>();
    if wanted.iter().all(|path| path.exists()) {
        println!("qemu {} already prepared", manifest.qemu.version);
        return Ok(());
    }

    ensure_tool("curl")?;
    ensure_tool("tar")?;
    ensure_tool("xz")?;
    ensure_tool("make")?;
    ensure_tool("python3")?;

    let downloads = asset_dir.join("downloads");
    let build_root = asset_dir.join("build");
    fs::create_dir_all(&downloads)?;
    fs::create_dir_all(&build_root)?;
    let tools_path = prepare_python_tools(asset_dir)?;

    let archive = downloads.join(format!("qemu-{}.tar.xz", manifest.qemu.version));
    download_if_missing(&manifest.qemu.source_url, &archive)?;
    verify_sha256(&archive, &manifest.qemu.source_sha256)?;

    let source_dir = build_root.join(format!("qemu-{}", manifest.qemu.version));
    if !source_dir.exists() {
        run(
            Command::new("tar")
                .arg("-xJf")
                .arg(&archive)
                .arg("-C")
                .arg(&build_root),
            "extract qemu source",
        )?;
    }

    if !install_dir.join("bin/qemu-system-x86_64").exists()
        || !install_dir.join("bin/qemu-system-aarch64").exists()
    {
        fs::create_dir_all(&install_dir)?;
        run(
            with_extra_path(
                Command::new("./configure")
                    .current_dir(&source_dir)
                    .arg("--target-list=x86_64-softmmu,aarch64-softmmu")
                    .arg(format!("--prefix={}", install_dir.display()))
                    .arg("--disable-docs")
                    .arg("--disable-werror")
                    .arg("--disable-gtk")
                    .arg("--disable-sdl")
                    .arg("--disable-vnc")
                    .arg("--disable-tools"),
                &tools_path,
            ),
            "configure qemu",
        )?;
        run(
            with_extra_path(
                Command::new("make")
                    .current_dir(&source_dir)
                    .arg(format!("-j{}", parallel_jobs())),
                &tools_path,
            ),
            "build qemu",
        )?;
        run(
            with_extra_path(
                Command::new("make").current_dir(&source_dir).arg("install"),
                &tools_path,
            ),
            "install qemu",
        )?;
    }

    for path in wanted {
        if !path.exists() {
            bail!("qemu build completed but {} is missing", path.display());
        }
    }
    Ok(())
}

fn prepare_python_tools(asset_dir: &Path) -> Result<PathBuf> {
    if command_exists("ninja")? {
        return Ok(PathBuf::new());
    }

    let venv = asset_dir.join("tools/python");
    let ninja = venv.join("bin/ninja");
    if !ninja.exists() {
        fs::create_dir_all(venv.parent().expect("venv has parent"))?;
        run(
            Command::new("python3").arg("-m").arg("venv").arg(&venv),
            "create qemu tools venv",
        )?;
        run(
            Command::new(venv.join("bin/python"))
                .arg("-m")
                .arg("pip")
                .arg("install")
                .arg("ninja==1.13.0"),
            "install pinned ninja",
        )?;
    }
    Ok(venv.join("bin"))
}

fn with_extra_path<'a>(command: &'a mut Command, extra: &Path) -> &'a mut Command {
    if extra.as_os_str().is_empty() {
        return command;
    }
    let current = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = std::env::split_paths(&current).collect::<Vec<_>>();
    paths.insert(0, extra.to_path_buf());
    let joined = std::env::join_paths(paths).expect("PATH join should succeed");
    command.env("PATH", joined)
}

fn prepare_guests(asset_dir: &Path, manifest: &SystemManifest) -> Result<()> {
    ensure_tool("curl")?;
    ensure_tool("tar")?;
    ensure_tool("gzip")?;

    let downloads = asset_dir.join("downloads");
    fs::create_dir_all(&downloads)?;

    for arch in &manifest.arch {
        let kernel = asset_dir.join(&arch.kernel);
        let initrd = asset_dir.join(&arch.initrd);
        if kernel.exists() && initrd.exists() {
            println!("guest {} already prepared", arch.name);
            continue;
        }

        let archive_name = arch
            .guest_archive_url
            .rsplit('/')
            .next()
            .context("guest archive URL must contain a file name")?;
        let archive = downloads.join(archive_name);
        download_if_missing(&arch.guest_archive_url, &archive)?;
        verify_sha256(&archive, &arch.guest_archive_sha256)?;

        let extract_dir = asset_dir.join("build").join(format!("guest-{}", arch.name));
        if extract_dir.exists() {
            fs::remove_dir_all(&extract_dir)
                .with_context(|| format!("failed to remove {}", extract_dir.display()))?;
        }
        fs::create_dir_all(&extract_dir)?;
        run(
            Command::new("tar")
                .arg("-xzf")
                .arg(&archive)
                .arg("-C")
                .arg(&extract_dir)
                .arg(&arch.guest_kernel_member)
                .arg(&arch.guest_initrd_member),
            "extract guest netboot archive",
        )?;

        copy_prepared_file(&extract_dir.join(&arch.guest_kernel_member), &kernel)?;
        copy_prepared_file(&extract_dir.join(&arch.guest_initrd_member), &initrd)?;
    }
    Ok(())
}

fn prepare_runtime_rootfs(asset_dir: &Path, manifest: &SystemManifest) -> Result<()> {
    ensure_tool("curl")?;
    ensure_tool("tar")?;
    ensure_tool("xz")?;

    let downloads = asset_dir.join("downloads");
    let rootfs_root = asset_dir.join("rootfs");
    fs::create_dir_all(&downloads)?;
    fs::create_dir_all(&rootfs_root)?;

    for arch in &manifest.arch {
        let rootfs = rootfs_root.join(&arch.name);
        if rootfs.join(".cargo-sonic-prepared").exists() {
            prune_runtime_rootfs(&rootfs)?;
            write_guest_init(&rootfs.join("init"), arch)?;
            println!("runtime rootfs {} already prepared", arch.name);
            continue;
        }
        if rootfs.exists() {
            fs::remove_dir_all(&rootfs)
                .with_context(|| format!("failed to remove {}", rootfs.display()))?;
        }
        fs::create_dir_all(&rootfs)?;

        let ubuntu_archive = downloads.join(
            arch.rootfs_archive_url
                .rsplit('/')
                .next()
                .context("rootfs archive URL must contain a file name")?,
        );
        download_if_missing(&arch.rootfs_archive_url, &ubuntu_archive)?;
        verify_sha256(&ubuntu_archive, &arch.rootfs_archive_sha256)?;
        run(
            Command::new("tar")
                .arg("-xzf")
                .arg(&ubuntu_archive)
                .arg("-C")
                .arg(&rootfs),
            "extract ubuntu base rootfs",
        )?;

        let rust_archive = downloads.join(
            arch.rust_archive_url
                .rsplit('/')
                .next()
                .context("rust archive URL must contain a file name")?,
        );
        download_if_missing(&arch.rust_archive_url, &rust_archive)?;
        verify_sha256(&rust_archive, &arch.rust_archive_sha256)?;

        let rust_extract = asset_dir
            .join("build")
            .join(format!("rust-install-{}", arch.name));
        if rust_extract.exists() {
            fs::remove_dir_all(&rust_extract)
                .with_context(|| format!("failed to remove {}", rust_extract.display()))?;
        }
        fs::create_dir_all(&rust_extract)?;
        run(
            Command::new("tar")
                .arg("-xJf")
                .arg(&rust_archive)
                .arg("-C")
                .arg(&rust_extract),
            "extract rust toolchain",
        )?;
        let installer = fs::read_dir(&rust_extract)?
            .filter_map(|entry| entry.ok())
            .find(|entry| entry.path().join("install.sh").exists())
            .map(|entry| entry.path().join("install.sh"))
            .context("rust archive did not contain install.sh")?;
        run(
            Command::new("sh")
                .arg(&installer)
                .arg("--prefix=/opt/rust")
                .arg(format!("--destdir={}", rootfs.display()))
                .arg("--disable-ldconfig"),
            "install rust toolchain into guest rootfs",
        )?;

        prune_runtime_rootfs(&rootfs)?;
        write_guest_init(&rootfs.join("init"), arch)?;
        fs::write(rootfs.join("etc/resolv.conf"), b"nameserver 1.1.1.1\n")?;
        fs::write(rootfs.join(".cargo-sonic-prepared"), b"prepared\n")?;
    }
    Ok(())
}

fn prune_runtime_rootfs(rootfs: &Path) -> Result<()> {
    for rel in [
        "opt/rust/share/doc",
        "opt/rust/share/man",
        "opt/rust/share/zsh",
        "opt/rust/etc",
    ] {
        let path = rootfs.join(rel);
        if path.exists() {
            fs::remove_dir_all(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
        }
    }
    Ok(())
}

fn write_guest_init(path: &Path, arch: &SystemArch) -> Result<()> {
    let script = format!(
        r#"#!/bin/sh
set -eu
export PATH=/opt/rust/bin:/usr/sbin:/usr/bin:/sbin:/bin
mount -t proc proc /proc || true
mount -t sysfs sysfs /sys || true
mount -t devtmpfs devtmpfs /dev || true
mkdir -p /tmp
cat >/tmp/native.rs <<'RS'
fn main() {{}}
RS
rustc /tmp/native.rs --emit=llvm-ir -C target-cpu=native -o /tmp/native.ll
native="$(sed -n 's/.*"target-cpu"="\([^"]*\)".*/\1/p' /tmp/native.ll | head -n1)"
echo "CARGO_SONIC_QEMU_BEGIN"
echo "arch={arch_name}"
echo "native=$native"
overall=ok
for app in /sonic-qemu-app-*; do
  if [ ! -f "$app" ] || [ ! -x "$app" ]; then
    continue
  fi
  variant="${{app#/sonic-qemu-app-}}"
  safe_variant="$(echo "$variant" | sed 's/[^A-Za-z0-9_.-]/_/g')"
  set +e
  CARGO_SONIC_DEBUG=1 SONIC_EXAMPLE_ITERS=1 SONIC_EXAMPLE_LEN=64 "$app" > "/tmp/app-$safe_variant.out" 2> "/tmp/app-$safe_variant.err"
  app_status="$?"
  set -e
  selected="$(sed -n 's/^selected target-cpu: //p' "/tmp/app-$safe_variant.out" | head -n1)"
  native_line="$(grep -F "    $native eligible=" "/tmp/app-$safe_variant.err" | head -n1 || true)"
  selected_line="$(grep -F "    $selected eligible=" "/tmp/app-$safe_variant.err" | head -n1 || true)"
  native_eligible="$(echo "$native_line" | sed -n 's/.* eligible=\([^ ]*\).*/\1/p')"
  selected_eligible="$(echo "$selected_line" | sed -n 's/.* eligible=\([^ ]*\).*/\1/p')"
  expectation=exact
  reason=
  if [ "$app_status" != 0 ]; then
    result=fail
    reason="app-exit-$app_status"
  elif [ -z "$native_line" ]; then
    result=fail
    reason=native-variant-missing
  elif [ "$selected_eligible" != yes ]; then
    result=fail
    reason=selected-variant-ineligible
  elif [ "$native_eligible" = yes ] && [ "$selected" = "$native" ]; then
    result=ok
    expectation=exact
  elif [ "$native_eligible" = yes ]; then
    result=fail
    reason=native-eligible-but-not-selected
  else
    result=ok
    expectation=best-effort-native-ineligible
  fi
  if [ "$result" = fail ]; then
    overall=fail
  fi
  echo "variant=$variant"
  echo "app_status=$app_status"
  echo "native_eligible=$native_eligible"
  echo "selected=$selected"
  echo "selected_eligible=$selected_eligible"
  echo "expectation=$expectation"
  echo "reason=$reason"
  cat "/tmp/app-$safe_variant.out"
  cat "/tmp/app-$safe_variant.err"
  echo "variant_result=$result"
done
if [ "$overall" = fail ]; then
  echo "guest /proc/cpuinfo:"
  cat /proc/cpuinfo || true
  echo "guest sysfs midr:"
  cat /sys/devices/system/cpu/cpu0/regs/identification/midr_el1 || true
fi
echo "result=$overall"
echo "CARGO_SONIC_QEMU_END"
echo 1 > /proc/sys/kernel/sysrq 2>/dev/null || true
echo o > /proc/sysrq-trigger 2>/dev/null || true
while :; do
  sleep 3600
done
"#,
        arch_name = arch.name,
    );
    fs::write(path, script).with_context(|| format!("failed to write {}", path.display()))?;
    make_executable(path)
}

fn build_qemu_test_app(
    root: &Path,
    asset_dir: &Path,
    cargo_sonic: &Path,
    arch: &SystemArch,
    build_variant: QemuBuildVariant,
    variants: &[String],
) -> Result<QemuTestApp> {
    let variant = build_variant.id();
    let package_name = format!("sonic-qemu-app-{variant}");
    let project = asset_dir
        .join("work")
        .join(format!("app-{}-{variant}", arch.name));
    if project.exists() {
        fs::remove_dir_all(&project)
            .with_context(|| format!("failed to remove {}", project.display()))?;
    }
    fs::create_dir_all(project.join("src"))?;
    fs::write(
        project.join("Cargo.toml"),
        format!(
            r#"[package]
name = "{package_name}"
version = "0.1.0"
edition = "2024"
publish = false

[workspace]

[profile.dev]
panic = "abort"

[profile.release]
panic = "abort"
"#
        ),
    )?;
    fs::write(
        project.join("src/main.rs"),
        qemu_app_source(build_variant.runtime),
    )?;

    let target_dir = asset_dir
        .join("cargo-target")
        .join("qemu-apps")
        .join(&variant);
    let mut command = Command::new(cargo_sonic);
    command
        .current_dir(root)
        .env("CARGO_TARGET_DIR", target_dir)
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_BUILD_RUSTFLAGS")
        .env_remove(cargo_target_rustflags_env(&arch.target));
    if arch.target == "aarch64-unknown-linux-musl" {
        command.env("CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER", "rust-lld");
    }
    if let Some(rustflags) = qemu_payload_rustflags(arch, build_variant.runtime)? {
        command.env("CARGO_ENCODED_RUSTFLAGS", rustflags);
    }
    let target_cpus = variants.join(",");
    let mut args = vec![
        "sonic".to_string(),
        "--target-cpus".to_string(),
        target_cpus,
        "--loader".to_string(),
        build_variant.loader.label().to_string(),
    ];
    if matches!(build_variant.compression, QemuCompression::Zstd) {
        args.extend([
            "--compress".to_string(),
            "zstd".to_string(),
            "--compression-level".to_string(),
            "3".to_string(),
        ]);
    }
    args.extend(["build".to_string(), "--release".to_string()]);
    if matches!(build_variant.compilation, QemuCompilation::Cross) {
        args.extend(["--target".to_string(), arch.target.clone()]);
    }
    args.push("--manifest-path".to_string());
    let output = command
        .args(args)
        .arg(project.join("Cargo.toml"))
        .output()
        .with_context(|| "failed to run cargo-sonic for qemu test app")?;
    if !output.status.success() {
        bail!(
            "cargo-sonic qemu test app build failed for {} {variant}\nstdout:\n{}\nstderr:\n{}",
            arch.name,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let path = stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(PathBuf::from)
        .context("cargo-sonic did not print final binary path")?;
    if !path.exists() {
        bail!(
            "cargo-sonic printed missing final binary {}",
            path.display()
        );
    }
    Ok(QemuTestApp {
        variant,
        binary: path,
    })
}

fn qemu_payload_rustflags(arch: &SystemArch, runtime: QemuRuntime) -> Result<Option<String>> {
    let flags = match runtime {
        QemuRuntime::GlibcDynamic => Some("-C\x1ftarget-feature=-crt-static".to_string()),
        QemuRuntime::MuslDynamic => {
            let lib_dir = musl_dynamic_lib_dir(&arch.target).with_context(|| {
                format!(
                    "dynamic musl libs are required for {}",
                    QemuRuntime::MuslDynamic.label()
                )
            })?;
            Some(format!(
                "-C\x1ftarget-feature=-crt-static\x1f-L\x1fnative={}",
                lib_dir.display()
            ))
        }
        QemuRuntime::GlibcStatic | QemuRuntime::MuslStatic => {
            Some("-C\x1ftarget-feature=+crt-static".to_string())
        }
        QemuRuntime::NoStd if arch.target.contains("-musl") => {
            Some("-Clink-self-contained=no\x1f-C\x1flink-arg=-static".to_string())
        }
        QemuRuntime::NoStd => Some("-C\x1flink-arg=-nostartfiles".to_string()),
    };
    Ok(flags)
}

fn musl_dynamic_lib_dir(target: &str) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("SONIC_QEMU_MUSL_DYNAMIC_LIB_DIR").map(PathBuf::from) {
        return musl_dynamic_lib_dir_is_usable(&path).then_some(path);
    }

    let sysroot = Command::new("rustc")
        .arg("--print")
        .arg("sysroot")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())?;
    let lib_dir = PathBuf::from(sysroot.trim())
        .join("lib")
        .join("rustlib")
        .join(target)
        .join("lib");
    musl_dynamic_lib_dir_is_usable(&lib_dir).then_some(lib_dir)
}

fn musl_dynamic_lib_dir_is_usable(path: &Path) -> bool {
    path.join("libc.so").exists()
        && (path.join("libgcc_s.so").exists() || path.join("libgcc_s.so.1").exists())
}

fn qemu_app_source(runtime: QemuRuntime) -> &'static str {
    match runtime {
        QemuRuntime::GlibcDynamic
        | QemuRuntime::GlibcStatic
        | QemuRuntime::MuslDynamic
        | QemuRuntime::MuslStatic => {
            r#"fn main() {
    let variant = std::env::var("CARGO_SONIC_SELECTED_TARGET_CPU").unwrap_or_default();
    let len = std::env::var("SONIC_EXAMPLE_LEN").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(64);
    let iters = std::env::var("SONIC_EXAMPLE_ITERS").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(1);
    let mut sum = 0u64;
    for r in 0..iters {
        for i in 0..len {
            sum = sum
                .wrapping_mul(1_664_525)
                .wrapping_add(r)
                .wrapping_add(i as u64);
        }
    }
    println!("selected target-cpu: {variant}");
    println!("checksum: {sum}");
}"#
        }
        QemuRuntime::NoStd => {
            r##"#![no_std]
#![no_main]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

#[cfg(target_arch = "x86_64")]
global_asm!(
    r#"
    .global _start
_start:
    mov rdi, rsp
    call {entry}
    ud2
"#,
    entry = sym rust_entry,
);

#[cfg(target_arch = "aarch64")]
global_asm!(
    r#"
    .global _start
_start:
    mov x0, sp
    bl {entry}
    brk #0
"#,
    entry = sym rust_entry,
);

#[panic_handler]
fn panic(_: &PanicInfo<'_>) -> ! {
    exit(101)
}

#[unsafe(no_mangle)]
extern "C" fn rust_eh_personality() {}

#[unsafe(no_mangle)]
extern "C" fn rust_entry(stack: *const usize) -> ! {
    let (selected, selected_len) = unsafe {
        find_env_value(stack, b"CARGO_SONIC_SELECTED_TARGET_CPU=")
            .unwrap_or((b"<missing>".as_ptr(), b"<missing>".len()))
    };
    write_all(1, b"selected target-cpu: ");
    unsafe {
        write_all(1, core::slice::from_raw_parts(selected, selected_len));
    }
    write_all(1, b"\nchecksum: 0.000000\n");
    exit(0)
}

unsafe fn find_env_value(stack: *const usize, prefix: &[u8]) -> Option<(*const u8, usize)> {
    let argc = unsafe { *stack };
    let mut envp = unsafe { stack.add(1 + argc + 1) as *const *const u8 };
    loop {
        let item = unsafe { *envp };
        if item.is_null() {
            return None;
        }
        if unsafe { cstr_starts_with(item, prefix) } {
            let value = unsafe { item.add(prefix.len()) };
            return Some((value, unsafe { cstr_len(value) }));
        }
        envp = unsafe { envp.add(1) };
    }
}

unsafe fn cstr_starts_with(ptr: *const u8, prefix: &[u8]) -> bool {
    for (offset, expected) in prefix.iter().enumerate() {
        if unsafe { core::ptr::read_volatile(ptr.add(offset)) } != *expected {
            return false;
        }
    }
    true
}

unsafe fn cstr_len(mut ptr: *const u8) -> usize {
    let start = ptr;
    while unsafe { core::ptr::read_volatile(ptr) } != 0 {
        ptr = unsafe { ptr.add(1) };
    }
    ptr as usize - start as usize
}

fn write_all(fd: usize, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        let written = unsafe { syscall3(SYS_WRITE, fd, bytes.as_ptr() as usize, bytes.len()) };
        if written <= 0 {
            exit(102);
        }
        bytes = &bytes[written as usize..];
    }
}

fn exit(code: usize) -> ! {
    unsafe {
        syscall1(SYS_EXIT, code);
    }
    loop {}
}

#[cfg(target_arch = "x86_64")]
const SYS_WRITE: usize = 1;
#[cfg(target_arch = "x86_64")]
const SYS_EXIT: usize = 60;

#[cfg(target_arch = "aarch64")]
const SYS_WRITE: usize = 64;
#[cfg(target_arch = "aarch64")]
const SYS_EXIT: usize = 93;

#[cfg(target_arch = "x86_64")]
unsafe fn syscall1(n: usize, a: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") n as isize => ret,
            in("rdi") a,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret
}

#[cfg(target_arch = "x86_64")]
unsafe fn syscall3(n: usize, a: usize, b: usize, c: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") n as isize => ret,
            in("rdi") a,
            in("rsi") b,
            in("rdx") c,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret
}

#[cfg(target_arch = "aarch64")]
unsafe fn syscall1(n: usize, a: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "svc 0",
            in("x8") n,
            inlateout("x0") a as isize => ret,
        );
    }
    ret
}

#[cfg(target_arch = "aarch64")]
unsafe fn syscall3(n: usize, a: usize, b: usize, c: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "svc 0",
            in("x8") n,
            inlateout("x0") a as isize => ret,
            in("x1") b,
            in("x2") c,
        );
    }
    ret
}
"##
        }
    }
}

fn build_cargo_sonic_driver(root: &Path, asset_dir: &Path) -> Result<PathBuf> {
    let target_dir = asset_dir.join("cargo-target");
    let mut command = Command::new("cargo");
    command
        .current_dir(root)
        .env("CARGO_TARGET_DIR", &target_dir)
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_BUILD_RUSTFLAGS")
        .args(["build", "-p", "cargo-sonic"]);
    run(&mut command, "build cargo-sonic qemu driver")?;
    let binary = target_dir.join("debug").join("cargo-sonic");
    if !binary.exists() {
        bail!(
            "cargo-sonic qemu driver was not built at {}",
            binary.display()
        );
    }
    Ok(binary)
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

fn rustc_payload_target_cpus(
    asset_dir: &Path,
    arch: &SystemArch,
    cases: &[&SystemCase],
) -> Result<Vec<String>> {
    let target = &arch.target;
    let valid = rustc_target_cpus(target)?;
    let requested = qemu_payload_candidates(arch, cases);
    let probe_dir = asset_dir
        .join("run")
        .join("target-cpu-probes")
        .join(sanitize_path_component(target));
    fs::create_dir_all(&probe_dir)
        .with_context(|| format!("failed to create {}", probe_dir.display()))?;

    let probe_source = probe_dir.join("probe.rs");
    fs::write(
        &probe_source,
        "#![no_std]\n#[no_mangle]\npub extern \"C\" fn cargo_sonic_probe() {}\n",
    )
    .with_context(|| format!("failed to write {}", probe_source.display()))?;
    ensure_rust_target_installed(target, &probe_source, &probe_dir)?;

    let mut accepted = Vec::new();
    let mut skipped = Vec::new();
    for cpu in requested {
        if !valid.contains(&cpu) {
            skipped.push((cpu, "rustc does not list this target-cpu".to_string()));
            continue;
        }
        let object = probe_dir.join(format!("{}.o", sanitize_path_component(&cpu)));
        let output = Command::new("rustc")
            .arg("--crate-type=lib")
            .arg("--emit=obj")
            .arg("--target")
            .arg(target)
            .arg("-C")
            .arg(format!("target-cpu={cpu}"))
            .arg("-C")
            .arg("panic=abort")
            .arg(&probe_source)
            .arg("-o")
            .arg(&object)
            .output()
            .with_context(|| format!("failed to probe target-cpu `{cpu}` for {target}"))?;
        if output.status.success() {
            accepted.push(cpu);
        } else {
            let reason = String::from_utf8_lossy(&output.stderr)
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("rustc rejected target-cpu for this target")
                .trim()
                .to_string();
            skipped.push((cpu, reason));
        }
    }

    if !skipped.is_empty() {
        println!(
            "rustc target-cpu probe for {target}: {} accepted, {} skipped",
            accepted.len(),
            skipped.len()
        );
        for (cpu, reason) in &skipped {
            println!("  skip {cpu}: {reason}");
        }
    }
    if accepted.is_empty() {
        let reasons = skipped
            .iter()
            .take(10)
            .map(|(cpu, reason)| format!("  - {cpu}: {reason}"))
            .collect::<Vec<_>>()
            .join("\n");
        bail!(
            "rustc reported no payload-buildable target-cpus for {target}{}",
            if reasons.is_empty() {
                String::new()
            } else {
                format!("\n{reasons}")
            }
        );
    }
    Ok(accepted)
}

fn qemu_payload_candidates(arch: &SystemArch, cases: &[&SystemCase]) -> Vec<String> {
    let mut cpus = Vec::new();
    for case in cases {
        match arch.name.as_str() {
            "x86_64" => cpus.extend(x86_qemu_case_target_cpus(&case.cpu)),
            "aarch64" => cpus.push(case.cpu.clone()),
            _ => {}
        }
    }
    cpus.sort();
    cpus.dedup();
    cpus
}

fn x86_qemu_case_target_cpus(cpu: &str) -> Vec<String> {
    match cpu {
        "qemu64" => strings(&["k8-sse3"]),
        "kvm64" => strings(&["nocona"]),
        "phenom" => strings(&["amdfam10", "phenom"]),
        "Opteron_G2" => strings(&["k8-sse3"]),
        "Opteron_G3" => strings(&["amdfam10"]),
        "Penryn" => strings(&["penryn"]),
        "Nehalem" => strings(&["nehalem"]),
        "Westmere" => strings(&["westmere"]),
        "SandyBridge" => strings(&["sandybridge"]),
        "IvyBridge" => strings(&["ivybridge"]),
        "Haswell-v4" => strings(&["haswell"]),
        "Broadwell-v4" => strings(&["broadwell"]),
        other => vec![other.to_string()],
    }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

fn ensure_rust_target_installed(target: &str, probe_source: &Path, probe_dir: &Path) -> Result<()> {
    let object = probe_dir.join("target-installed.o");
    let output = Command::new("rustc")
        .arg("--crate-type=lib")
        .arg("--emit=obj")
        .arg("--target")
        .arg(target)
        .arg("-C")
        .arg("panic=abort")
        .arg(probe_source)
        .arg("-o")
        .arg(&object)
        .output()
        .with_context(|| format!("failed to check whether Rust target `{target}` is installed"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("can't find crate for `core`") {
        bail!(
            "Rust target `{target}` is not installed for the host toolchain; run `rustup target add {target}`"
        );
    }
    bail!(
        "failed to compile host probe for Rust target `{target}`:\n{}",
        stderr
    );
}

fn rustc_target_cpus(target: &str) -> Result<Vec<String>> {
    let implicit_baseline = if target.starts_with("x86_64-") {
        "x86-64"
    } else {
        "generic"
    };
    let output = Command::new("rustc")
        .args(["--print", "target-cpus", "--target", target])
        .output()
        .with_context(|| format!("failed to run rustc --print target-cpus --target {target}"))?;
    if !output.status.success() {
        bail!(
            "rustc --print target-cpus failed for {target}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut cpus = stdout
        .lines()
        .filter_map(|line| {
            let cpu = line.split_whitespace().next()?;
            if line.starts_with("    ") && cpu != "native" && cpu != implicit_baseline {
                Some(cpu.to_string())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    cpus.sort();
    cpus.dedup();
    if cpus.is_empty() {
        bail!("rustc reported no target-cpus for {target}");
    }
    Ok(cpus)
}

fn host_target_triple() -> Result<String> {
    let output = Command::new("rustc")
        .arg("-vV")
        .output()
        .context("failed to run rustc -vV")?;
    if !output.status.success() {
        bail!(
            "rustc -vV failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("host: ").map(str::to_string))
        .context("rustc -vV output did not contain a host triple")
}

fn build_test_initramfs(
    asset_dir: &Path,
    arch: &SystemArch,
    apps: &[QemuTestApp],
) -> Result<PathBuf> {
    ensure_tool("cpio")?;
    ensure_tool("gzip")?;

    let rootfs = asset_dir.join("rootfs").join(&arch.name);
    let initrd = asset_dir
        .join("run")
        .join(&arch.name)
        .join("initramfs-matrix.cpio.gz");
    if let Some(parent) = initrd.parent() {
        fs::create_dir_all(parent)?;
    }
    for entry in fs::read_dir(&rootfs)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("sonic-qemu-app-") {
            let path = entry.path();
            if path.is_dir() {
                fs::remove_dir_all(&path)
                    .with_context(|| format!("failed to remove old {}", path.display()))?;
            } else {
                fs::remove_file(&path)
                    .with_context(|| format!("failed to remove old {}", path.display()))?;
            }
        }
    }
    install_musl_dynamic_runtime(&rootfs, arch)?;
    for app in apps {
        let guest_name = qemu_guest_app_name(&app.variant);
        let guest_path = rootfs.join(&guest_name);
        fs::copy(&app.binary, &guest_path).with_context(|| {
            format!(
                "failed to copy {} into {}",
                app.binary.display(),
                rootfs.display()
            )
        })?;
        make_executable(&guest_path)?;
        let bundle = bundle_dir_for(&app.binary);
        if bundle.exists() {
            copy_dir_all(&bundle, &rootfs.join(format!("{guest_name}.bundle")))?;
        }
    }

    let mut sh = Command::new("sh");
    sh.current_dir(&rootfs).arg("-c").arg(format!(
        "find . -print0 | cpio --null -o -H newc 2>/dev/null | gzip -9 > {}",
        shell_quote(&initrd)
    ));
    run(&mut sh, "build qemu test initramfs")?;
    Ok(initrd)
}

fn qemu_guest_app_name(variant: &str) -> String {
    format!("sonic-qemu-app-{variant}")
}

fn install_musl_dynamic_runtime(rootfs: &Path, arch: &SystemArch) -> Result<()> {
    if !arch.target.contains("-musl") {
        return Ok(());
    }
    let lib_dir = musl_dynamic_lib_dir(&arch.target).with_context(|| {
        format!(
            "dynamic musl runtime for {} is required; set SONIC_QEMU_MUSL_DYNAMIC_LIB_DIR",
            arch.target
        )
    })?;
    let rootfs_lib = rootfs.join("lib");
    fs::create_dir_all(&rootfs_lib)
        .with_context(|| format!("failed to create {}", rootfs_lib.display()))?;
    for entry in
        fs::read_dir(&lib_dir).with_context(|| format!("failed to read {}", lib_dir.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == "libc.so" || name.starts_with("libgcc_s.so") {
            fs::copy(entry.path(), rootfs_lib.join(name.as_ref()))
                .with_context(|| format!("failed to copy {}", entry.path().display()))?;
        }
    }
    let loader = rootfs_lib.join(musl_loader_name(&arch.target));
    if !loader.exists() && rootfs_lib.join("libc.so").exists() {
        std::os::unix::fs::symlink("libc.so", &loader)
            .with_context(|| format!("failed to create {}", loader.display()))?;
    }
    Ok(())
}

fn musl_loader_name(target: &str) -> &'static str {
    if target.starts_with("aarch64-") {
        "ld-musl-aarch64.so.1"
    } else if target.starts_with("x86_64-") {
        "ld-musl-x86_64.so.1"
    } else {
        "ld-musl.so.1"
    }
}

fn bundle_dir_for(binary: &Path) -> PathBuf {
    PathBuf::from(format!("{}.bundle", binary.display()))
}

fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        fs::remove_dir_all(dst).with_context(|| format!("failed to remove {}", dst.display()))?;
    }
    fs::create_dir_all(dst).with_context(|| format!("failed to create {}", dst.display()))?;
    for entry in fs::read_dir(src).with_context(|| format!("failed to read {}", src.display()))? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let target = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)
                .with_context(|| format!("failed to copy {}", entry.path().display()))?;
            if ty.is_file() {
                make_executable(&target)?;
            }
        }
    }
    Ok(())
}

fn run_qemu_case(
    asset_dir: &Path,
    arch: &SystemArch,
    variant: &str,
    case: &SystemCase,
    initrd: &Path,
) -> std::result::Result<(), Box<QemuCaseFailure>> {
    let qemu = asset_dir.join(&arch.qemu_binary);
    let kernel = asset_dir.join(&arch.kernel);
    let mut args = vec![
        "-nographic".to_string(),
        "-no-reboot".to_string(),
        "-m".to_string(),
        "6144".to_string(),
        "-kernel".to_string(),
        kernel.display().to_string(),
        "-initrd".to_string(),
        initrd.display().to_string(),
    ];
    if arch.name == "aarch64" {
        args.extend(["-M".into(), "virt".into()]);
        args.extend(["-cpu".into(), case.cpu.clone()]);
        args.extend([
            "-append".into(),
            "console=ttyAMA0 panic=-1 init=/init".into(),
        ]);
    } else {
        args.extend(["-cpu".into(), case.cpu.clone()]);
        args.extend(["-append".into(), "console=ttyS0 panic=-1 init=/init".into()]);
    }

    let output = Command::new("timeout")
        .arg("180s")
        .arg(qemu)
        .args(args)
        .output()
        .map_err(|err| {
            Box::new(QemuCaseFailure {
                comparison: None,
                status: "spawn-error".to_string(),
                message: format!("failed to run qemu: {err}"),
                log_path: None,
            })
        })?;
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !combined.lines().any(|line| line == "result=ok") {
        let log_path = match write_qemu_case_log(asset_dir, arch, variant, case, &combined) {
            Ok(path) => Some(path),
            Err(err) => {
                eprintln!(
                    "warning: failed to write qemu log for {} {} {}: {err}",
                    arch.name, variant, case.cpu
                );
                None
            }
        };
        return Err(Box::new(QemuCaseFailure {
            comparison: parse_guest_comparison(&combined),
            status: output.status.to_string(),
            message: "guest oracle failed".to_string(),
            log_path,
        }));
    }
    Ok(())
}

fn write_qemu_case_log(
    asset_dir: &Path,
    arch: &SystemArch,
    variant: &str,
    case: &SystemCase,
    output: &str,
) -> Result<PathBuf> {
    let dir = asset_dir
        .join("run")
        .join("logs")
        .join(&arch.name)
        .join(variant);
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create qemu log directory {}", dir.display()))?;
    let path = dir.join(format!("{}.log", sanitize_path_component(&case.cpu)));
    fs::write(&path, output).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

fn sanitize_path_component(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "case".to_string()
    } else {
        sanitized
    }
}

fn parse_guest_comparison(output: &str) -> Option<GuestComparison> {
    let block = output
        .split("CARGO_SONIC_QEMU_BEGIN")
        .nth(1)?
        .split("CARGO_SONIC_QEMU_END")
        .next()?;
    Some(GuestComparison {
        native: parse_guest_field(block, "native").unwrap_or_else(|| "<missing>".to_string()),
        selected: parse_guest_field(block, "selected").unwrap_or_else(|| "<missing>".to_string()),
        result: parse_guest_field(block, "result").unwrap_or_else(|| "<missing>".to_string()),
        failed_variant: parse_failed_variant_field(block, "variant"),
        failed_app_status: parse_failed_variant_field(block, "app_status"),
        failed_reason: parse_failed_variant_field(block, "reason"),
    })
}

fn parse_failed_variant_field(block: &str, name: &str) -> Option<String> {
    let mut current = GuestVariantBlock::default();
    for line in block.lines() {
        if let Some(value) = line.strip_prefix("variant=") {
            current = GuestVariantBlock::default();
            current.variant = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("app_status=") {
            current.app_status = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("reason=") {
            current.reason = Some(value.trim().to_string());
        } else if line.trim() == "variant_result=fail" {
            return match name {
                "variant" => current.variant,
                "app_status" => current.app_status,
                "reason" => current.reason,
                _ => None,
            };
        }
    }
    None
}

#[derive(Default)]
struct GuestVariantBlock {
    variant: Option<String>,
    app_status: Option<String>,
    reason: Option<String>,
}

fn parse_guest_field(block: &str, name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    block
        .lines()
        .find_map(|line| line.strip_prefix(&prefix).map(str::trim))
        .map(str::to_string)
}

fn copy_prepared_file(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(from, to)
        .with_context(|| format!("failed to copy {} to {}", from.display(), to.display()))?;
    Ok(())
}

fn download_if_missing(url: &str, path: &Path) -> Result<()> {
    if path.exists() {
        println!("download already exists {}", path.display());
        return Ok(());
    }
    let partial = path.with_extension("part");
    println!("download {url}");
    run(
        Command::new("curl")
            .arg("-fL")
            .arg("--retry")
            .arg("3")
            .arg("--retry-delay")
            .arg("2")
            .arg("-o")
            .arg(&partial)
            .arg(url),
        "download asset",
    )?;
    fs::rename(&partial, path).with_context(|| {
        format!(
            "failed to rename {} to {}",
            partial.display(),
            path.display()
        )
    })
}

fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    let output = Command::new("sha256sum")
        .arg(path)
        .output()
        .with_context(|| "failed to run sha256sum")?;
    if !output.status.success() {
        bail!(
            "sha256sum failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let actual = stdout
        .split_whitespace()
        .next()
        .context("sha256sum produced no digest")?;
    if actual != expected {
        bail!(
            "sha256 mismatch for {}\nexpected: {}\nactual:   {}",
            path.display(),
            expected,
            actual
        );
    }
    Ok(())
}

fn ensure_tool(name: &str) -> Result<()> {
    if command_exists(name)? {
        return Ok(());
    }
    bail!("required host tool `{name}` is missing");
}

fn command_exists(name: &str) -> Result<bool> {
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name} >/dev/null 2>&1"))
        .status()
        .with_context(|| format!("failed to check for `{name}`"))?;
    Ok(status.success())
}

fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn run(command: &mut Command, label: &str) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("failed to run {label}"))?;
    if !status.success() {
        bail!("{label} failed with status {status}");
    }
    Ok(())
}

fn parallel_jobs() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(2)
}

fn qemu_jobs() -> usize {
    std::env::var("SONIC_QEMU_JOBS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|jobs| *jobs > 0)
        .unwrap_or_else(parallel_jobs)
}

fn write_qemu_readme(asset_dir: &Path, manifest: &SystemManifest) -> Result<()> {
    let mut text = String::new();
    text.push_str("# cargo-sonic system-mode QEMU assets\n\n");
    text.push_str("This directory is generated by `just qemu-prepare`.\n\n");
    text.push_str(&format!(
        "Pinned QEMU version: {}\n\n",
        manifest.qemu.version
    ));
    text.push_str(&format!(
        "Pinned QEMU source: `{}`\n\n",
        manifest.qemu.source_url
    ));
    text.push_str(
        "The correctness suite must boot qemu-system guests and compare the cargo-sonic loader selection with rustc native detection inside the same guest. Host qemu-user and host rustc are intentionally not used as oracles.\n\n",
    );
    for arch in &manifest.arch {
        text.push_str(&format!("## {}\n\n", arch.name));
        text.push_str(&format!("- target: `{}`\n", arch.target));
        text.push_str(&format!("- qemu: `{}`\n", arch.qemu_system));
        text.push_str(&format!("- qemu binary: `{}`\n", arch.qemu_binary));
        text.push_str(&format!("- kernel: `{}`\n", arch.kernel));
        text.push_str(&format!("- initrd: `{}`\n", arch.initrd));
        text.push_str("- variants: rustc target-cpus needed by configured qemu cases, excluding `native` and the implicit cargo-sonic baseline\n");
        text.push_str(&format!("- cpu cases: `{}`\n", arch.case.len()));
        text.push_str(&format!(
            "- cpus: `{}`\n\n",
            arch.case
                .iter()
                .map(|case| case.cpu.as_str())
                .collect::<Vec<_>>()
                .join("`, `")
        ));
        if !arch.exception.is_empty() {
            text.push_str(&format!("- exceptions: `{}`\n", arch.exception.len()));
            for exception in &arch.exception {
                text.push_str(&format!("  - `{}`: {}\n", exception.cpu, exception.reason));
            }
            text.push('\n');
        }
    }
    fs::write(asset_dir.join("README.md"), text)
        .with_context(|| format!("failed to write {}", asset_dir.join("README.md").display()))
}

fn integration() -> Result<()> {
    let status = Command::new("cargo")
        .args(["test", "-p", "cargo-sonic"])
        .status()?;
    if !status.success() {
        bail!("integration command failed");
    }
    Ok(())
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("xtask crate should live under crates/xtask")
        .to_path_buf()
}

fn qemu_asset_dir(root: &Path) -> Result<PathBuf> {
    if let Some(target_dir) =
        std::env::var_os("CARGO_TARGET_DIR").or_else(|| std::env::var_os("CARGO_TARGET"))
    {
        let target_dir = PathBuf::from(target_dir);
        return Ok(if target_dir.is_absolute() {
            target_dir.join(QEMU_ASSET_SUBDIR)
        } else {
            root.join(target_dir).join(QEMU_ASSET_SUBDIR)
        });
    }
    Ok(root.join("target").join(QEMU_ASSET_SUBDIR))
}
