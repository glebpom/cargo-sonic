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

### The Compiler Optimization Gap

Many high-performance libraries, such as cryptography or compression libraries,
use manual runtime detection to switch between hand-written assembly
implementations. Most application code does not have hand-written alternatives
for every possible CPU.

For general-purpose code, performance depends on the compiler. When LLVM is
restricted to a generic target, it must be conservative. It cannot safely use
modern instruction sets such as AVX-512, and it cannot apply
microarchitecture-specific scheduling weights that optimize instruction ordering
for a particular chip pipeline. Target CPU selection can also change inlining,
basic block placement, loop layout, and the final instruction layout that the
processor frontend sees. Auto-vectorization matters, but it is not the only
reason target-specific builds can be faster.

### Performance Impact

The difference between a generic build and one tuned for a specific
microarchitecture can be substantial, particularly for compute-heavy tasks where
the compiler can use CPU-specific instructions, scheduling weights, and code
layout decisions.

The examples folder includes three release-mode benchmarks. The first is an
aggressive CPU-heavy floating point kernel that repeatedly updates three large
`f32` buffers and accumulates a checksum. On a Raptor Lake host, the performance
delta is clear:

| Selection Mode | Target CPU | Execution Time |
| --- | --- | ---: |
| Sonic (Optimized) | `raptorlake` | 154 ms |
| Baseline Fallback | `x86-64` | 2771 ms |

The second benchmark is intentionally less specialized. It uses `ndarray` matrix
multiplication and array transforms instead of a hand-written element-wise hot
loop. On the same Raptor Lake host, a local release run with `n=384` and
`iterations=48` produced these median times over five runs:

| Benchmark | Selection Mode | Target CPU | Execution Time |
| --- | --- | --- | ---: |
| `examples/ndarray-benchmark` | Sonic (Optimized) | `raptorlake` | 49 ms |
| `examples/ndarray-benchmark` | Baseline Fallback | `x86-64` | 66 ms |

This is a more realistic result: useful for this workload, but nowhere near the
extreme delta from the synthetic element-wise benchmark.

The third benchmark, `examples/blake3-benchmark`, uses a crypto/hash library
that already performs runtime CPU feature detection and dispatches to optimized
implementations internally. It is useful for measuring workloads where
`cargo-sonic` can still tune surrounding code, even when the dominant library
code is already doing its own feature-based selection.

Even in this extremely optimized case, a local run with `input-mib=64` and
`iterations=8` still improved throughput when the loader selected `znver5`:

| Benchmark | Selection Mode | Target CPU | Execution Time | Throughput |
| --- | --- | --- | ---: | ---: |
| `examples/blake3-benchmark` | Sonic (Optimized) | `znver5` | 79 ms | 6479.0 MiB/s |
| `examples/blake3-benchmark` | Baseline Fallback | `x86-64` | 90 ms | 5674.9 MiB/s |

### Optimized Portability

`cargo-sonic` is useful when you want one Linux artifact that can run across a
mixed fleet, but you still want the compiler to produce code for several CPU
families. It is not a replacement for `-C target-cpu=native` when you already
know the deployment machine, and it is not a general substitute for targeted
runtime dispatch in a small hot loop.

- **Microarchitecture tuning:** enables the compiler to use efficient
  instruction weights, vectorization strategies, and code layout choices for the
  specific host.
- **Automated fallback:** includes a Rust baseline payload to preserve
  compatibility with legacy hardware or restricted environments.
- **Infrastructure simplicity:** provides the performance of specialized builds
  within a single portable deployment unit.

By resolving selection at the loader level, `cargo-sonic` lets Rust applications
use more of the underlying hardware without writing application-level dispatch
code. The cost is that payloads are whole binaries, so size and build time scale
with the number of selected target CPUs.

## Tradeoffs

`cargo-sonic` is deliberately a coarse-grained tool: it builds complete payload
binaries for multiple `target-cpu` values and selects one at process startup.
That has clear costs.

### Binary Size

The embedded loader stores one payload per target CPU plus the implicit baseline
payload. Without compression, final binary size is roughly proportional to the
number of payloads. Rust binaries often link dependencies statically, so this
can be a large increase.

Use a small target list. Prefer broad levels such as `x86-64-v3`/`x86-64-v4`
when they cover your fleet, and add concrete CPUs only when you have evidence
that they matter. `--compress=zstd` can reduce artifact size, but it is not free:
compressed payloads must be decompressed before execution, and dictionary
compression is only useful when it wins after accounting for dictionary and
decoder overhead.

