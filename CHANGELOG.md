# Changelog

All notable changes to this project are documented in this file. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- Minimal Rust library skeleton: a single non-publishable crate, `tuishou-ting`,
  with `VERSION`, a unit test, and a committed `Cargo.lock`.
- Repository policy: `deny.toml`, `SECURITY.md`, Dependabot configuration, and
  a workflow pin validator.
- CI, Audit, CodeQL, Scorecard, and dependency-review workflows, plus a release
  workflow that archives the source, generates an SBOM, and attests the
  artifacts.

