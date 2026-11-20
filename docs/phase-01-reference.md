# Phase 0.1 reference comparison

The Phase 0.1 preview was compared with pinned Skylos 5.7.0 on two independently
reviewed fixtures. The comparison projects function findings to relative file,
qualified function name, and definition line. The comparison used CPython
3.11.14 on macOS ARM64. The pinned source revision, wheel hashes, dependency
versions, analyzer options, and source-built `skylos_fast` wheel are recorded in
the [baseline manifest](../tests/conformance/baselines/skylos-5.7.0.toml).

| Fixture | Skarmory root policy | Files in each run | Projected findings in each run |
| --- | --- | --- | --- |
| [Flat application](../tests/conformance/fixtures/reachable) | Default direct-file `main.py` | 3 | `main.dead_left` at line 8, `main.dead_right` at line 11, and `other.work` at line 1 |
| [Packaged application](../tests/conformance/preview/packages) | Source root `src` and named-module entry `shop.start` | 3 | `shop.service.dead_left` at line 13, `shop.service.dead_right` at line 17, and `shop.start.unused` at line 8 |

For each fixture, Skarmory reported complete coverage. Skylos reported all
three files, no analysis errors, no abstentions, and no additional nonempty
unused-code categories with the pinned options. Skylos marked both runs
`resolution_scope_limited`. The reference adapter observed one native discovery
call per accelerated run and zero with the accelerator blocked.

Both reference backends matched the exact projected findings and file set in
these cases. These results cover the stated projection. Skarmory reports a
versioned reachability result under declared roots, while the pinned Skylos
worker applies its own root and confidence policies. Confidence, evidence,
coverage semantics, and timing are outside this comparison.

## Reproduce

Build the native CLI with `cargo build --locked -p skarmory_cli`, and prepare
the pinned reference environment using the [Phase 0 reproduction
instructions](../benchmarks/phase0.md#reproduce). Run these commands from
the repository root with `REFERENCE_PYTHON` pointing to that environment's
Python interpreter.

```sh
FLAT="$PWD/tests/conformance/fixtures/reachable"
PACKAGE="$PWD/tests/conformance/preview/packages"
target/debug/skarmory "$FLAT"
target/debug/skarmory "$PACKAGE" --source-root src --entry-module shop.start
for fixture in "$FLAT" "$PACKAGE"; do
  for backend in python skylos_fast; do
    PYTHONHASHSEED=0 RAYON_NUM_THREADS=1 "$REFERENCE_PYTHON" \
      benchmarks/phase0.py --worker "$backend" "$fixture"
  done
done
```

For fixture assertions and worker equivalence, run
`python tests/conformance/check_phase01.py target/debug/skarmory`.
