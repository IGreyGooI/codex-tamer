# codex-tamer Agent Instructions

This repository contains `codex-tamer`, an independent Rust CLI frontend for
querying and controlling Codex app-server threads from other Agents.

## Product Boundary

`codex-tamer` is a headless, Agent-first hard fork of `kcosr/codex-threads`
`0.2.4`.

- Keep Codex app-server authoritative for thread state, history, settings,
  goals, turns, and execution.
- Keep the product a frontend. Do not add another model runtime, rollout parser,
  thread index, scheduler, or orchestration policy.
- Do not add a TUI, web UI, TUI assets, PTY tests, terminal rendering modules,
  or documentation that claims `codex-tamer` has an interactive UI.
- Prefer exact machine output and explicit IDs over presentation features.
- Preserve one explicit app-server target per command. Do not merge cursors or
  implicitly fan commands out across independent servers.

## Protocol Invariants

- Treat stdout as the machine-output channel for JSON and NDJSON modes.
- Send diagnostics and warnings to stderr.
- Preserve separate server, thread, turn, and item identifiers.
- Keep `turn/steer`, `turn/start`, and `turn/interrupt` semantically distinct.
- Validate app-server responses at the boundary before reporting success.
- Make protocol compatibility explicit and test it against the exact reviewed
  Codex release.
- Do not hide retries that change operation semantics.

The inherited CLI currently uses command-specific JSON shapes rather than a
versioned JSON-RPC envelope for its caller. Any public output redesign is a
breaking change and must update tests, README, Skill instructions, and the
Unreleased changelog together.

## Fast Bootstrap

1. Build: `cargo build`
2. Check formatting: `cargo fmt --check`
3. Test: `cargo test`
4. Lint: `cargo clippy --all-targets --all-features`
5. Release build: `cargo build --release`

## Development

- Use TDD for behavior changes and maintain at least 80% coverage for changed
  code.
- Run `cargo fmt --check`, `cargo test`, and
  `cargo clippy --all-targets --all-features` before handing off substantial
  changes.
- Run `cargo build --release` before release-oriented or packaging changes.
- Keep CLI entrypoints thin; put behavior behind focused library modules.
- Prefer typed structures over unvalidated `serde_json::Value` at public
  protocol boundaries.
- Prefer deterministic offline tests for config, target resolution, protocol
  parsing, rendering, streaming, and error mapping.
- Keep live Codex smoke tests opt-in and documented under `smoke/`.
- Validate user input and never hardcode credentials or tokens.
- Preserve unrelated work in a dirty worktree and coordinate file ownership
  when several Agents work concurrently.

## Documentation

- Update `README.md` for user-facing behavior, config, command, output, or
  workflow changes.
- Update `CHANGELOG.md` under `## [Unreleased]` for changes intended to ship.
- Update `CODEX_COMPATIBILITY.md` with the exact upstream Codex tag and commit
  whenever intentionally adopting app-server behavior from a new release.
- Keep `skills/codex-tamer/SKILL.md` concise and Agent-oriented.
- Regenerate `skills/codex-tamer/agents/openai.yaml` with the `skill-creator`
  generator when Skill metadata changes, then run `quick_validate.py`.
- Do not add auxiliary README or changelog files inside the Skill directory.

## Historical Records

Released `0.1.0` through `0.2.4` changelog entries belong to upstream
`codex-threads`. Preserve their product names, links, dates, and TUI facts
verbatim. They are provenance, not claims about current `codex-tamer` features.
Record hard-fork changes only under the current `Unreleased` section until an
independent release is made.

## Layout

- `src/bin/` contains the binary entrypoint.
- `src/lib.rs` is the shared library entrypoint.
- `config` owns TOML schema, defaults, validation, and target resolution.
- `rpc` owns Unix-socket/WebSocket transport, JSON-RPC correlation, and the
  app-server handshake.
- `cli` owns command-line parsing.
- `app` owns command orchestration, event normalization, and rendering.
- `session` and `turns` own app-server thread/turn behavior.
- `tests/` contains deterministic integration coverage.
- `smoke/` contains the opt-in live smoke harness.
- `skills/codex-tamer/` contains packaged Agent guidance.

## Changelog Format

Use only the needed subsections under `## [Unreleased]`:

- `### Breaking Changes`
- `### Added`
- `### Changed`
- `### Fixed`
- `### Removed`

Append to an existing subsection instead of duplicating it. Never edit released
upstream entries. Use inline pull-request links only when a pull request exists.

## Releasing

`codex-tamer` has not yet made an independent release. Before the first release:

1. Verify the hard-fork version and tag policy.
2. Verify the release repository and archive names in the release scripts.
3. Run the full local verification suite and opt-in live smoke when available.
4. Stamp only the current `Unreleased` changes as a `codex-tamer` release.
5. Keep the inherited `codex-threads` release history unchanged.