For container images, `--loader=bundle` is often the more practical layout. It
keeps a small launcher at the normal binary path and stores payloads next to it
in `<bin>.bundle/`, which can preserve fast startup for uncompressed payloads.

### Build Time

Each target CPU is a real Cargo build with its own target directory and linker
step. That is required because `target-cpu` and `target-feature` can affect code
generation, inlining, and crates that compile target-specific code.

Use `--parallelism` on CI builders with enough cores and memory:

```bash
cargo sonic --target-cpus=x86-64-v3,znver5 --parallelism=2 build --release
```

Build caches such as `sccache` can help, but LTO and final codegen/link steps
still run per payload.

### Startup And Memory

The loader does not make Linux load every embedded payload into resident memory.
The final executable is mapped by the kernel, and only touched pages become
resident. However, embedded mode must still materialize the selected payload
before `execveat`, either through the fast filesystem path or by copying into a
`memfd`. Large debug binaries and short-lived CLI tools can therefore start much
slower than the plain binary.

Some constrained environments also care about virtual memory limits even when
RSS stays low. Benchmark with the same container limits and filesystem you use
in production.

### Expected Performance

The synthetic example benchmark is intentionally CPU-heavy and shows a case
where LLVM can make a large difference from target-specific code generation.
Real applications should be judged by where they spend CPU time under load.

Any CPU-bound part of an application may benefit, including services commonly
described as I/O-heavy. Request parsing, validation, routing, compression,
serialization, query planning, protocol handling, checksums, filtering, and
business logic all execute on the CPU and can affect tail latency when the
service is saturated. The more time a workload spends in such code, the more
room there is for target-specific code generation to matter.

The opposite is also true: the closer a service is to a trivial byte-copy or
syscall-forwarding path, the less likely `cargo-sonic` is to improve throughput
or latency.

`cargo-sonic` is a better fit for:

- CPU-bound servers and workers
- long-running services where startup is amortized
- analytics, compression, search, simulation, media, storage engines, or
  nontrivial network services
- one Docker image or binary that runs across a mixed VM/cloud fleet

It is usually a poor fit for:

- startup-sensitive CLI tools
- tiny binaries where size matters more than throughput
- deployments pinned to one known CPU model
- embedded or storage-constrained devices

### Relation To Function Multiversioning

Function-level multiversioning and manual runtime dispatch can be more precise:
only hot functions are duplicated, and the rest of the binary stays generic.
That can produce a smaller artifact and lower startup cost. It also requires the
application or libraries to be structured around dispatch points, and it only
helps code that has been explicitly multiversioned.

`cargo-sonic` instead asks LLVM to optimize the whole binary for each selected
target CPU. That is simpler to apply to an existing application, but it is a
bigger hammer. Use function-level dispatch for narrow hot paths when you can;
use `cargo-sonic` when whole-program compiler tuning and deployment simplicity
are the tradeoff you want.

## Status

This repository currently has a working Linux x86_64/AArch64 vertical slice:

- CLI target CPU selection through `--target-cpus`
- implicit baseline payload, always built
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

## Install

Install the Cargo subcommand:

```bash
cargo install cargo-sonic
```

Then run it from the crate you want to build:

```bash
cargo sonic --target-cpus=x86-64-v4,znver5 build --release
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

To build a different manifest, pass Cargo's normal `--manifest-path` flag:

```bash
cargo sonic --target-cpus=x86-64-v4,znver5 build \
  --manifest-path examples/cpu-benchmark/Cargo.toml
