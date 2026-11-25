# Notes

## Code-versus-brief discrepancies

### Orphan watcher (earlier change)

The brief describes the watcher as a change to an existing node agent. This
repository does not contain that agent; verification against the tree found:

- `src/lib.rs` was the only source file before this change. It contained
  `VERSION` and one test, with no dependencies in `Cargo.toml`.
- There is no job state, harness adapter, container/process lister, config
  parser, logger or node metrics endpoint in the tree to modify.
- There is no `docs/` directory before this change, and the README documents
  only `cargo fmt`, `cargo clippy` and `cargo test`.

Following the code, the watcher was added as a self-contained, dependency-free
`orphan` module. Listing, job state, termination, timing and logging are
injected through traits so the safety rules are testable with fakes; the agent
must supply the real adapters and share `Arc<NodeMetrics>` with its metrics
endpoint. `NodeMetrics::render()` provides the counter text but does not open a
listener, because no HTTP server exists in this crate. `ActiveJobs` is provided
for callers that keep job state in memory.

The brief mentions a documented mock limitation as the only accepted check
skip. No such limitation is documented in this repository (README, CHANGELOG,
SECURITY, workflows, scripts, deny policy or sources were checked). Nothing was
skipped.

### Heartbeat health (this change)

The brief says to extend the heartbeat and points at `docs/compatibility.md`.
Verification against the tree found:

- No heartbeat type, heartbeat payload builder, controller wire schema, HTTP
  server or local harness endpoint address exists in `src/` or `docs/`.
- No workspace path configuration or filesystem-health code exists.
- No compatibility document is mirrored here; the only local document before
  this change was `docs/orphan-reaper.md`, and a filesystem search found no
  compatibility document.

Following the code, a self-contained `health` module was added:

- `HeartbeatHealth::to_json()` is the additive payload fragment. No existing
  heartbeat key is present to change, so the agent merges these keys into the
  payload it already sends. Unknown fields are omitted, matching the rule that
  older controllers ignore keys they do not recognise.
- The agent supplies both external inputs the tree cannot provide: the harness
  listener `SocketAddr` and the workspace path. The probe sends `GET /` and
  treats any HTTP reply as an answer; that path is a documented choice, not a
  value found in code.
- `NodeMetrics::render()` already served as the metrics endpoint text, so the
  three gauges were added there. No listener was introduced.
- The crate forbids unsafe code and had no dependency exposing free-space
  statistics. `rustix` (safe `statvfs` wrapper, `fs` and `std` features) was
  added rather than libc FFI or spawning `df`; it costs one syscall per fresh
  sample. The `Statvfs` and `HarnessProbe` traits keep the unit tests free of
  real filesystem state and live servers.

### Workspace GC (this change)

The brief describes a "config section", active job workspaces and an agent
that owns per-job target directories. Verification against the tree found:

- There is still no agent binary, config parser, job workspace registry or
  metrics listener. Sources are `lib.rs`, `health.rs` and `orphan.rs` plus this
  change; `GcConfig` is therefore the configuration section passed to the
  collector by the agent, and `NodeMetrics::render()` remains the metrics text.
- No code or document names the agent's per-job target directories. Following
  the observed layout in the brief, the workspace defaults are `target-*`
  (directly under the workspace root) and `*/target` (inside a per-job
  workspace); tmp-root defaults are the two globs stated in the brief. All four
  are replaceable through `GcConfig::with_patterns`.
- File modification times are `SystemTime`, while the existing `Clock` trait
  supplies `Instant`. A separate `MtimeClock` trait was added rather than
  changing the heartbeat or orphan clock contracts.
- `Statvfs`, `DiskStats::free_pct()`, `Sleeper`, `Logger` and `NodeMetrics` were
  reused after verification; only the new counters were added to the renderer.
- `SystemDirRemover` rechecks `symlink_metadata` and canonical containment in
  the configured root before calling `std::fs::remove_dir_all`. That standard-library call was also checked on the
  development toolchain with a temporary scratch tree: a symlink inside a
  removed directory did not cause its target to be removed.

Following the code, `src/gc.rs` is a self-contained module with injected
filesystem lister, remover, active-workspace state, clock, sleeper and logger.
The real adapters are deliberately narrow: listings and patterns resolve one
path component at a time, symlinks are typed and refused, and roots are
validated as absolute normalized paths. A listing error, unknown free space or
unusable job state aborts the sweep before any removal.

## Repository state

The `.git` directory is owned by a different account and is not readable by
the session user, so git commands fail with "not a git repository" and no
commit could be created. The intended terse commit messages are:

    add orphan harness watcher
    add heartbeat health fields
    add workspace garbage collector

No hook, workflow or policy file was changed. The orphan watcher added no
dependencies; this change adds `rustix` and its `Cargo.lock` entries, regenerated
by cargo. No dead-code allowances or suppressions were used, and no
secret-shaped text was written.

### Public repository hygiene (this change)

The brief asks for neutral names in checked-in files. Verification against the
tree found:

- The only tool-named fixture directory was a `target`-style test path in
  `src/gc.rs`; it now uses a neutral application word. No stored path, wire key,
  metric name or owner marker is derived from it.
- References to the earlier source repository in `Cargo.toml`, `README.md`,
  `docs/heartbeat-health.md` and this file were prose only. They now describe
  the source neutrally; no code or configuration uses that repository name.
- A short tool label in this file was prose only and is now generic.
- No internal hostname, internal domain or email address exists in the tree.
  Every URL found is a public project, badge or registry address.

Kept deliberately, with the reason recorded in `repo-hygiene-allowlist.txt`:

- Canonical public repository, homepage, badge, security-reporting and registry
  URLs keep their existing host and owner segments. They are public project
  addresses, and changing them would break links and registry resolution.
- The `LICENSE` copyright notice keeps the rights holder's attribution. It is a
  legal notice, not a stored or wire value.

No wire key, metric name, marker prefix or stored path was renamed, so there is
no legacy spelling on the wire to preserve. The hygiene test scans text files
in a checkout (excluding generated directories) and reports unapproved target
fixtures, repository slugs, hosts, email addresses, copyright holders and
earlier-repository wording.

The intended terse commit message for this change is:

    keep repository names neutral
