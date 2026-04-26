# sonic-variant-printer

Minimal `cargo-sonic` example. `generic` is implicit and is always built, so it is
not listed in `Cargo.toml`.

Build the fat executable:

```bash
just build
```

Run it:

```bash
just run
```

Force the generic payload:

```bash
just run-generic
```

Compare the default selection against generic:

```bash
just compare
```

The loader is silent. The output comes from the application, which reads:

```text
CARGO_SONIC_SELECTED_TARGET_CPU
```

The application also runs a CPU-heavy floating point kernel implemented directly
in `src/main.rs`, with no crate-level runtime CPU dispatch. Increase work with:

```bash
SONIC_EXAMPLE_LEN=524288 SONIC_EXAMPLE_ITERS=3000 just compare
```

The generated executable is written to:

```text
target/sonic/x86_64-unknown-linux-gnu/release/sonic-variant-printer
```
