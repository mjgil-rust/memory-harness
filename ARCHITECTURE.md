# Memory Harness Architecture

## Goal

`memory-harness` is a small Linux-only Rust CLI for running one child process
and reporting resource usage that is hard to recover accurately from shell
wrappers alone.

The primary output is the child process high-water RSS value, along with wall
clock time, CPU time, page fault counts, and exit status.

The repo-level goal is more specific than that: this project exists to create a
memory harness with low enough wrapper noise that `perf` can run concurrently
and still produce a useful CPU profile for the wrapped program.

## Non-goals

- It is not a benchmark framework.
- It does not aggregate multiple runs.
- It does not attempt heap attribution like Valgrind DHAT or Heaptrack.
- It does not replace `perf` for CPU profiling.

## Noise Budget

The harness is intentionally designed around a wrapper-noise budget.

That means:

- prefer kernel-provided accounting over heavy user-space polling
- keep sampling optional and lightweight
- keep orchestration shell-side when possible
- capture child output without turning the harness into a logging framework
- accept that some wrapper noise remains, but keep it small enough that
  concurrent `perf` remains practically useful

## Process Model

The harness spawns exactly one child command, waits for it with `wait4(2)`, and
formats the resulting `struct rusage` plus exit status into either human-readable
text or JSON.

The current CLI surface is intentionally small:

- `--format human|json`
- `--cwd <dir>`
- `--env KEY=VALUE` repeated as needed
- `--clear-env`
- `--child-timeout-seconds <seconds>`
- `--timeout-marker-file <path>`
- `--sample-proc-status-ms <ms>`
- `--sample-proc-status-out <path>`
- `--objects-processed <count>`
- `-- <program> [args...]`

There are two distinct resource-usage views:

- Child usage from `wait4(2)`: this is the measurement that matters for the
  wrapped command.
- Harness self usage from `getrusage(RUSAGE_SELF)`: this shows how much memory
  the wrapper itself consumed, so users can judge whether the harness overhead
  is negligible.
- Run aggregate memory from cgroup v2 when available: this captures RSS and file
  cache for all descendants in the command tree.

The optional throughput metadata path is also supported:

- `--objects-processed <count>` accepts a caller-provided total units processed.
- `objects_processed`, `objects_per_second`, and `seconds_per_object` are emitted
  in JSON as derived fields.
- If the option is omitted, those fields remain `null`.

## Cgroup Memory Accounting (v2)

The harness now attempts a best-effort run-level cgroup flow:

- Read the process cgroup from `/proc/self/cgroup` and use that as the cgroup v2 parent.
- Create a short-lived child cgroup for the run.
- Move the launched child process into it before execution begins.
- Read `memory.current`, `memory.peak`, and `memory.stat` from that cgroup at the end
  of the run.
- Remove the temporary cgroup directory afterwards.

This path exists to cover grandchild/worker processes and include cache-style memory
pressure (`file` and `shmem` contributions) in addition to the process-local `wait4`
metrics.

When the path cannot be attached or read, the harness includes a `child_cgroup_memory_status`
string explaining why cgroup accounting was not collected for that run.

When cgroup v2 accounting is not available or writable, this path is skipped and
`child_cgroup_memory` is omitted from output so the existing process-based metrics remain
authoritative.

## Why `getrusage`

The kernel maintains a peak resident-set high-water mark for the process. A
plain `/proc/<pid>/status` read only tells you a current point-in-time RSS and
can miss short spikes unless you poll aggressively.

`getrusage` matters because it exposes kernel-tracked resource accounting
directly, including `ru_maxrss`, without requiring intrusive sampling inside the
target program.

In this repo, `getrusage(RUSAGE_SELF)` is used for the harness process and
`wait4(2)` is used to obtain the child process `struct rusage` on exit. That
keeps the peak-RSS path authoritative.

