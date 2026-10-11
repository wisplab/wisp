# Isolated Native versus Wisp branch

This macOS benchmark compares a warm Wisp service with a warm Native
CPython service. Each backend precomputes the price series and cumulative
sum once, then creates **a fresh COW child for every parameter**. Native
workers are not reused. Both have a maximum of eight concurrent children.

The trusted Native coordinator imports Python/NumPy and generates the
fixed benchmark dataset. After `fork`, before running parameter code,
each Native child installs this macOS Seatbelt profile:

```scheme
(version 1)
(deny default)
(allow sysctl-read)
```

Environment variables passed to the coordinator contain only PATH and
single-thread BLAS settings; its environment is cleared after bootstrap.
Children close the coordinator's input and other children's result pipes.
They retain the benchmark's output pipes. Seatbelt initialization happens
once **per child and is included in branch timing**. The coordinator remains
outside the sandbox, as the trusted launcher. This is a benchmark runner
for these fixed computations, not a general untrusted Python service;
arbitrary parent-session user code is not implemented on the Native side.

Before timing, Native probes verify denial of reading/writing an existing
canary file, connecting to a live loopback TCP listener, executing
`/usr/bin/true`, and forking another process. Probes must fail with
EPERM/EACCES, not ENOENT or connection refusal. Wisp is launched with host
capabilities disabled and its file read/write, web fetch, and shell bridge
are also probed. Any failed probe aborts the benchmark.

## Reproduce

From the repository root, with the release daemon and reactor already built:

```sh
python3 bench/branch-sweep/isolated.py --repeats 9 \
  --output bench/branch-sweep/results/isolated-original.json
python3 bench/branch-sweep/isolated.py --dataset binary-exact --repeats 9 \
  --output bench/branch-sweep/results/isolated-binary-exact.json
python3 bench/branch-sweep/isolated.py --bars 10000 --repeats 9 \
  --output bench/branch-sweep/results/isolated-small.json
```

The script starts its own Wisp daemon on an ephemeral loopback port and
stops it on completion. Snapshots and probe files are in a temporary directory
and are removed afterward. It does not download images or change application
sandbox settings. Seatbelt's `sandbox_init` API is deprecated, as documented
by the installed macOS manual; this experiment does not establish a supported
production deployment mechanism. Apple's discussion of sandbox inheritance:
https://developer.apple.com/forums/thread/747499

## Measurement boundaries

- Python/NumPy service bootstrap is outside the warm comparison for **both**
  implementations. Observed service startup is recorded separately; Wisp
  includes reactor compilation/bootstrap and Native includes Python imports.
- Total time includes parent setup, all child creation, sandbox installation
  on Native / snapshot and instances on Wisp, computation and gathering all
  results. Parent session disposal is excluded on both sides.
- Wisp uses its actual HTTP API; Native uses an actual subprocess JSON/pipe
  interface. Thus these are end-to-end implementation comparisons, including
  different transports, not isolated instruction-throughput measurements.
- One full sweep warms each backend; measured rounds shuffle backend order
  with a fixed seed. Medians, quartiles and every raw round are saved. Paired
  ratios and ratios of separate medians are different statistics and both
  are reported when laptop noise makes the distinction material.
- The default dataset is the original seed-7 random-price workload. The
  binary-exact control uses `100 + (arange % 1024) / 1024` so price and
  cumulative-sum inputs avoid RNG/libm differences between NumPy versions.
  Both execute the same moving-average sweep. Control totals cannot be
  substituted for the original workload's totals.
- Guest PnL is printed at full precision. All 40 results must agree to
  `1e-9`, and parent namespaces are checked for child-variable leakage.
- BLAS thread environment settings are 1, branch concurrency is 8,
  Wisp host acceleration is enabled. Native NumPy is 2.4.2, guest NumPy is
  1.26.4; both Python versions are 3.14.3. This remaining version difference
  means the results cannot attribute all overhead exclusively to WASM.

## Isolation scope

