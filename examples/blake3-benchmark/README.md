# sonic-blake3-benchmark

`blake3`-based benchmark for `cargo-sonic`. This example represents a workload
that is already heavily optimized by a dependency with runtime CPU feature
detection. Unlike the CPU benchmark, the hot path is the library hash
implementation, not application code that depends on LLVM auto-vectorization.

Build the fat executable:

```bash
just build
```

Compare the default selected payload against the forced baseline:

```bash
just compare
```

Tune the workload with:

```bash
SONIC_BLAKE3_MIB=128 SONIC_BLAKE3_ITERS=16 just compare
```
