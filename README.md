# memory-harness

`memory-harness` is a small Linux-only Rust CLI that runs a child command and
reports its kernel-accounted resource usage, especially peak RSS.

The entire purpose of this repo is to build a memory harness with low enough
wrapper noise that `perf` can run concurrently and still produce a useful CPU
profile for the wrapped program.

That goal drives the design:

- keep the Rust binary small
- use kernel accounting like `getrusage` for peak RSS
- make optional sampling lightweight
- keep the default profiling workflow synchronized in one run
- minimize wrapper-side work that would drown out the target process

It is intentionally narrow:

- one command per invocation
- no internal run matrix
- no statistical aggregation
- no allocator-specific profiling

## What it measures

The wrapped command's result includes:

- wall clock time
- peak RSS in KiB and MiB
- user CPU time
- system CPU time
- minor and major page faults
- voluntary and involuntary context switches
- exit code or terminating signal
- optional throughput metadata:
  - `objects_processed` (caller-supplied count)
  - `objects_per_second` (derived from elapsed time)
  - `seconds_per_object` (derived from elapsed time)

The harness also reports its own peak RSS separately so you can see whether the
wrapper overhead is small relative to the child process.

## Cgroup v2 aggregate memory (best effort)

Where cgroup v2 memory accounting is available and writable, the harness also
collects an aggregate memory view from a short-lived run cgroup:

- current memory (`memory.current`)
- peak memory (`memory.peak`)
- `anon`, `file`, and `shmem` fields from `memory.stat`

This gives coverage for descendant/worker processes and cache-backed memory pressure
that is not visible from process-only `/proc/<pid>` accounting.

The report JSON includes this as `child_cgroup_memory` when available, including
`memory.current`, `memory.peak`, `anon`, `file`, `shmem`, `cache`, `rss`, and
`rss_huge` totals for the run cgroup.
When cgroup v2 is not available or setup fails, the report still includes
`child_cgroup_memory_status` with a short reason.

## Optional proc-status sampling

The harness can also sample `/proc/<pid>/status` while the child is running.
This is complementary to `ru_maxrss`, not a replacement for it.

Use it when you want a lightweight time series in addition to the final kernel
high-water mark:

```bash
cargo run --release -- \
  --format json \
  --sample-proc-status-ms 10 \
  --sample-proc-status-out tmp/proc-status.tsv \
  -- my-program arg1 arg2
```

What this adds:

- a TSV file with elapsed time and RSS-related fields from `/proc/<pid>/status`
- summary fields in the final report for sampled `VmRSS`, `VmHWM`, anon/file/shmem peaks, and thread peak

Why it exists:

- `ru_maxrss` is the authoritative lifetime peak
- `/proc/<pid>/status` helps explain the shape of the run over time

## Why the code uses `getrusage`

The point of the harness is peak-memory measurement, not point-in-time sampling.

That distinction matters:

- `/proc/<pid>/status` tells you what RSS looks like when you read the file.
- `getrusage` gives you the kernel-maintained high-water mark in `ru_maxrss`.

If the target spikes memory for a short interval and then frees it before you
sample `/proc`, a polling design can miss the peak. `ru_maxrss` exists so the
kernel can report the maximum resident set size observed over the process
lifetime.

This repo uses:

- `wait4(2)` to collect the child process `struct rusage` on exit
- `getrusage(RUSAGE_SELF)` to show the harness process overhead

The child result is the primary measurement. The harness self result is there to
make the wrapper cost explicit.

## Build setup

The repo carries scaffold-derived local build config in `.cargo/config.toml`.
That file assumes these tools are available:

- `clang`
- `mold`
- `sccache`

If your machine does not have them, adjust `.cargo/config.toml`.

## `Cargo.toml` settings that matter for `perf`

The release profile intentionally keeps full DWARF debug sections in the ELF:

```toml
[profile.release]
opt-level = 1
lto = false
codegen-units = 16
incremental = true
debug = 2
split-debuginfo = "off"
strip = "none"
```

What these variables do:

- `debug = 2` enables full debug info.
- `split-debuginfo = "off"` keeps the debug sections in the binary instead of
  moving them into side files.
- `strip = "none"` keeps symbols and debug data intact.

Those three fields are the key ones for readable `perf report` output.

The repo also defines a `ship` profile:

```toml
[profile.ship]
inherits = "release"
opt-level = 3
lto = true
codegen-units = 1
incremental = false
```

Use `ship` when you want a more fully optimized build while still inheriting the
debug-section settings from `release`.

## Build

Standard release build:

```bash
cargo build --release
```

More optimized build:

```bash
cargo build --profile ship
```

## Use the harness

Run a program with inherited stdout and stderr:

```bash
cargo run --release -- -- /usr/bin/env true
```

Wrap a command in a different working directory:

```bash
cargo run --release -- --cwd /tmp -- git status
```

Emit JSON instead of human-readable output:

```bash
cargo run --release -- --format json -- /usr/bin/env true
```

Pass a processed-object count to get throughput fields:

```bash
cargo run --release -- --format json --objects-processed 1950 -- /usr/bin/env true
```

Pass environment variables through the harness:

```bash
cargo run --release -- --env RUST_LOG=debug --env FOO=bar -- my-program --flag
```

Try the included demo target:

