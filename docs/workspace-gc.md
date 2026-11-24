# Workspace garbage collector

Cargo target trees from cancelled or crashed jobs can consume the node disk
long after their job is gone. The `gc` module in `tuishou-ting` adds a periodic,
threshold-gated sweep that removes only directories it can prove are stale,
unused, and shaped like job build output.

## Settings

`GcConfig::new(workspace_root, tmp_root)` validates both roots and returns these
defaults:

| Setting | Default | Behaviour |
| --- | --- | --- |
| `sweep_interval` | 15 minutes | Delay between sweeps. `Some(Duration)` enables it; `None` disables it. A zero interval is rejected. |
| `free_pct_threshold` | 15 | A sweep runs only when filesystem free space is strictly below this percentage. Valid range is `0..=100`. |
| `grace_period` | 2 hours | A directory must be unmodified for at least this long. The boundary is inclusive. |
| patterns | see below | Relative globs anchored at one of the two configured roots. |

Roots must be absolute, normalized UTF-8 paths. `/`, top-level system
directories (`/home`, `/var`, `/tmp`, `/srv`, `/usr`, ...) and a home directory
(`/home/<user>`, `/Users/<user>`) are rejected. The collector never lists or
removes anything outside them. `with_pattern` adds a pattern; `with_patterns`
replaces the default set. An empty set is valid and deletes nothing.

Default patterns:

| Root | Glob | Purpose |
| --- | --- | --- |
| workspace | `target-*` | Agent-managed per-job target directories named directly under the workspace root. |
| workspace | `*/target` | A target directory inside a per-job workspace directory. |
| tmp | `*-target` | Scratch target directories named after their job. |
| tmp | `target-*` | Scratch target directories with the job name last. |

A glob is `/`-separated, relative, and anchored at every segment. Within one
segment `*` matches any sequence of characters and `?` matches one character.
Path separators, `.`/`..`, control characters and the glob metacharacters
`[ ] { }` are rejected. Patterns cannot cross a `/` boundary, so a pattern can
never match a path at a different depth or outside its root.

No central config parser exists in this repository, so `GcConfig` is the
configuration section the agent supplies to `WorkspaceGc::new`.

## When a sweep runs

1. A disabled collector does nothing: `run` returns immediately and
   `sweep_once` does not sample, list or remove.
2. Free space is read from the workspace filesystem through the existing
   [`Statvfs`](heartbeat-health.md#workspace-disk) trait. If statistics are
   unavailable, the sweep is skipped rather than guessed.
3. If the free percentage is greater than or equal to `free_pct_threshold`, the
   sweep is skipped without listing any directory.
4. Active job workspaces are read next. `None`, an unreadable state, or any
   relative or non-normalized path aborts the sweep before any removal.

## Sweep decisions

Patterns are expanded segment by segment. At every segment the collector lists
one directory and considers only entries whose lister type is `Dir`. A symlink
whose name matches a pattern is counted in `skipped_symlink` and is never
listed or removed. Entries with malformed names are counted in
`skipped_invalid`. A failed listing aborts the whole sweep before any removal.

For each matching directory, in order:

1. Protected by an active workspace: skip. A candidate is protected when it is
   inside an active workspace path or contains one, so both workspace roots and
   target directories can be reported safely.
2. Contains the workspace root or the tmp root: skip.
3. The directory's own modification time is within `grace_period`: skip
   without descending.
4. The tree is measured: regular file sizes are summed, symlinks are never
   traversed, and the newest modification time of the directory and every
   descendant is tracked. A tree that cannot be measured is skipped; one that
   vanished is counted as `skipped_vanished`.
5. A top-level `.git` or `Cargo.toml` marks a checkout: skip.
6. Newest modification time in the future: skip as clock skew. Newest time
   younger than `grace_period`: skip. A directory is therefore kept while
   anything inside it, such as a running build, was written recently.
7. Otherwise remove it. `SystemDirRemover` checks `symlink_metadata` again,
   rejects symlinks and non-directories, rejects the root itself, canonicalizes
   the path and its root and requires the canonical path to equal the
   canonical root joined with the relative path (so a symlinked parent is
   refused), then calls `std::fs::remove_dir_all` on the canonical path. A
   directory that vanishes first is counted as `skipped_vanished`, not a
   failure. Roots should still not be writable by untrusted processes.
8. Each successful removal emits an info line containing the path, byte count
   and idle time. Failures emit an error line and are counted in the report but
   do not change the metrics.

Overlapping patterns are de-duplicated, so one directory is removed at most
once per sweep.

## Wiring the agent

```rust
use std::sync::Arc;
use tuishou_ting::gc::{
    ActiveJobWorkspaces, GcConfig, WorkspaceGc, SystemDirLister, SystemDirRemover,
};
use tuishou_ting::health::SystemStatvfs;
use tuishou_ting::orphan::NodeMetrics;

let metrics = Arc::new(NodeMetrics::default());
let gc = WorkspaceGc::new(
    GcConfig::new("/srv/work", "/srv/work/tmp")?,
    Box::new(SystemDirLister),
    Box::new(SystemDirRemover),
    Box::new(SystemStatvfs),
    Box::new(ActiveJobWorkspaces::new()),
    Arc::clone(&metrics),
)?;
gc.run(&shutdown);
```

The agent supplies its own `ActiveWorkspaces` if job state lives elsewhere;
running and draining jobs must both be reported. `ActiveJobWorkspaces` is an
in-memory implementation with `start`, `drain` and `finish`. `Clock`-style
injections use `MtimeClock` because file timestamps are `SystemTime`, not the
`Instant` used by the heartbeat and orphan watcher. `DirRemover` receives the
configured root with each path so an implementation can verify containment. `Sleeper` and `Logger` are
reused from the orphan module with real defaults and test overrides.

Most tests use a fake `DirLister`, clock, `Statvfs`, remover and active-workspace
state. A small `real_fs` test module creates directories under the system temp
dir to check symlink and containment handling in the system adapters.

## Metrics

`NodeMetrics::render()` always includes:

```text
# HELP gc_removed_total Directories removed by the workspace garbage collector.
# TYPE gc_removed_total counter
gc_removed_total 0
# HELP gc_bytes_freed_total Bytes in directories removed by the workspace garbage collector.
# TYPE gc_bytes_freed_total counter
gc_bytes_freed_total 0
```

`gc_removed_total` counts successful removals and `gc_bytes_freed_total` counts
the bytes measured before each removal. Both are additive to the existing
orphan counters and health gauges.
