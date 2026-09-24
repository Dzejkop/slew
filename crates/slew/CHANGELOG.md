# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.1](https://github.com/Dzejkop/slew/compare/v0.2.0...v0.2.1) - 2026-09-24

### Added

- *(slew)* park native waits per coroutine

### Fixed

- *(tui)* validate wait tokens and unfreeze parked siblings
- *(slew)* keep the single-slot guards and parking consistent
- *(robot_fleet)* keep wait kind with the token across adoption
- keep Facing names lower-case and clarify error contracts

### Other

- trim verbose comments
- *(robot_fleet)* derive the park condition from the wait kind
- *(robot_fleet)* block on native waits instead of polling
- typed errors (thiserror), strum enum mappings, color-eyre reports
- drive multi-case tests through test-case
- *(slew)* de-duplicate, fix latent bugs, and harden binary chunks

## [0.2.0](https://github.com/Dzejkop/slew/compare/v0.1.0...v0.2.0) - 2026-09-13

### Other

- hoist shared metadata, dependencies, and lints to the workspace
- move slew into crates/ under a cargo workspace