Seatbelt is an OS process sandbox sharing the host kernel. Wisp isolates
Python in WASM linear memory, with read-only runtime preopens and explicitly
controlled host imports. The mechanisms and accessible runtime files differ;
these probes do not prove equivalent security or a full security audit.
Neither benchmark imposes matching CPU-time/memory quotas. Neither provides
microVM-level kernel isolation. Native process isolation, filesystem/network
restrictions, and fresh per-task state remove the main advantages of the old
unsandboxed reusable fork pool, without claiming identical threat models.

## Results on 2026-10-11

Apple M2 (Mac14,2), eight cores, 8 GiB RAM; 40 parameter combinations,
eight simultaneous children, one discarded warmup plus nine measured rounds.
All times below are warm wall-clock medians in milliseconds.

| Dataset | Native fresh fork, no sandbox | Native fresh fork + Seatbelt | Wisp branch | Wisp / isolated Native, ratio of medians |
|---|---:|---:|---:|---:|
| Original random prices, 2M bars | 284.5 | 337.2 | 538.0 | 1.60 |
| Binary-exact control, 2M bars | 305.3 | 355.7 | 467.9 | 1.32 |
| Original random prices, 10K bars | 33.2 | 104.0 | 62.6 | 0.60 |

For the original 2M workload, the isolated Native setup/branch medians are
32.4 / 304.2 ms, and Wisp's are 81.2 / 450.9 ms. These component medians
are calculated independently and need not sum to the total median. Wisp's
snapshot median is 23.6 ms; the remaining branch time also contains fresh
instance creation, COW faults, computation, scheduling and response handling.
The measured snapshot time alone does not explain the whole gap.

The original 2M total-time interquartile ranges are 332.0–351.5 ms for
isolated Native and 414.4–642.9 ms for Wisp. There are substantial outliers
(up to 888.4 and 920.5 ms respectively). The **median of same-round ratios**
is 1.27, whereas the **ratio of separate medians** is 1.60. Both are in the
raw results. This noisy laptop run establishes that isolated Native remains
competitive/faster for the larger workload; it does not establish a precise,
constant 60% runtime penalty. Fixed per-child Seatbelt initialization cost
is more visible on the short workload, where Wisp is faster.

All 40 full-precision PnL outputs agree: maximum absolute errors are
7.78e-15 (original 2M), 8.89e-16 (binary-exact 2M), and below 1e-9 for the
small sweep. These are tolerance comparisons, not claims of bitwise output
equality. The Native and Wisp security probes passed before timing. No
matching CPU/memory quotas were added on either backend.

For the original run, observed service startup was approximately 108 ms for
the isolated Native coordinator and 5.27 s for Wisp. These single startup
observations are outside the warm table, include different bootstrap work,
and are not a repeated cold-start benchmark.

Raw data: `results/isolated-20261011-original.json`,
`results/isolated-20261011-binary-exact.json`, and
`results/isolated-20261011-small.json`. The binary-exact control was run before
the additional Wisp bridge probes were added; its Native probes and result
checks passed, and it used the same disabled-capability daemon configuration.

## Optimized runtime follow-up

See [OPTIMIZATION.md](OPTIMIZATION.md) for the subsequent profiling and
optimization. The candidate's original 2M warm sweep median is 407 ms,
compared with 536 ms for a fresh old-binary batch and 338 ms for the candidate
batch's isolated Native control. The macOS candidate now uses an in-memory
kernel COW snapshot. All original results above remain historical records.

`--wisp-binary PATH` selects a daemon binary. `--baseline-binary PATH` adds
an older service to each shuffled round; it needs extra RAM. `--profile-arrays`
adds parent/child operation timers and belongs in a separate diagnostic run.
`--snapshot-backend file` / `--no-prelink` select candidate ablations. Global
VM counters are recorded outside backend timing in newer results.

## Selecting a reactor

Use `--wisp-reactor /absolute/path/python-reactor.wasm` to compare guest
changes while holding the Rust daemon fixed. The harness passes this exact
path through `WISP_PYTHON_WASM` and records its SHA-256 as well as the daemon
SHA-256. Without the option it explicitly uses the repository's default
reactor. This prevents an inherited environment variable selecting another
reactor. The optional older-binary backend uses the same selected reactor.
