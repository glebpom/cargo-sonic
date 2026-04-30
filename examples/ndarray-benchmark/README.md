# sonic-ndarray-benchmark

`ndarray`-based benchmark for `cargo-sonic`. This is intentionally less
specialized than `examples/cpu-benchmark`: the workload uses `ndarray` matrix
multiplication and array transforms instead of a hand-written element-wise loop.

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
SONIC_NDARRAY_N=512 SONIC_NDARRAY_ITERS=64 just compare
```
