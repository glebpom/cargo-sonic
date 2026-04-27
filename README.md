# cargo-sonic

`cargo-sonic` is a Cargo subcommand that builds one Linux fat executable for a
Rust binary.

The output executable contains multiple ELF payloads built from the same binary
with different `-C target-cpu=<cpu>` values. At runtime, a small generated
`no_std` loader detects CPU features, selects the best compatible payload,
writes it to a `memfd`, and executes it with `execveat(AT_EMPTY_PATH)`.

The loader is intentionally silent. Applications can observe the selected
variant through environment variables.

## Motivation

Modern software distribution relies on portability. To ensure a single Docker
image or binary runs across a diverse fleet of servers, developers typically
target a generic architecture baseline. While this ensures the application
"just works" everywhere, it prevents the compiler from using the specific
capabilities of modern hardware.

### The Auto-Vectorization Gap

Many high-performance libraries, such as cryptography or compression libraries,
use manual runtime detection to switch between hand-written assembly
implementations. Most application code does not have hand-written alternatives
for every possible CPU.

For general-purpose code, performance depends on the compiler. When LLVM is
restricted to a generic target, it must be conservative. It cannot safely use
modern instruction sets such as AVX-512, and it cannot apply
microarchitecture-specific scheduling weights that optimize instruction ordering
for a particular chip pipeline.

### Performance Impact

The difference between a generic build and one tuned for a specific
microarchitecture can be substantial, particularly for compute-heavy tasks where
the compiler can leverage auto-vectorization.

In the CPU benchmark included in the examples folder, a CPU-heavy floating point
kernel repeatedly updates three large `f32` buffers and accumulates a checksum.
On a Raptor Lake host, the performance delta is clear:

| Selection Mode | Target CPU | Execution Time |
| --- | --- | ---: |
| Sonic (Optimized) | `raptorlake` | 154 ms |
| Generic Fallback | `generic` | 2771 ms |

### Optimized Portability

`cargo-sonic` removes the need to choose between a single portable image and
multiple hardware-specific builds. By packaging multiple optimized payloads into
one fat executable, it lets the application negotiate with the silicon at
runtime.

- **Microarchitecture tuning:** enables the compiler to use efficient
  instruction weights and vectorization strategies for the specific host.
- **Automated fallback:** includes a generic payload to preserve compatibility
  with legacy hardware or restricted environments.
- **Infrastructure simplicity:** provides the performance of specialized builds
  within a single portable deployment unit.

By resolving this at the loader level, `cargo-sonic` lets Rust applications use
more of the underlying hardware without manual dispatch code or complex CI/CD
pipelines.

## Status

This repository currently has a working Linux x86_64/AArch64 vertical slice:

- CLI target CPU selection through `--target-cpus`
- implicit `generic` payload, always built
- target-cpu validation through `rustc --print target-cpus`
- payload builds with isolated target directories
- generated Rust 2024 `no_std`/`no_main` loader
- `include_bytes!` payload embedding
- `memfd_create` plus `execveat`
- `CARGO_SONIC_*` env replacement
- x86_64 CPUID and AArch64 auxv feature detection for the implemented feature set
- x86_64 CPU identity detection for selection affinity
- unit tests, generated-loader integration tests, and fixture-driven modern CPU
  selector tests
- pinned QEMU 11.0.0 system-mode correctness suite via `just qemu`

Contributions and bug reports are welcome, especially reports from real
hardware. CPU feature exposure varies across bare metal, virtual machines, cloud
instances, firmware, kernels, and hypervisors, so real host reports are useful
for improving selection safety and coverage.

## Startup Cost

`cargo-sonic` optimizes code generation for the selected CPU, not process
startup. The generated executable is a selector plus embedded payload binaries,
so startup can be slower than the plain binary, especially for very large debug
builds or short-lived CLI tools.

On reflink-capable filesystems, the loader tries to execute the selected payload
through an unnamed cloned tmpfile. If that fast path is unavailable, it falls
back to copying the selected payload into a `memfd` before `execveat`, and that
copy cost is proportional to the payload size.

Benchmark before using `cargo-sonic` for startup-sensitive commands. It is a
better fit for servers, daemons, and other long-running applications where the
one-time startup cost is amortized.

## Run Locally

From a target crate directory:

```bash
cargo run --manifest-path /path/to/cargo-sonic/crates/cargo-sonic/Cargo.toml -- \
  sonic --target-cpus=x86-64-v3,znver5 build
```

