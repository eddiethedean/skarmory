# Phase 0 measurements

The harness runs the release CLI and the pinned Skylos 5.7.0 API in fresh
processes. The local function projection compares file, fully qualified name,
and line. Skylos computes broader evidence and uses confidence and listing
policy. These measurements do not establish a matched speedup.

## Reproduce

Use CPython 3.11.14 on macOS arm64 to reproduce the recorded dependency
environment. The [baseline](../tests/conformance/baselines/skylos-5.7.0.toml)
contains the wheel URL and checksum. Download that wheel and verify its SHA-256
before installing it with the pinned dependencies. Build the accelerator from
the exact upstream revision with its Cargo lockfile.

```bash
python3.11 -m venv /tmp/skarmory-reference
source /tmp/skarmory-reference/bin/activate
python -m pip install maturin==1.16.0
git clone --branch v5.7.0 https://github.com/duriantaco/skylos.git /tmp/skylos-5.7.0
test "$(git -C /tmp/skylos-5.7.0 rev-parse HEAD)" = 87978e69adf91b3811e97b00c40aa39d7937cd94
maturin build --release --locked --manifest-path /tmp/skylos-5.7.0/rust/Cargo.toml -i python --out /tmp/skylos-wheels
python - <<'VERIFY_WHEEL'
from hashlib import sha256
from pathlib import Path
import tomllib
from urllib.request import urlopen

baseline = Path("tests/conformance/baselines/skylos-5.7.0.toml")
artifact = tomllib.loads(baseline.read_text())["distribution"]
with urlopen(artifact["url"]) as response:
    data = response.read()
assert sha256(data).hexdigest() == artifact["sha256"]
Path("/tmp/skylos-wheels", artifact["wheel"]).write_bytes(data)
VERIFY_WHEEL
python -m pip install -r tests/conformance/baselines/skylos-5.7.0-macos-arm64-cp311.txt \
  /tmp/skylos-wheels/skylos-5.7.0-*.whl /tmp/skylos-wheels/skylos_fast-*.whl
cargo build --locked --release -p skarmory_cli
python tests/conformance/check_phase0.py target/release/skarmory
python benchmarks/phase0.py target/release/skarmory /tmp/skarmory-reference/bin/python --samples 5 > phase0-results.json
```

The harness uses the same sources for both tools in standalone temporary
directories. It verifies Skylos' file selections and requires successful native
discovery in the accelerated runs. It uses a fresh process per sample, a
rotating run order, and one Rayon thread. The OS page cache is uncontrolled.
The native CLI additionally reports discovery, read/decode, parse/extract,
resolution, reachability, rule, and output timings.

To measure distributions separately, build the regular SDK wheel and install
the Cargo CLI to a temporary prefix. The script creates fresh environments for
both supported separate artifacts and the combined-wheel experiment.

```bash
maturin build --release --locked --interpreter python --out dist
cargo install --locked --path crates/cli --root dist/cli
python scripts/check_distributions.py dist/skarmory-*.whl dist/cli/bin/skarmory --combined
```

## macOS reference run

Apple M4 Pro, 12 cores, 24 GB RAM, macOS 26.5.2 arm64, Rust 1.96.0 release
profile with thin LTO and CPython 3.11.14. The host was shared and OS caching
was uncontrolled. Each complete workload has five fresh-process samples per
mode. The incomplete controls have one sample per mode. Raw wall time, user
and system CPU time, peak RSS, source hashes, phase samples, and coverage
observations are in the [run record](results/phase0-macos-arm64.json).

| Workload | Files / lines | Native CLI median | Skylos Python API median | Skylos API + accelerator median | Projected function findings per tool |
| --- | ---: | ---: | ---: | ---: | ---: |
| Paired reachability | 3 / 32 | 3.73 ms | 605.57 ms | 569.86 ms | 3 |
| Generated flat application | 33 / 897 | 5.61 ms | 935.51 ms | 897.63 ms | 256 |

For the generated case, median native stages were 107 µs discovery, 1,573 µs
read/decode, 202 µs parse/extract, 15 µs resolution, 0.71 µs reachability, 17 µs
rule evaluation, and 71 µs output. These phase medians use different samples
and do not add to the end-to-end median.

The modes selected the same fixture files. Both Skylos backends agreed on the
recorded projection, including coverage controls. The native findings matched
both backends on the two supported workloads and matched the independent source
expectations. Skylos emits richer per-finding evidence and confidence. Its
resolution scope is also flagged as limited under the benchmark configuration
with import-cycle checks disabled. No speedup ratio is reported.

## Reviewed differences

| Case | Observed difference | Classification |
| --- | --- | --- |
| Invalid function | Ruff reports unexpected EOF at line 2; CPython reports the unclosed parenthesis at line 1. Both record a parse failure. | Intentional native diagnostic formatting |
| Python 3.12 type alias | The adapter records the Python 3.11 target mismatch; Skylos on CPython 3.11 reports a syntax error. | Intentional target-version diagnostic |
| Unresolved external call | The native report records uncertainty and withholds findings. Both Skylos backends list `main.maybe_unused`. | Intentional prototype coverage policy |
| Unsupported assignment | The native report is incomplete. Both Skylos backends list `main.unused` and include an unused-variable category. | Unsupported native syntax and rule family |
| Process status | The native CLI exits 2 for incomplete scans. The comparison invokes the Skylos Python API, which returns diagnostics in JSON and leaves the harness process successful. | Distinct entry-point contracts |

## Distribution evidence

[Linux arm64, CPython 3.11](results/distributions-linux-arm64-cp311.json)
records the Cargo-installed CLI, SDK wheel rebuilt from the source distribution,
and combined-wheel experiment. The tests ran on Debian 12 with Rust 1.96.0 and
verified the same fixture reports in both frontends. Hosted CI supplies the
rest of the platform matrix.
