# Changelog

## [Unreleased]

_No unreleased changes._

## [0.3.3] - 2026-08-21

### Changed

- Remove packaged-Skill guidance that required concurrent Agents to use
  separate Git worktrees; checkout coordination remains with the user or
  controlling Agent.

## [0.3.2] - 2026-08-08

### Fixed

- Validate managed connections against their real `codex-tamer/0.146.0`
  initialize identity, and prevent VS Code's inherited
  `CODEX_INTERNAL_ORIGINATOR_OVERRIDE` from rebranding that listener as the
  incompatible `codex_vscode` product.
- Put install steps and the Node.js 20+ prerequisite directly in public Release
  notes, and include the versioned README in every platform archive.
- Keep the public README install example synchronized with the version selected
  by the release script.

## [0.3.1] - 2026-08-08

### Changed

- Require an exact-SHA five-platform pre-release workflow before the release
  script creates an annotated tag, and reject release remotes with multiple
  fetch or push URLs.

### Fixed

- Build Linux artifacts in a pinned Amazon Linux 2023 environment so the
  documented glibc 2.34 and OpenSSL 3.0 ABI floor is real, and emit actionable
  diagnostics when a release binary exceeds it or lacks a required library or
  symbol version.
- Keep release checks portable across Windows LF checkout, executable-bit,
  drive-path, archive, and macOS Unix-socket path behaviors. Validate packaged
  manifests against the actual native target and binary name on every supported
  platform, and keep completion compatible with macOS Bash 3.2.
- Require managed app-server reuse to validate a same-UID Unix peer and the
  exact reviewed `codex_cli_rs/0.146.0` product. Reject unsafe
  `XDG_RUNTIME_DIR` permissions and overlong managed socket paths before
  connecting or starting a listener.

## [0.3.0] - 2026-08-08

### Breaking Changes

- Establish `codex-tamer` as an independent, headless hard fork of
  `kcosr/codex-threads` `0.2.4`. Rename the binary, package, configuration and
  state paths, environment variables, completion definitions, app-server client
  identity, release artifacts, and packaged Skill from `codex-threads` to
  `codex-tamer`.
- Remove the interactive TUI, TUI-only actions, syntax-highlighting features,
  screenshot asset, terminal dependencies, and PTY smoke tests. `codex-tamer`
  is an Agent-facing CLI only.

### Added

- Add a Unix-only default shared app-server per canonical `CODEX_HOME`, with a
  private stable runtime socket, concurrent startup locking, exact Codex 0.146.0
  identity validation, detached listener startup, and explicit
  `servers start`, `servers status`, and `servers stop` lifecycle commands.
- Add `[managed].codex`, `[managed].codex_home`, `--codex`, and `--codex-home`
  overrides for selecting the managed Codex runtime without conflating runtime
  identity with thread model defaults.
- Add independent `wait`, `result`, and `events follow` commands so another
  Agent process can reattach to a known thread/turn pair after a no-wait start.
- Add validated raw history injection through Codex 0.146
  `thread/inject_items`, accepting a JSON array from an argument, file, or
  stdin without starting a user turn.
- Add platform release bundles that package the prebuilt CLI and unchanged
  Agent Skill together, plus a cross-platform installer, manifest validation,
  SHA256 output, best-effort rollback that attempts both installed resources,
  and deterministic packaging tests.
- Add a five-platform tag workflow that validates binary identity, smoke-tests
  each extracted archive, verifies adjacent checksums, and publishes a complete
  GitHub Release with curated changelog notes and a combined `SHA256SUMS` file.

### Changed

- Allow every ordinary app-server JSON-RPC request up to 120 seconds without a
  response, and document updated-descending order for bounded `--since` thread
  discovery over large persisted session stores.
- Treat a missing implicit config as an empty config and automatically select
  the managed listener when no servers are configured. Preserve explicit
  endpoint and server-alias priority, and keep probes from starting a listener.
- Reframe product documentation and the packaged Skill around independent Agent
  control through JSON/NDJSON CLI commands.
- Harden release automation with a validated configurable remote, a complete
  locked preflight before tagging, Node.js 20 CI coverage, an enforced Linux
  glibc/OpenSSL ABI baseline, matching official fetch/push URLs, tag provenance
  checks against `main`, and draft-only GitHub Release updates.
- Declare the `codex-tamer` PATH dependency in the packaged Skill instead of
  writing machine-specific binary paths into Skill instructions.
- Preserve structured Codex turn errors in failed terminal events and aggregate
  results when the server supplies them.
- Cursor-page persisted turn lookup so `result` can find targets beyond the
  app-server's per-request turn cap while respecting `--max-turns`.
