# Stable-load RNG comparison: pending

The user requested a natural recovery of machine load before measuring the
complete pre/post RNG workload. The first observation period (2026-10-11T10:58:41.859776+00:00
to 2026-10-11T11:06:10.105399+00:00, 71 samples) did not meet the load gate even once. macOS memory
pressure remained at warning level 2, with intermittent background swapping.
**No new benchmark timing has been measured or labeled as stable.**

The foreground observer was stopped before service startup and replaced
with a heartbeat on this same conversation, checking every ten minutes.
The automation is named `稳定负载后复测 Wisp RNG` (id `wisp-rng`). It keeps
waiting while load is unchanged and pauses after completing the comparison.
User applications and background services are left under the user's control.

Raw observations: `results/rng-stable-20261011-b0-before.load.jsonl`.

## Prepared experiment

`stable-rng.py` runs the unchanged 2M-bar, 40-parameter, eight-child fair
workload in before/after/after/before blocks, nine measured rounds per block
after one discarded full warmup. One Wisp service is resident at a time.
The Rust daemon is fixed; the saved pre-RNG reactor and current reactor
are identified by SHA-256. Native still installs Seatbelt in each fresh
child, and all capability-denial probes and correctness checks remain.

Before startup, warmup and each measured round, `--idle-guard` requires
three consecutive observations with CPU idle >=80%, normal memory pressure
(level 1), zero swap-outs and <=64 swap-in pages/s. All failed and accepted
observations are saved outside timed work. Accepted timing rounds are also
checkpointed as `.rounds.jsonl` so an interrupted later gate does not lose
already measured samples. Nothing is filtered by whether it is fast.

The final report must evaluate IQR, adjacent-block order effects, Native
controls and swap deltas during actual work. Quiet starts alone do not prove
a whole-task speedup; a small gain inside the noise must be reported as
uncertain. On success this document will be replaced with the measurements.

The pre-change reactor is preserved locally at:

`runtime/cpython-wasi/vendor/cpython/cross-build/wasm32-wasip1/python-reactor-pre-rng-27966be.wasm`

It is byte-identical to the previous `/tmp` baseline, and is in the ignored
local build tree so a reboot or temporary-directory cleanup does not remove
the reference needed by the scheduled run.
