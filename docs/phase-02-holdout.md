# Phase 0.2 carry-forward holdout

These two cases pin already shipped source independently of the new Phase 0.2
conformance suite. The inputs come from Skarmory's v0.1.0 tag at
`162d58b4332248ebeb4a166a7b2181a2367e4ef0`. They were selected and labeled
before running the Phase 0.2 comparison. The fixtures already served as
Phase 0.1 controls. This holdout checks for regressions across the new profile
and does not provide an external accuracy sample.

## Frozen expectations

| Input | Boundary | Independently reviewed function expectations |
| --- | --- | --- |
| `tests/conformance/preview/packages` at the tag | Three files under `src/shop`, launched as `shop.start`. The parent initializer runs before the entry. | Report `shop.service.dead_left`, `shop.service.dead_right`, and `shop.start.unused`. Retain the package bootstrap, the definition-time default helper, and the call chain from the main guard into `guarded`, `handle`, `live_left`, and `live_right`. |
| `tests/conformance/preview/library/api.py` at the tag | A single public module. Its effective literal `__all__` exposes `factory` and noncallable `metadata`. | Report `api.private`. Retain `factory`, both branches it can take with external input, and the nested callback and its helper because the returned callable can be invoked. The dormant assignment to `__all__` leaves the public boundary unchanged. |

The application contains a live recursive cycle. Its positive expectations
describe static reachability, with no expectation that statements after that
cycle execute in CPython. The comparison does not score those subsequent
statements or findings outside the unused-function projection. The library
input is the single frozen file. The separate `unbound.py` negative control
has its own coverage contract.

## Observed results

Run on macOS ARM64 with a Rust 1.98.1 native build and the pinned CPython
3.11.14 reference environment. Each scan used one worker, a fresh process,
and the dead-code settings in the
[Skylos baseline manifest](../tests/conformance/baselines/skylos-5.7.0.toml).
The projection compares relative file, qualified function name, and line.

| Input | Native 0.1 profile | 0.2 Skylos profile | Skylos 5.7.0, both backends |
| --- | --- | --- | --- |
| Packaged application | Complete, three files and three expected findings | Complete, same files and findings | Same files and findings with no analysis errors |
| Library | Complete, one file and `api.private` at line 25 | Complete, same file and finding | Same file, no unused-function findings and no analysis errors |

The library's returned callback remains retained as possible use in both
Skarmory profiles. Native uncertainty points to the public entry, while
compatibility abstention points to the return expression. Skylos reports the
callback as referenced and records `api.private` as alive with public-export
evidence. Lowering the Python backend's threshold to zero still gives no
finding for `api.private`.

The library result is an intentional difference under the declared boundary.
The final list names `factory` and `metadata`. The assignment naming
`private` is unreachable, and the fixture contains no call or other export
of that function. The independent expectation stays `api.private` at line 25.
The application has no differences in the stated projection. Both Skylos
backends set `resolution_scope_limited`, so their coverage fields carry a
different meaning from Skarmory's completion status. These cases establish
no timing or general accuracy claim.

## Reproduce

Build the CLI with `cargo build --locked -p skarmory_cli`. Prepare the
reference interpreter using the [Phase 0 instructions](../benchmarks/phase0.md#reproduce)
and assign its executable path to `REFERENCE_PYTHON`. Clear ambient
`SKYLOS_*` options and `PYTHONPATH` before running from the repository root.

```sh
scratch=$(mktemp -d)
scratch=$(cd "$scratch" && pwd -P)
git archive v0.1.0 tests/conformance/preview/packages | tar -x -C "$scratch"
app="$scratch/tests/conformance/preview/packages"
library="$scratch/library"
mkdir "$library"
git show v0.1.0:tests/conformance/preview/library/api.py > "$library/api.py"

target/debug/skarmory "$app" --source-root src --entry-module shop.start
target/debug/skarmory "$library" --policy library --public-module api
for fixture in "$app" "$library"; do
  target/debug/skarmory "$fixture" --profile skylos --json --no-grep-verify \
    --all-advice --include-generated --confidence 60 \
    --config-file tests/conformance/baselines/phase0-skylos-config.toml
  for backend in python skylos_fast; do
    PYTHONHASHSEED=0 RAYON_NUM_THREADS=1 "$REFERENCE_PYTHON" \
      benchmarks/phase0.py --worker "$backend" "$fixture"
  done
done
```