- Replay assistant content already persisted at attachment time before
  `events follow` switches to live or polled events.
- Bound pre-response notification buffers and release emitted NDJSON progress
  records and assistant text instead of retaining the complete stream in memory.
  Reconcile buffered replay in linear time and retain collision-resistant
  BLAKE3 fingerprints after streamed text is released.
- Index assistant item aliases and reject turns beyond 4,096 assistant items;
  cap retained progress at 10,000 events or 16 MiB with explicit errors.

### Fixed

- Update `anyhow` to `1.0.104` to address `RUSTSEC-2026-0190`, an unsound
  `Error::downcast_mut()` implementation in versions before `1.0.103`.
- Make `servers status` reserve successful `stopped` output for an absent
  listener and fail on incompatible, malformed, or insecure managed targets.
- Resolve `CODEX_HOME` through symlink-aware parent traversal, including
  dangling final symlinks, so one home cannot change endpoint identity after
  creation or through `symlink/..` paths.
- Bound both the managed readiness handshake and `codex --version` probe, and
  bound existing-listener startup probes. Keep the Windows WebSocket-only build
  warning-free under the release Clippy gate.
- Keep the fallback managed Unix socket below macOS path limits, allow explicit
  `--server managed` selection through a reserved alias, require same-UID peers,
  and reject reachable unverified listeners from `servers stop` even when no
  ownership record exists.
- Keep managed-process ownership stable across timezone and `PATH` changes,
  bind schema-versioned records to the system boot and verified Unix peer
  process group, preserve unverifiable legacy evidence, refuse to overwrite a
  live startup record, wait for the complete managed process group during
  shutdown, and derive one canonical endpoint before and after a missing
  `CODEX_HOME` is created.
- Prevent `steer` from resuming an unloaded persisted thread and presenting the
  result as control of an active turn owned by another app-server process.
- Clarify in the packaged Skill and README that VS Code stdio-only sessions
  cannot be attached after start, that `CODEX_ENDPOINT` is not an automatic
  target source, and how to establish and validate a listener for future
  trackable sessions without implicitly enabling Codex's standalone updater.
- Retry the fallback turn-history poll when Codex reports that a new thread is
  not materialized yet, while continuing to surface unrelated `-32600` errors.
- Keep fallback turn-history polling cancellable by command timeout and Ctrl-C,
  and print server/thread/turn diagnostics for locally interrupted waits.
- Reject persisted turns whose status is missing, non-string, or unknown instead
  of treating malformed app-server responses as valid state.
- Preserve the existing steer and interrupt acknowledgement shapes, validate
  their app-server success responses, reject non-progressing result pages and
  malformed cursor/inject/poll responses, and handle closed NDJSON output pipes
  without panicking.
- Apply attach timeouts and Ctrl-C handling to the initial resume request, use
  terminal state already present in resume snapshots, retain notifications
  received before an ignored materialization error, and suppress cross-ID
  snapshot replay duplicates.
- Reject installer flags supplied where a destination path is required, and
  continue restoring the binary when restoring the Skill fails during rollback.
- Reject colliding JSON-RPC server requests before matching client responses,
  fail closed on malformed response envelopes and mutation acknowledgements,
  and compile Windows as a WebSocket-only client.

The released entries below are the preserved upstream `codex-threads` history.
They are not `codex-tamer` releases, and their historical TUI references remain
intentionally unchanged.

## [0.2.4] - 2026-07-30

### Breaking Changes

