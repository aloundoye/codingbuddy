# Coding Quality Benchmark

This benchmark tracks whether coding-task behavior is improving or regressing over time.

## Scope

The current deterministic suite covers 4 task classes:

- `edit-single-file`
- `debug-bugfix`
- `refactor-rename`
- `multi-file-update`

Each case records:

- pass/fail
- tool invocation count
- retry count (invocations above expected minimum)
- completion quality score (`1.0`, `0.5`, `0.0`)
- execution duration

## Run

Use the helper script:

```bash
./scripts/run_coding_quality_benchmark.sh
```

It runs:

```bash
cargo test -p codingbuddy-agent --test coding_quality_benchmark -- --nocapture
```

The test writes a report to:

```text
.codingbuddy/benchmarks/coding-quality-core.scripted-tool-loop.latest.json
```

## Baseline Gate

Baseline file:

```text
docs/benchmarks/coding_quality_baseline.json
```

Gate rule in the test:

- fail if pass-rate drops by more than `5.0` percentage points vs baseline
- fail if average completion quality score drops by more than `0.10`
- fail if average retries increase by more than `0.50`
- fail if baseline suite/model identity does not match current report

Update baseline only when an intentional quality shift is accepted.
