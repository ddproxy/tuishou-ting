# tuishou-ting

[![CI](https://github.com/ddproxy/tuishou-ting/actions/workflows/ci.yml/badge.svg)](https://github.com/ddproxy/tuishou-ting/actions/workflows/ci.yml)
[![CodeQL](https://github.com/ddproxy/tuishou-ting/actions/workflows/codeql.yml/badge.svg)](https://github.com/ddproxy/tuishou-ting/actions/workflows/codeql.yml)
[![OpenSSF Scorecard](https://api.securityscorecards.dev/projects/github.com/ddproxy/tuishou-ting/badge)](https://securityscorecards.dev/viewer/?uri=github.com/ddproxy/tuishou-ting)
[![Latest release](https://img.shields.io/github/v/release/ddproxy/tuishou-ting)](https://github.com/ddproxy/tuishou-ting/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Ting (the node agent), part of [Tuishou](https://github.com/ddproxy/tuishou).

Status: the agent code is being moved here as it gains its own release cadence. This repository currently contains the library skeleton and the workflows that guard it.

The library includes the [orphan harness watcher](docs/orphan-reaper.md): a configurable periodic sweep that terminates harnesses left behind by a previous agent run, with an exact owner-marker check and a fail-closed job-state check. It also provides [optional heartbeat health fields](docs/heartbeat-health.md) for harness liveness and workspace free space, and a [workspace garbage collector](docs/workspace-gc.md) that removes stale job build directories only when the workspace is low on space.

Checked-in text is guarded by a [repository hygiene check](docs/repo-hygiene.md) with a small allowlist for justified public URLs and the license notice.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```
