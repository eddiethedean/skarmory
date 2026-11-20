# Changelog

## 0.0.1

Released November 20, 2026.

- Introduces the native CLI and Python SDK for detecting unreachable
  functions in a flat, closed-world Python 3.11 application.
- Reports findings, uncertainty, and incomplete coverage in the same versioned
  JSON format across both interfaces.

See the [Phase 0 contract](docs/phase-0.md) for supported syntax and scan limits.

### Published artifacts

- PyPI has eight `cp311-abi3` wheels for standard CPython 3.11+ and one source
  distribution. [PyPI files](https://pypi.org/project/skarmory/0.0.1/#files)
- crates.io has four workspace crates at version 0.0.1. Install `skarmory_cli`
  for the native `skarmory` command. [`skarmory_cli`](https://crates.io/crates/skarmory_cli/0.0.1) ·
  [`skarmory_core`](https://crates.io/crates/skarmory_core/0.0.1) ·
  [`skarmory_python`](https://crates.io/crates/skarmory_python/0.0.1) ·
  [`skarmory_bindings`](https://crates.io/crates/skarmory_bindings/0.0.1)

| Platform | x86-64 wheel tag | ARM64 wheel tag |
| --- | --- | --- |
| Linux (glibc) | `manylinux_2_17_x86_64` | `manylinux_2_28_aarch64` |
| Linux (musl) | `musllinux_1_2_x86_64` | `musllinux_1_2_aarch64` |
| macOS | `macosx_10_12_x86_64` | `macosx_11_0_arm64` |
| Windows | `win_amd64` | `win_arm64` |

All 19 release jobs passed on commit `602aec4`, including all eight wheel
builds, installation checks, and publication to both registries.
[`v0.0.1` source tag](https://github.com/eddiethedean/skarmory/tree/v0.0.1) ·
[Release CI](https://github.com/eddiethedean/skarmory/actions/runs/44782566713)