```

## Target CPUs

Choose variants at build time with `--target-cpus`:

```bash
cargo sonic --target-cpus=znver5,x86-64-v4,icelake-server build --release
```

Payload builds run sequentially by default. Pass `-p`/`--parallelism` before the
`build` subcommand to compile multiple target-cpu payloads at once:

```bash
cargo sonic --target-cpus=x86-64-v3,znver5 -p 2 build --release
```

Pass `--compress=zstd` to store payloads compressed inside the final fat binary.
The compression level is controlled separately:

```bash
cargo sonic --target-cpus=x86-64-v3,znver5 --compress=zstd --compression-level=10 build --release
```

The default loader strategy is `--loader=embedded`, which produces one
self-contained fat executable. For container images and other layouts where a
directory can travel with the launcher, `--loader=bundle` builds a small CPU
detection launcher at the usual output path and places the concrete payload
binaries in an adjacent `<bin-name>.bundle/` directory:

```bash
cargo sonic --target-cpus=x86-64-v3,znver5 --loader=bundle build --release
```

With uncompressed payloads, the bundle loader avoids copying or decompressing
the selected payload at startup; it directly `exec`s the matching binary from
the bundle directory. Uncompressed bundle payloads use the `.elf` extension;
compressed bundle payloads use `.elf.zstd`. This is faster to start, works well
in Docker images, and should be comparable to a single generic build for startup
overhead. It is not a single-file binary distribution format. Compression can
still be combined with bundle mode, but it gives up the fast-start benefit
because the selected payload must be decompressed before execution.

With `--parallelism=1`, payload build output is passed through without
cargo-sonic block headers. Output from parallel payload builds is buffered
briefly and printed in blocks headed by `cargo-sonic[<target-cpu>]` so
interleaved cargo output remains attributable. Cargo's normal
`--color auto|always|never` argument is supported; with `auto`, cargo-sonic
preserves colors when its output is attached to a terminal even though child
Cargo output is buffered through pipes.

The baseline target CPU is implicit. It is always built and always eligible at
runtime, so do not list it in `--target-cpus`. On x86_64 the baseline is
`x86-64`; on AArch64 it is `generic`. Pass at least one non-baseline CPU; with
only one baseline payload there is no reason to use `cargo-sonic`.

Rules:

- `--target-cpus` is required.
- the target baseline is added automatically and rejected if listed explicitly.
- `native` is rejected.
- CPU names must exactly match local `rustc --print target-cpus` names.
- Cross-architecture CPU names are skipped for the current target.
- Unknown CPU names are hard errors.

The target triple comes from normal Cargo arguments:

```bash
cargo sonic --target-cpus=x86-64-v3,znver5 build --release
cargo sonic --target-cpus=x86-64-v3,znver5 build --target x86_64-unknown-linux-gnu
```

To inspect what the current machine can run, use `score`. It does not require
`--target-cpus`; it scores the current host against rustc-supported target CPUs
for the selected target and prints compatible CPUs in runtime selection order:

```bash
cargo sonic score
cargo sonic score --target x86_64-unknown-linux-gnu
```

Binary/package selection also uses normal Cargo arguments:

```bash
cargo sonic --target-cpus=x86-64-v3,znver5 build --bin server
cargo sonic --target-cpus=x86-64-v3,znver5 build --package my-crate --bin worker
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
cargo sonic --target-cpus=x86-64-v3,znver5,raptorlake probe
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
`CARGO_SONIC_ENABLE=false` to force the loader to select the baseline payload:

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

See [examples/cpu-benchmark](examples/cpu-benchmark) and
[examples/ndarray-benchmark](examples/ndarray-benchmark).

It has a `justfile`:

```bash
cd examples/cpu-benchmark
just build
just run
just check-loader
```

The CPU benchmark builds in release mode and runs a CPU-heavy floating point
kernel implemented directly in the example, with no crate-level runtime CPU
dispatch. On a Raptor Lake host, `just run` prints:

```text
selected target-cpu: raptorlake
```

Compare default selection with forced baseline:

```bash
just compare
```

The loader itself prints nothing.

The ndarray benchmark uses a general-purpose numeric crate:

```bash
cd examples/ndarray-benchmark
just compare
```

It is intended as a less aggressive benchmark for ordinary Rust numeric code.

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
rustup target add x86_64-unknown-linux-gnu aarch64-unknown-linux-musl
just qemu-prepare
just qemu
```

The QEMU correctness suite is system-mode only. It must boot a controlled Linux
guest for each CPU model listed in `tests/qemu/system.toml`, run rustc inside
that guest with `-C target-cpu=native`, run both cargo-sonic loader strategies
inside the same guest, and compare each loader-selected target against the
rustc-derived expectation. Host `qemu-user`, host rustc, and checked-in
selected-target goldens are intentionally not used as correctness oracles.
The generated test binaries include only the rustc target-cpus needed by the
configured QEMU cases; cargo-sonic adds the architecture baseline payload
implicitly.

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
examples/ndarray-benchmark ndarray-based benchmark example
tests/cpu-fixtures          fixture-driven modern CPU selector suites
tests/fixtures              integration fixtures
tests/qemu                  QEMU system-mode matrix
```