```bash
cargo run --example spiky-alloc -- 8 512 20
```

Run a fixed-length sample where the harness owns the timeout boundary:

```bash
cargo run --release -- \
  --format json \
  --child-timeout-seconds 3 \
  -- my-program arg1 arg2
```

## How to use it with `perf`

Use the harness and `perf` together when you want one timestamped artifact bundle
from a single wrapped run.

The preferred repo workflow is the wrapper script, which starts
`memory-harness`, waits for it to announce the child PID, attaches `perf`
directly to that child process, and writes a timestamped log bundle under
`~/.memory-harness/logs/` by default. The harness stops the child immediately
after spawn, lets the wrapper attach `perf`, and then continues it.

When the local `perf` supports `--control`, the wrapper now uses control FIFOs
instead of blind settle sleeps and signal-based shutdown:

- `perf record` starts disabled with `--delay=-1`
- the wrapper waits for `perf` to become ready
- the wrapper sends `enable` and waits for `ack`
- only then does it release the child
- when the run ends, the wrapper sends `stop` and waits for `ack` before
  collecting `perf report`

When `--perf-stat` is enabled, the wrapper now attaches `perf stat` to that
same child PID during the same run instead of executing the command again after
the profile is already finished.

For direct harness-only memory and resource accounting:

```bash
./target/release/memory-harness -- my-program arg1 arg2
```

For raw target-only CPU hotspot profiling:

```bash
perf record --call-graph dwarf ./target/release/my-program arg1 arg2
perf report
```

The design target is that the harness stays quiet enough that this concurrent
workflow remains useful. The wrapper now improves that by attaching `perf`
directly to the paused child PID instead of profiling the harness parent path.
Direct `perf` on the target remains the absolute lowest-noise option, but this
repo exists to push the concurrent path as far toward low-noise usefulness as
practical.

For direct harness-only runs, `--child-timeout-seconds N` is still the right
way to bound the child.

For the concurrent wrapper workflow, `--timeout-seconds N` is owned by the
wrapper instead. That is deliberate: the wrapper needs a chance to ask `perf`
to stop before it terminates the child. The harness receives a timeout marker
file so the JSON result still reports `timed_out=true` for the same run.

If you want the synchronized one-run bundle, use the wrapper. If you want the
cleanest target-only CPU profile, run `perf` on the target command directly.

## Wrapper for both passes

The repo includes a convenience wrapper:

```bash
scripts/run-with-memory-and-perf.sh -- my-program arg1 arg2
```

By default it now runs `perf` at the same time as `memory-harness`, but it does
so by attaching `perf` to the announced child PID instead of wrapping the
harness process itself.

It can also:

- repeat the whole workflow multiple times with `--repeat N`
- capture `perf stat` counters with `--perf-stat`
- bound every measured pass with wrapper-owned `--timeout-seconds N`
- store proc-status samples from the memory-harness pass
- write timestamped output under `~/.memory-harness/logs/`

The wrapper writes side-by-side artifacts into a timestamped directory:

- `combined.log`
- `memory-harness.json`
- `proc-status.tsv`
- `perf.data`
- `perf.report.txt`
- `perf.stat.txt` when `--perf-stat` is enabled
- `summary.txt`
- `command.txt`

Optional output directory override:

```bash
scripts/run-with-memory-and-perf.sh --out-dir tmp/run-1 -- my-program arg1 arg2
```

Optional environment override for the harness binary:

```bash
MEMORY_HARNESS_BIN=target/ship/memory-harness \
scripts/run-with-memory-and-perf.sh -- my-program arg1 arg2
```

Fixed-length concurrent sample:

```bash
scripts/run-with-memory-and-perf.sh \
  --timeout-seconds 15 \
  -- my-program arg1 arg2
```

Optional `perf` call-graph mode override:

```bash
MH_PERF_CALL_GRAPH=dwarf \
scripts/run-with-memory-and-perf.sh -- my-program arg1 arg2
```

**Note on call-graph modes:** The wrapper default for `MH_PERF_CALL_GRAPH` is
`dwarf` (set by the script). `dwarf` usually gives better stack quality, but
`fp` can be more robust when processes terminate abruptly (especially under
timeout), leaving `perf.data` complete and readable more often.

Repeat the concurrent workflow twice and collect `perf stat` too:

```bash
scripts/run-with-memory-and-perf.sh \
  --repeat 2 \
  --perf-stat \
  --out-dir ~/.memory-harness/logs/demo \
  -- cargo run --example spiky-alloc -- 8 512 20
```

## Verify the binary carries debug sections

After a release build, inspect the ELF sections:

```bash
readelf -S target/release/memory-harness | rg '\.debug_|\.symtab|\.eh_frame'
```

You should see sections such as:

- `.debug_info`
- `.debug_line`
- `.debug_abbrev`
- `.debug_str`

## Current boundaries

- Linux only
- best-effort cgroup v2 aggregate memory (child + descendants)
- no child-tree sampling daemon
- no flamegraph generation
- no direct Valgrind or Heaptrack integration
- wrapper repeat support is intentionally shell-level, not built into the Rust binary

That is deliberate. The current design optimizes for clarity, accurate peak RSS
reporting, low implementation overhead, and clean separation between resource
measurement and CPU profiling orchestration.