From this repository root, build the included example by passing the target
crate manifest through Cargo's normal `--manifest-path` flag:

```bash
cargo run --manifest-path crates/cargo-sonic/Cargo.toml -- \
  sonic --target-cpus=x86-64-v3,znver5,raptorlake build \
  --manifest-path examples/cpu-benchmark/Cargo.toml
```

The final executable is written under the target crate:

```text
target/sonic/<target-triple>/<profile>/<bin-name>
```

If `CARGO_TARGET_DIR` is set, `cargo-sonic` writes under that directory instead:

```text
$CARGO_TARGET_DIR/sonic/<target-triple>/<profile>/<bin-name>
```

`CARGO_TARGET` is also accepted as a compatibility alias, but
`CARGO_TARGET_DIR` is the Cargo-standard variable.

For example:

```text
examples/cpu-benchmark/target/sonic/x86_64-unknown-linux-gnu/debug/sonic-cpu-benchmark
```

## Target CPUs

Choose variants at build time with `--target-cpus`:

```bash
cargo sonic --target-cpus=znver5,x86-64-v4,icelake-server build --release
```

`generic` is implicit. It is always built and is always eligible at runtime, so
do not list it in `--target-cpus`. Pass at least one non-generic CPU; with only
one generic payload there is no reason to use `cargo-sonic`.

Rules:

- `--target-cpus` is required.
- `generic` is added automatically and rejected if listed explicitly.
- `native` is rejected.
- CPU names must exactly match local `rustc --print target-cpus` names.
- Cross-architecture CPU names are skipped for the current target.
- Unknown CPU names are hard errors.

The target triple comes from normal Cargo arguments:

```bash
cargo run --manifest-path crates/cargo-sonic/Cargo.toml -- \
  sonic --target-cpus=x86-64-v3,znver5 build --release
cargo run --manifest-path crates/cargo-sonic/Cargo.toml -- \
  sonic --target-cpus=x86-64-v3,znver5 build --target x86_64-unknown-linux-gnu
```

Binary/package selection also uses normal Cargo arguments:

```bash
cargo run --manifest-path crates/cargo-sonic/Cargo.toml -- \
  sonic --target-cpus=x86-64-v3,znver5 build --bin server
cargo run --manifest-path crates/cargo-sonic/Cargo.toml -- \
  sonic --target-cpus=x86-64-v3,znver5 build --package my-crate --bin worker
```

## Auditable Binaries

Pass `--auditable` to embed one cargo-auditable-compatible dependency list in
the final fat binary:

```bash
cargo sonic --target-cpus=x86-64-v3,znver5 --auditable build --release
```

The audit data is collected once from `cargo metadata`, zlib-compressed, and
linked into the generated loader as a non-loaded ELF `.dep-v0` section. Payload
binaries are not individually annotated; the final executable carries the
dependency list for the selected package.

## Probe

Use `cargo sonic probe` to inspect the current host without building payloads:

```bash
cargo run --manifest-path crates/cargo-sonic/Cargo.toml -- \
  sonic --target-cpus=x86-64-v3,znver5,raptorlake probe
```

The probe uses the same `--target-cpus` list, asks rustc for each target-cpu's
feature contract, detects the current CPU, and prints which target-cpus are
eligible. It uses the same selection logic as the loader, but runs as a normal
`std` command.

## Runtime Environment

Before executing the selected payload, the loader removes old `CARGO_SONIC_*`
values and appends:

```text
CARGO_SONIC_ENABLED=1
CARGO_SONIC_SELECTED_TARGET_CPU=<target_cpu>
CARGO_SONIC_SELECTED_FLAGS=<rustc target_feature CSV>
```

Selection is enabled by default. Set `CARGO_SONIC_ENABLE=0` or
`CARGO_SONIC_ENABLE=false` to force the loader to select the `generic` payload:

```bash
CARGO_SONIC_ENABLE=0 target/sonic/x86_64-unknown-linux-gnu/release/sonic-cpu-benchmark
```

Application code can read these like normal environment variables:

```rust
fn main() {
    let cpu = std::env::var("CARGO_SONIC_SELECTED_TARGET_CPU")
        .unwrap_or_else(|_| "not-running-under-cargo-sonic".to_string());
    println!("selected target-cpu: {cpu}");
}
```