- Replace `search QUERY` with the explicit `search threads QUERY` command
  shape. The removed form is not retained as an alias
  ([#12](https://github.com/kcosr/codex-threads/pull/12)).

### Added

- Add persisted Codex thread `pin` and `unpin` commands, `list --pinned` and
  `list --unpinned` filters, and pin state in human list output
  ([#12](https://github.com/kcosr/codex-threads/pull/12)).
- Add `CODEX_COMPATIBILITY.md` to record exact upstream Codex references,
  adopted app-server features, and reviewed follow-up candidates for each
  integration update
  ([#12](https://github.com/kcosr/codex-threads/pull/12)).

### Changed

- Reject send and steer operations when a thread explicitly reports
  `canAcceptDirectInput: false`, including when that state is learned after an
  automatic resume. Apply the same safeguard in the TUI composer
  ([#12](https://github.com/kcosr/codex-threads/pull/12)).

## [0.2.3] - 2026-07-19

### Added

- Add repeatable `list` and `tui` provider/source thread filters, and a confirmed TUI-only thread delete action
  ([#11](https://github.com/kcosr/codex-threads/pull/11)).
- Add per-server, opt-in `usage redeem`, which automatically redeems the best detailed rate-limit reset credit; use the same deterministic selection in the TUI
  ([#11](https://github.com/kcosr/codex-threads/pull/11)).

### Fixed

- Preserve a successful CLI reset redemption result when its follow-up usage refresh fails
  ([#11](https://github.com/kcosr/codex-threads/pull/11)).

## [0.2.2] - 2026-07-09

### Added

- Add `fork THREAD_ID` for creating Codex app-server thread forks, with
  `--last-turn` support for forking through a specific completed turn and
  explicit model, effort, and service-tier overrides when needed
  ([#10](https://github.com/kcosr/codex-threads/pull/10)).
- Add `list --parent THREAD_ID` and `list --ancestor THREAD_ID` filters for
  browsing spawned direct child threads or all spawned descendant threads
  ([#10](https://github.com/kcosr/codex-threads/pull/10)).
- Accept `max` and `ultra` as model reasoning effort suggestions, and pass
  through other non-empty app-server-supported effort values
  ([#10](https://github.com/kcosr/codex-threads/pull/10)).

## [0.2.1] - 2026-06-22

### Added

- Show Codex rate-limit reset-credit availability in `usage` output when
  provided by app-server.
- Add a TUI usage modal with reset-credit display and explicit confirmation
  before redeeming a banked Codex rate-limit reset.

## [0.2.0] - 2026-06-15

### Added

- Add `codex-threads tui`, an interactive terminal UI for browsing, viewing,
  searching, streaming, and controlling threads
  ([#6](https://github.com/kcosr/codex-threads/pull/6)).

## [0.1.5] - 2026-06-05

### Added

- Add local thread annotations with `annotate set/get/clear/list/search/prune`,
  endpoint-scoped JSON state, and annotation projection in `list`, `search`, and
  `show` output ([#5](https://github.com/kcosr/codex-threads/pull/5)).

## [0.1.4] - 2026-06-04

### Added

- Add endpoint-based server configuration for `unix://`, `ws://`, and
  `wss://` Codex app-server targets
  ([#4](https://github.com/kcosr/codex-threads/pull/4)).
- Add WebSocket-over-TCP app-server connections with optional bearer-token auth
  from `auth_token_env`, `auth_token`, `--connect-auth-token-env`, or
  `--connect-auth-token`
  ([#4](https://github.com/kcosr/codex-threads/pull/4)).

### Changed

- Normalize `servers` output around endpoint strings
  ([#4](https://github.com/kcosr/codex-threads/pull/4)).
- Deprecated legacy `type = "uds"` plus `path` server config; existing configs
  continue to work with a warning
  ([#4](https://github.com/kcosr/codex-threads/pull/4)).

### Fixed

- Keep `servers` listing from resolving auth token environment variables, and
  report unresolved auth for `servers ping --all` as a per-server failure
  ([#4](https://github.com/kcosr/codex-threads/pull/4)).
- Reject unknown config fields so misspelled auth keys do not silently drop
  credentials
  ([#4](https://github.com/kcosr/codex-threads/pull/4)).

## [0.1.3] - 2026-06-04

### Added

- Add shell completion setup and generated bash, zsh, and fish completion
  scripts for commands, options, static values, and configured server aliases
  ([#3](https://github.com/kcosr/codex-threads/pull/3)).

## [0.1.2] - 2026-06-03

### Added

- Add `usage` to show account plan, credits, and rate-limit windows from Codex
  app-server
  ([#2](https://github.com/kcosr/codex-threads/pull/2)).

### Fixed

- Improved release-script preflight checks, diagnostics, and changelog validation edge cases.

## [0.1.1] - 2026-06-03

### Added

- Add `status THREAD_ID --load` to explicitly resume/load a thread before
  reporting status
  ([#1](https://github.com/kcosr/codex-threads/pull/1)).
- Support top-level and per-server `model` and `model_reasoning_effort` config
  defaults for new threads
  ([#1](https://github.com/kcosr/codex-threads/pull/1)).

### Changed

- Include `CHANGELOG.md` and `skills/` in documented release archive contents
  ([#1](https://github.com/kcosr/codex-threads/pull/1)).

### Fixed

- Correct documented release upload tag to use the `vX.Y.Z` tag created by the
  release script
  ([#1](https://github.com/kcosr/codex-threads/pull/1)).
- Correct release and changelog documentation now that `0.1.0` has shipped
  ([#1](https://github.com/kcosr/codex-threads/pull/1)).
- Document live smoke goal checks in `smoke/README.md`
  ([#1](https://github.com/kcosr/codex-threads/pull/1)).

## [0.1.0] - 2026-06-01

### Added

- Initial `codex-threads` release.