The optional `/proc/<pid>/status` sampler exists for a different reason: it
provides a low-overhead time series for `VmRSS`, `VmHWM`, `RssAnon`,
`RssFile`, `RssShmem`, and thread count so a run can be explained over time
without giving up `ru_maxrss` as the primary measurement.

When cgroup accounting is active, the harness emits `child_cgroup_memory` with
`memory.current`, `memory.peak`, and breakdown fields (`anon`, `file`, `shmem`,
`cache`, `rss`, and `rss_huge`) for the entire cgroup (child + descendants).

## Perf Boundary

The default repo workflow is now a synchronized concurrent wrapper run:
`memory-harness` forks the target command, the child self-stops in `pre_exec`,
the parent waits until `/proc/<pid>/status` reports the stopped state,
announces the child PID, and the wrapper attaches `perf` directly to that child
before releasing it.

That gives one timestamped artifact bundle containing:

- the harness JSON summary
- proc-status samples
- `perf.data`
- text `perf report`
- optional attached `perf stat`
- a combined child-output and profiler log

That removes the harness parent wait/report path from the main sampled CPU
stream and is the biggest single improvement toward the repo's stated purpose.
Direct `perf` on the target command remains the lower-noise option when CPU
attribution purity matters more than synchronized logging.

When supported by the local `perf`, the wrapper uses `perf record --control`
with named FIFOs to make both startup and shutdown explicit:

- `perf record` starts disabled with `--delay=-1`
- the wrapper waits until `perf` is ready
- the wrapper sends `enable` and waits for `ack`
- only then does the harness continue the child
- when the run is over, the wrapper sends `stop` and waits for `ack`

When `--perf-stat` is enabled, the wrapper also attaches `perf stat` to the
same announced child PID for the same run instead of launching a second pass
after the profiled command completes.

This removes the fixed "settle sleep" from the main path and avoids relying on
signal delivery for normal `perf` finalization.

## Timeout Boundary

There are now two timeout modes, and they exist for different reasons.

Direct harness mode:

- `memory-harness --child-timeout-seconds N -- ...`
- the harness owns the deadline itself
- the timeout starts when the child is actually allowed to execute
- on timeout the harness kills the child, reports `timed_out=true`, and exits
  `124`

Concurrent wrapper mode:

- `scripts/run-with-memory-and-perf.sh --timeout-seconds N -- ...`
- the wrapper owns the deadline so it can ask `perf` to stop before terminating
  the child
- at the deadline the wrapper writes a timeout marker, asks the attached
  profilers to stop or disable collection, and only then terminates the child
- `memory-harness` sees the timeout marker via `--timeout-marker-file` and marks
  the JSON result as `timed_out=true` even though the wrapper owned the clock

That split exists because a pure harness-owned timeout kills the child before
the wrapper can coordinate `perf` shutdown, which reintroduces the original
attached-`perf` finalization problem.

## Call-Graph Mode Choice

The wrapper passes-through `MH_PERF_CALL_GRAPH` into `perf record`; the
script currently defaults to `dwarf`.

**Why to use `fp`:**

- Frame pointer unwinding handles abrupt termination (e.g., timeout) gracefully
- The wrapper's `--control` mechanism can stop `perf` cleanly before the child dies
- `perf.data` remains valid and readable even under timeout scenarios
- Works reliably with the wrapper-coordinated timeout boundary

**When to use `dwarf`:**

- DWARF provides more precise stack traces, especially for optimized code built
  with `-fomit-frame-pointer`
- Use `dwarf` when stack accuracy is more important than timeout shutdown
  resilience
- Use `dwarf` only if the workload is expected to run to natural completion or if
  occasional incomplete `perf.data` on abrupt termination is acceptable

**The limitation:**

DWARF call-graph unwinding can get stuck when the traced process dies abruptly,
even with the wrapper's control FIFO mechanism and extended stop timeout. The
unwinding process appears to require the tracee to remain alive for finalization.
Frame pointer mode does not have this limitation and handles shutdown reliably
under timeout conditions.