The application cannot inspect loader internals directly. The environment is
the supported observation surface.

## Loader Debugging

The loader is silent by default. Set `CARGO_SONIC_DEBUG` to make it print
selection diagnostics to stderr before it executes the selected payload:

```bash
CARGO_SONIC_DEBUG=1 target/sonic/x86_64-unknown-linux-gnu/release/sonic-cpu-benchmark
```

Debug output includes:

- detected host feature mask and feature names
- detected CPU identity
- every configured variant
- each variant's eligibility
- missing feature names for ineligible variants
- selected target CPU

## Example

See [examples/cpu-benchmark](examples/cpu-benchmark).

It has a `justfile`:

```bash
cd examples/cpu-benchmark
just build
just run
just check-loader
```

The example builds in release mode and runs a CPU-heavy floating point kernel
implemented directly in the example, with no crate-level runtime CPU dispatch.
On a Raptor Lake host, `just run` prints:

```text
selected target-cpu: raptorlake
```

Compare default selection with forced generic:

```bash
just compare
```

The loader itself prints nothing.

## Supported Targets

The intended v0 target set is:

```text
target_os = "linux"
target_arch = "x86_64" | "aarch64"
```

The currently verified paths are Linux x86_64 and Linux AArch64.

## Testing

Run pure unit tests, generated-loader integration tests, and fixture-driven
selector tests:

```bash
cargo test
```

Run only the modern CPU fixture suites:

```bash
cargo test -p cargo-sonic fixture_modern
```

These tests parse `tests/cpu-fixtures/x86_64-modern.tsv` and
`tests/cpu-fixtures/aarch64-modern.tsv`, synthesize loader `HostInfo` values
from CPUID family/model or AArch64 MIDR implementer/part rows, and assert that
the real loader selector chooses the expected rustc/LLVM target-cpu. They are
not QEMU tests and do not execute a generated binary; they cover modern CPU
identity cases that QEMU TCG cannot model as strict guest oracles.

Run the QEMU system-mode suite:

```bash
just qemu-prepare
just qemu
```

The QEMU correctness suite is system-mode only. It must boot a controlled Linux
guest for each CPU model listed in `tests/qemu/system.toml`, run rustc inside
that guest with `-C target-cpu=native`, run the cargo-sonic fat executable inside
the same guest, and compare the loader-selected target against the rustc-derived
expectation. Host `qemu-user`, host rustc, and checked-in selected-target goldens
are intentionally not used as correctness oracles.

QEMU cases run in parallel. The default worker count is the number of available
CPU cores; override it with `SONIC_QEMU_JOBS=<n>` when needed:

```bash
SONIC_QEMU_JOBS=4 just qemu
```

`just qemu-prepare` owns the asset directory under
`$CARGO_TARGET_DIR/sonic-qemu-system` when `CARGO_TARGET_DIR` is set, otherwise
`target/sonic-qemu-system`.
It downloads and builds the pinned QEMU version from source, downloads pinned
guest kernels/initrds, installs pinned guest Rust toolchains into cached Ubuntu
base rootfs directories, and leaves a generated asset README in that directory.

The default matrix is intentionally limited to CPU models where pinned QEMU TCG
can expose a runtime feature set consistent with the rustc native target. For
example, QEMU 11.0.0 reports `EPYC-Turin` as `znver5` to rustc, but TCG cannot
expose the AVX512 features required by rustc's `znver5` contract, so that model
is documented in `tests/qemu/system.toml` rather than kept as a permanently
failing exact-oracle case.

## Loader Minimalism Check

The example includes:

```bash
just check-loader
```

This verifies the generated loader executable has no ELF interpreter and no
dynamic libc dependency:

```bash
readelf -l target/sonic/x86_64-unknown-linux-gnu/release/sonic-cpu-benchmark
readelf -d target/sonic/x86_64-unknown-linux-gnu/release/sonic-cpu-benchmark
```

Expected:

```text
no INTERP segment
no NEEDED libc
```

## Workspace Layout

```text
crates/cargo-sonic          Cargo subcommand, build orchestration, and loader logic
crates/xtask                automation and QEMU matrix runner
examples/cpu-benchmark    minimal CPU benchmark example
tests/cpu-fixtures          fixture-driven modern CPU selector suites
tests/fixtures              integration fixtures
tests/qemu                  QEMU system-mode matrix
```
