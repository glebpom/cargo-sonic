# CPU fixture suites

These fixtures are not QEMU inputs. They are data-driven selector tests for CPU
families that QEMU TCG cannot model as strict native-oracle guests.

Sources used when curating these rows:

- LLVM `lib/TargetParser/Host.cpp` host CPU name heuristics.
- Public CPUID/MIDR dump corpora such as InstLatx64, when checking that modern
  model numbers appear in real hardware dumps.

The expected value is the rustc/LLVM target-cpu spelling. Feature lists are the
runtime feature contract we want cargo-sonic to treat as available for the
synthetic host; they are intentionally compact and should include only features
known to cargo-sonic.
