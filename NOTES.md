# Notes

## Code-versus-brief discrepancies

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

The brief mentions a documented D1 mock limitation as the only accepted check
skip. No such limitation is documented in this repository (README, CHANGELOG,
SECURITY, workflows, scripts, deny policy or sources were checked). Nothing was
skipped.

## Repository state

The `.git` directory is owned by a different account and is not readable by
the session user, so git commands fail with "not a git repository" and no
commit could be created. The intended terse commit message is:

    add orphan harness watcher

No hook, workflow or policy file was changed. No dependencies or lockfile
entries were added, and no dead-code allowances or suppressions were used.
