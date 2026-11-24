# Changelog

All notable changes to this project are documented in this file. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- Workspace garbage collector with a configurable, disableable sweep interval (default 15 minutes), a strict free-space threshold (default 15 percent), a two-hour grace period, anchored glob patterns, active-workspace protection, symlink refusal, canonical containment checks, newest-descendant grace checks, broad-root and checkout refusal, per-removal logging and `gc_removed_total` / `gc_bytes_freed_total` metrics. Documented in `docs/workspace-gc.md`.
- Optional heartbeat health fields (`harness_ok`, `disk_free_pct`, `disk_free_bytes`) with a bounded harness probe, cached sampling, additive JSON and node metrics gauges. Documented in `docs/heartbeat-health.md`.
- Orphan harness watcher with a configurable, disableable sweep interval (default 10 minutes), a termination grace period (default 15 minutes), exact owner-marker and job-state checks, per-action logging, and an `orphans_reaped_total` metric. Documented in `docs/orphan-reaper.md`.
- Minimal Rust library skeleton: a single non-publishable crate, `tuishou-ting`,
  with `VERSION`, a unit test, and a committed `Cargo.lock`.
- Repository policy: `deny.toml`, `SECURITY.md`, Dependabot configuration, and
  a workflow pin validator.
- CI, Audit, CodeQL, Scorecard, and dependency-review workflows, plus a release
  workflow that archives the source, generates an SBOM, and attests the
  artifacts.

