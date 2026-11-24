# Orphan harness watcher

The node agent starts harness work as containers or child processes. If the
agent restarts, or a job is cancelled while its harness is still starting, the
harness can outlive the job and keep consuming disk and IO on the node. The
`orphan` module in `tuishou-ting` provides a periodic sweep that finds and
terminates those leftovers.

## Settings

`OrphanReaperConfig::new(agent_id)` uses these defaults:

| Setting | Default | Behaviour |
| --- | --- | --- |
| `sweep_interval` | 10 minutes | Delay between sweeps. `Some(Duration)` enables it; `None` disables the watcher entirely. |
| `grace_period` | 15 minutes | A harness must be at least this old before termination. The boundary is inclusive. |
| `agent_id` | required | Identifies this agent in the owner marker. Only ASCII letters, digits, `-`, `_` and `.` are accepted, up to 128 characters. |

The interval must be non-zero when enabled. A zero grace period is allowed and
means "reap on the first sweep after detection". A disabled watcher neither
sleeps nor lists, and a direct `sweep_once()` call is a no-op as well.

## Owner marker

The agent must put the exact string `tuishou-ting:<agent_id>` on every harness
it starts, as a container label or a process marker. The lister reports that
string in `HarnessHandle::owner_marker`. The reaper compares the whole string
for equality: a missing, partial or differently prefixed marker is skipped,
and so is a marker for a different agent. The marker is provenance, not
authentication; use a distinct `agent_id` per agent instance and keep it
stable across restarts so previous harnesses remain recognisable.

## Wiring the agent

The module does no process or container I/O itself. Supply these adapters:

- `HarnessLister` returns a snapshot of visible harnesses: `id`, `job_id`,
  `owner_marker`, `started_at` and `kind` (`Container` or `Process`).
- `JobState` reports the phase of a job. Running and draining jobs must be
  reported as such; an absent job is `Absent`. If state cannot be read, return
  `Unknown` so the harness is left alone. `ActiveJobs` is a ready-made
  in-memory implementation.
- `HarnessTerminator` stops one harness, including descendants it owns.
- `Clock`, `Sleeper` and `Logger` have real defaults (`SystemClock`,
  `ThreadSleeper`, `StderrLogger`) and can be replaced for tests.
- `Arc<NodeMetrics>` is shared with the metrics endpoint.

Create the watcher with `OrphanWatcher::new(...)`, run `run(&shutdown)` on a
dedicated thread with an `Arc<AtomicBool>` shutdown flag, and set that flag to
stop after the current sleep. The first sleep happens before the first sweep so
job state has time to load after a restart. `sweep_once()` can also be called
directly for an on-demand sweep.

## Sweep decisions

For each harness returned by the lister, in order:

1. Missing or malformed `id`/`job_id`: skip. A `job_id` must be non-empty, at
   most 256 bytes, with no control characters. A container `id` must start with
   a letter or digit and use only letters, digits, `-`, `_` and `.`. A process
   `id` must be a decimal pid greater than 1. A terminator that shells out must
   still use argv, never a shell string.
2. `owner_marker` that is not exactly this agent's marker: skip.
3. Job phase `Running`, `Draining` or `Unknown`: skip.
4. Start time in the future, or age below the grace period: skip.
5. Otherwise terminate. A successful termination logs an info line with the
   kind, id, job and age, and increments `orphans_reaped_total`. A failed
   termination logs an error and is reported in the sweep report but does not
   increment the success counter.

A lister error aborts the sweep before any harness is touched. Shutdown is
checked before each sleep and again before each sweep. Every enabled sweep
also emits a debug-level summary of its counts. Job state should be read fresh
at sweep time; if the agent can re-check ownership inside its terminator, it
should do so to narrow the window in which a job can start after the phase
check.

## Metrics

`NodeMetrics::render()` produces Prometheus text exposition:

```text
# HELP orphans_reaped_total Harnesses terminated by the orphan watcher.
# TYPE orphans_reaped_total counter
orphans_reaped_total 0
```

The node metrics endpoint should serve this text alongside its other counters.
The same renderer also exports the optional
[heartbeat health gauges](heartbeat-health.md#metrics) when health samples are
available, and the [workspace collector counters](workspace-gc.md#metrics).
