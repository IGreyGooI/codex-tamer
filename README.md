# codex-tamer

`codex-tamer` is a headless, Agent-facing CLI for inspecting and controlling
Codex app-server threads. It is a frontend to Codex, not another Agent runtime:
Codex app-server remains authoritative for threads, turns, history, models, and
execution.

This repository is an independent hard fork of
[`kcosr/codex-threads`](https://github.com/kcosr/codex-threads), based on its
`0.2.4` release. The fork intentionally removes the interactive TUI and focuses
on a CLI plus packaged Skill that other Agents can invoke with JSON or NDJSON
output.

```text
calling Agent
    |
    | argv + JSON/NDJSON
    v
codex-tamer
    |
    | Codex app-server JSON-RPC
    v
one named Unix-socket or WebSocket endpoint
    |
    +-- thread A / turn 1
    +-- thread B / turn 7
    +-- thread C / active turn
```

`codex-tamer` has no TUI or web UI. It does not choose orchestration policy,
schedule Agents, parse rollout files, or maintain a competing thread store.

## Capabilities

- Select one named Codex app-server endpoint per command.
- On Unix, reuse or start one private shared app-server for the selected
  `CODEX_HOME` when no endpoint is configured explicitly.
- List, search, inspect, and page Codex threads and turn history.
- Read flattened user and assistant messages.
- Create and fork threads.
- Start turns, wait for completion, stream assistant output, or return after
  acceptance.
- Reattach to an accepted turn with `wait`, inspect its persisted `result`, or
  follow normalized events as NDJSON.
- Inject validated raw Responses API items into model-visible thread history
  without starting a turn.
- Discover an active turn, steer it, or interrupt it.
- Read and change model, reasoning-effort, and service-tier settings where the
  connected app-server supports them.
- Name, pin, unpin, archive, and unarchive threads.
- Read models, account usage, rate limits, and Codex thread goals.
- Store endpoint-scoped local annotations for thread discovery.
- Connect over Unix domain sockets or `ws://` / `wss://`, with optional bearer
  authentication for WebSockets.

## Safety Model

By default, thread creation, resume-before-action recovery, and turn start
requests force `approvalPolicy = "never"` and full-access sandboxing. This
behavior is inherited from the upstream CLI and is called yolo mode.

Use global `--no-yolo` to preserve the connected app-server's configured
approval and sandbox defaults:

```bash
codex-tamer --no-yolo new --cwd "$PWD" "Inspect the repository" --json
```

The headless client does not conduct an interactive approval conversation. If
the app-server defaults require a server-initiated approval request, the
operation can fail; configure a suitable noninteractive policy at the trusted
endpoint instead of assuming the CLI will approve it.

Do not treat `codex-tamer` as a security boundary. A controlling Agent must
select an appropriate workspace, sandbox, credentials, and endpoint before
submitting work. Run concurrent write tasks in separate Git worktrees or other
isolated working directories.

## Compatibility

The inherited app-server integration was reviewed through Codex application
release `0.146`. See [`CODEX_COMPATIBILITY.md`](CODEX_COMPATIBILITY.md) for the
exact Codex tag and commit, the hard-fork baseline, and deferred APIs.

This is a reviewed baseline, not a compatibility shim. Codex experimental APIs
can change, and commands that rely on unsupported methods fail normally.
`codex-tamer` requests `capabilities.experimentalApi = true` during
`initialize`.

## Prerequisites

- The exact reviewed Codex CLI/runtime release, `0.146.0`, either in the
  selected `CODEX_HOME` standalone install, on `PATH`, or selected with
  `--codex PATH`.
- On Windows, an explicitly configured `ws://` or `wss://` app-server. Unix
  builds can manage a private local Unix listener automatically.
- Node.js 20 or newer to run the release-bundle installer.
- Linux release bundles require glibc 2.34 or newer and the OpenSSL 3 shared
  libraries (`libssl.so.3` and `libcrypto.so.3`); the release workflow checks
  this ABI baseline on both Linux architectures.
- Rust only when developing or building a release bundle.
- `jq` only for the optional projection examples below.

Codex app-server reads the selected `CODEX_HOME` state database and scans its
session JSONL when listing threads, repairing persisted metadata as needed.
Sharing a `CODEX_HOME` therefore makes persisted threads discoverable, but it
does not make a private stdio process remotely controllable.

## Install

Release archives contain a prebuilt platform binary, the Agent Skill,
`manifest.json`, and `install.mjs`. Recipients do not need Rust, GitHub
authentication, or the source checkout. Download the archive and adjacent
checksum from the public release:

```bash
VERSION=0.3.0
ASSET="codex-tamer-${VERSION}-linux-x86_64.tar.gz"
BASE="https://github.com/IGreyGooI/codex-tamer/releases/download/v${VERSION}"
curl --fail --location --remote-name "${BASE}/${ASSET}"
curl --fail --location --remote-name "${BASE}/${ASSET}.sha256"
sha256sum -c "${ASSET}.sha256"
tar -xzf "$ASSET"
cd "codex-tamer-${VERSION}-linux-x86_64"
node install.mjs --json
codex-tamer --version
```

The installer defaults to these per-user locations:

```text
~/.local/bin/codex-tamer
~/.agents/skills/codex-tamer/
```

On Windows, the binary defaults to
`%LOCALAPPDATA%\codex-tamer\bin\codex-tamer.exe`; the Skill remains under
`%USERPROFILE%\.agents\skills\codex-tamer`. Use `--bin-dir` or `--skills-dir`
to override either location. Windows supports `ws://` and `wss://` app-server
endpoints; Unix-socket endpoints are rejected with a platform error.

The installed Skill always invokes the stable command name `codex-tamer`.
Installation never rewrites `SKILL.md` with an absolute binary path. If the
selected binary directory is not on `PATH`, the installer reports the exact
directory to add; restart Codex or the calling Agent after changing `PATH`.

To build the current platform bundle as a maintainer:

```bash
cargo build --release
node scripts/package-release.mjs \
  --binary target/release/codex-tamer \
  --target linux-x86_64 \
  --out-dir dist
```

The packaging command creates the extracted bundle directory, a `.tar.gz` or
`.zip` archive, and an adjacent `.sha256` file. Supported target labels are
`linux-x86_64`, `linux-aarch64`, `macos-aarch64`, `macos-x86_64`, and
`windows-x86_64`.

Pushing a version tag runs the tag workflow in
`.github/workflows/release-assets.yml`. It builds and tests all five targets,
extracts and smoke-installs each packaged archive and unchanged Skill, verifies
every adjacent checksum, creates `SHA256SUMS`, and publishes the GitHub Release
with notes from the tagged `CHANGELOG.md` only after the full matrix succeeds.
The workflow can resume a draft Release, but refuses to replace assets on an
already-published Release.

The local release script runs Node test coverage plus the locked Rust test,
lint, and release-build preflight before it creates a tag. It defaults to the
`upstream` git remote; override only the remote name when needed. The selected
remote's push URL must still resolve to `IGreyGooI/codex-tamer`:

```bash
CODEX_TAMER_RELEASE_REMOTE=upstream node scripts/release.mjs patch
```

The local script creates and pushes the release commit and tag; it leaves
GitHub Release creation to the tag workflow.

## Shared App-Server

On Unix, an absent default config is valid. When no explicit or configured
server wins target selection, ordinary commands reuse or start one shared Codex
app-server for the canonical `CODEX_HOME`:

```bash
codex-tamer list --json
```

The endpoint is stable for that home:

```text
$XDG_RUNTIME_DIR/codex-tamer/<CODEX_HOME-hash>/app-server.sock
```

If `XDG_RUNTIME_DIR` is unset, the short UID-specific runtime root
`/tmp/codex-tamer-<UID>` is used so the final socket remains within macOS path
limits. Pass `--server managed` to select this synthetic target explicitly.
The alias `managed` is reserved and cannot be declared under `[servers]`. The
runtime directories and connected listener peer must belong to the current UID;
the directories use mode `0700`.
`codex-tamer` refuses an unsafe directory rather than placing the control socket
inside a permission-inaccurate WSL DrvFS `CODEX_HOME` or falling back to TCP.
It does not create a config file or run `codex app-server daemon bootstrap`.

Manage the inferred listener explicitly when needed:

```bash
codex-tamer servers start --json
codex-tamer servers status --json
codex-tamer servers stop --json
```

`servers ping` and `servers status` are probes and never start a stopped
listener. `servers status` reports `stopped` only when no listener accepts a
connection; an incompatible, malformed, or insecure managed target exits `3`
with diagnostics on stderr. `servers stop` only terminates a process group
whose schema-versioned record, system boot identity, launcher identity, home,
endpoint, and Unix peer credentials prove `codex-tamer` ownership; it refuses a
reachable external listener.

The managed home resolves in this order:

1. `--codex-home PATH`
2. `[managed].codex_home`
3. `CODEX_HOME`
4. `~/.codex`

The executable resolves independently:

1. `--codex PATH`
2. `[managed].codex`
3. `CODEX_HOME/packages/standalone/current/codex`
4. `codex` on `PATH`

The selected executable and the listener's initialize identity must both match
the exact reviewed Codex version and canonical home. No fallback occurs after
an explicit executable is rejected.

To use a separate listener, configure it or pass it directly:

```bash
CODEX_ENDPOINT=unix:///absolute/private/path/codex.sock
codex app-server --listen "$CODEX_ENDPOINT"
codex-tamer --connect "$CODEX_ENDPOINT" servers ping --json
codex --remote "$CODEX_ENDPOINT" --cd /absolute/worktree
```

`CODEX_ENDPOINT` above is only a shell variable shared by the commands;
`codex-tamer` does not read it automatically.

The official VS Code Codex 0.146 integration starts a private stdio app-server
and exposes no setting for redirecting it to this listener. `codex-tamer`
cannot attach to that process after start or inject into its active turn. The
shared app-server can still discover persisted threads from the same
`CODEX_HOME`, but only clients connected to the same explicit endpoint share
live loaded-thread state. Use `codex --remote` for future sessions that must be
live-controllable by `codex-tamer`.

## Configure Targets

The optional default config path is:

```text
~/.config/codex-tamer/config.toml
```

Configure managed startup overrides without defining a server:

```toml
[managed]
codex = "/absolute/path/to/codex"
codex_home = "/absolute/path/to/.codex"
```

Configure one external server to make it the implicit target instead:

```toml
model = "gpt-5.5"
model_reasoning_effort = "high"

[servers.main]
endpoint = "unix:///path/to/codex.sock"
```

Configure several independent endpoints when needed:

```toml
[servers.main]
endpoint = "unix:///path/to/main.sock"

[servers.work]
endpoint = "ws://127.0.0.1:8765"
model = "gpt-5.5"
model_reasoning_effort = "low"
auth_token_env = "CODEX_APP_SERVER_TOKEN"
```

See [`config.example.toml`](config.example.toml) for the complete schema.

Target resolution is deterministic:

1. Global `--connect ENDPOINT` selects that endpoint directly.
2. Command `--server ALIAS` selects a configured server.
3. `CODEX_TAMER_SERVER` selects a configured server.
4. A config containing exactly one server selects it automatically.
5. With no configured servers, Unix uses the managed `CODEX_HOME` listener.
6. Several configured servers without a selection exit with code `2`.

`--connect` cannot be combined with `--server` or `CODEX_TAMER_SERVER`.
Commands do not aggregate independent server results; `servers ping --all` is
the explicit exception.

The config path resolves in this order:

1. Global `--config PATH`
2. `CODEX_TAMER_CONFIG`
3. `~/.config/codex-tamer/config.toml`

A missing path at step 3 behaves as an empty config. A missing path selected by
`--config` or `CODEX_TAMER_CONFIG` is an error. Top-level and per-server `model`
and `model_reasoning_effort` values are thread/turn defaults only; they do not
select, start, or identify the app-server.

## Agent Quickstart

Install or point the controlling Agent at the packaged Skill:

```text
skills/codex-tamer
```

Use JSON for discovery and exact IDs:

```bash
codex-tamer servers ping --json
codex-tamer list --since 24h --limit 20 --sort updated --desc --json
codex-tamer search threads "release process" --limit 10 --json
```

Pair `--since` with `--sort updated --desc`. Without that explicit order,
`codex-tamer` must scan every app-server page to avoid missing recent threads
in server-defined order; `--limit` caps returned matches rather than the amount
of persisted history scanned. This matters for large session stores and for
`CODEX_HOME` on WSL DrvFS.

Inspect one selected thread:

```bash
codex-tamer status THREAD_ID --json
codex-tamer messages THREAD_ID --last 8 --max-turns 50 --json
codex-tamer show THREAD_ID --last 10 --items full --json
```

Create independent work:

```bash
codex-tamer new --cwd /absolute/worktree/path \
  "Run the requested analysis" --no-wait --json
```

Keep the returned IDs, then wait from any controlling process:

```bash
codex-tamer wait THREAD_ID TURN_ID --json
```

Send a blocking follow-up and receive the final result:

```bash
codex-tamer send THREAD_ID "Continue and report the result" --json
```

Check before writing to an active thread. Steer when input belongs to the
current turn; use `send` when it should start a new turn:

```bash
status=$(codex-tamer status THREAD_ID --json)
turn_id=$(printf '%s\n' "$status" | jq -r '.activeTurnId // empty')

codex-tamer steer THREAD_ID "$turn_id" "Prioritize the failing test" --json
codex-tamer interrupt THREAD_ID "$turn_id" --json
```

Independent threads can be controlled by separate `codex-tamer` processes.
Avoid uncontrolled concurrent writers to the same thread or working tree.

## Turn Modes

`new` with a prompt and `send` wait for a terminal turn status by default, for
up to one hour.

```bash
# One final JSON object after completion.
codex-tamer send THREAD_ID "Return the result" --json

# Return immediately after turn/start is accepted.
codex-tamer send THREAD_ID "Run in the background" --no-wait --json

# Emit accepted, progress, and terminal records as NDJSON.
codex-tamer send THREAD_ID "Stream the result" --json --stream
```

`--no-wait` returns `server`, `threadId`, and `turnId`. Retain both IDs. A later
controller can independently wait for terminal state, fetch the current
persisted result, or follow normalized events:

```bash
codex-tamer wait THREAD_ID TURN_ID --timeout 3600 --json
codex-tamer result THREAD_ID TURN_ID --json
codex-tamer events follow THREAD_ID TURN_ID --timeout 3600
```

`wait` returns the same aggregate terminal shape as blocking `send`. `result`
does not resume or subscribe to the thread; it cursor-pages through up to the
newest 200 turns by default and can include an in-progress result. Increase
`--max-turns` when the target is older. `events follow` writes NDJSON beginning
with an `attached` record, replays persisted assistant content as
`assistantMessage` records with `source = "snapshot"`, and then follows live or
polled events through the terminal record. A local timeout or Ctrl-C does not
imply that the remote turn stopped. Ctrl-C reports the selected server, thread,
and turn on stderr before exiting `130`; a timeout reports an app-server error
and exits `3`.

Failed terminal results preserve Codex's structured `error` object when the
server supplies one. Always use both the JSON `status` and the process exit
code; do not reduce failures to assistant text alone.

Use `inject` only when raw model-visible history is intentional. It calls
Codex `thread/inject_items` and does not start a user turn:

```bash
codex-tamer inject THREAD_ID --items-file items.json --json
printf '%s\n' '[{"type":"message","role":"user","content":[]}]' \
  | codex-tamer inject THREAD_ID --items-file - --json
```

The input must be a non-empty JSON array of objects and is limited to 16 MiB.
Use `send` or `steer` for ordinary instructions; injected items alter history
semantics.

Use `steer` only with the current `activeTurnId`:

```bash
codex-tamer steer THREAD_ID TURN_ID "Additional instruction" --json
```

Interrupt explicitly:

```bash
codex-tamer interrupt THREAD_ID TURN_ID --json
```

`send` and `settings set` may retry once after the exact unloaded-thread error
by resuming persisted state on the selected endpoint. Do this only when another
runtime is not actively writing the same thread. `steer` never resumes: it only
targets an active turn already loaded in the selected app-server and therefore
cannot masquerade as control of a private VS Code stdio process. `send` and
`steer` reject an explicit `canAcceptDirectInput: false` before submitting
input.

## Machine Output

`--json` writes one command-specific JSON object to stdout for successful read,
acknowledgement, no-wait, and blocking commands. Diagnostics and warnings go to
stderr. `--json --stream` writes compact NDJSON records to stdout.

Important output shapes include:

| Command | JSON shape |
| --- | --- |
| `list` | `{ server, threads, nextCursor, backwardsCursor }` |
| `search threads` | `{ server, results, nextCursor, backwardsCursor }` |
| `show` | `{ server, thread, turns }` |
| `messages` | `{ server, threadId, messages, nextCursor, truncated }` |
| `status` | `{ server, reachable, loadedThreadIds, nextCursor }` |
| `status THREAD_ID` | `{ server, threadId, thread, activeTurnId, truncated }` |
| blocking `new` / `send` | `{ server, threadId, turnId, status, progress, assistantResponses, finalAssistantText }` |
| no-wait `new` / `send` | `{ type, server, threadId, turnId, status }` |
| `wait` | `{ server, threadId, turnId, status, progress, assistantResponses, finalAssistantText }` |
| `result` | `{ server, threadId, turnId, status, assistantResponses, finalAssistantText, turn }` |
| `events follow` | NDJSON `attached`, snapshot/live assistant, progress, and terminal records |
| `inject` | `{ server, threadId, status, itemCount }` |
| `servers start/status/stop` | `{ server, status, backend, endpoint, codexHome, running, ... }` |

The CLI does not currently expose a versioned JSON-RPC envelope to its caller;
the table above is a command-specific CLI contract. Nested thread and turn
objects retain fields supplied by the connected Codex version.

Stream records use `type` values such as `accepted`, `progress`,
`assistantMessage`, `completed`, `failed`, and `interrupted`. Each emitted turn
record includes `server`, `threadId`, and `turnId`; assistant records include an
`itemId` when Codex supplies one.

Exit codes:

| Code | Meaning |
| --- | --- |
| `0` | Command succeeded, or a blocking turn completed. |
| `1` | A blocking turn reached `failed` or `interrupted`. |
| `2` | Usage, argument, validation, configuration, or local lookup error. |
| `3` | App-server, connection, socket, WebSocket, capability, or timeout error. |
| `130` | Local Ctrl-C while waiting; the remote turn may still be running. |

On codes `2`, `3`, and `130`, callers must inspect stderr. A streaming command
can have already emitted valid NDJSON before a later transport or protocol
error, so also check the process exit code.

Ordinary JSON-RPC requests wait up to 120 seconds without receiving an
app-server message before exiting with code `3`. Turn-level `--timeout` options
remain separate operation deadlines.

## Command Reference

| Command | Purpose |
| --- | --- |
| `servers` | List configured endpoint aliases, or the inferred managed target. |
| `servers ping` | Probe one target, or all configured targets with `--all`, without starting it. |
| `servers start` | Start or reuse the inferred `CODEX_HOME` managed listener. |
| `servers status` | Probe managed listener identity and ownership without starting it. |
| `servers stop` | Stop only the managed listener process started by `codex-tamer`. |
| `list` | List threads with cursor, time, cwd, pin, provider, source, and ancestry filters. |
| `search threads QUERY` | Search thread metadata and previews. |
| `show THREAD_ID` | Read thread metadata plus paged turns. |
| `messages THREAD_ID` | Flatten a bounded recent turn scan into user/assistant messages. |
| `new --cwd PATH [PROMPT]` | Create a thread and optionally its first turn. |
| `fork THREAD_ID` | Fork through the full thread or `--last-turn TURN_ID`. |
| `send THREAD_ID PROMPT` | Start a follow-up turn. |
| `status [THREAD_ID]` | Inspect loaded threads or one thread and its active turn. |
| `steer THREAD_ID TURN_ID PROMPT` | Add input to the expected active turn. |
| `interrupt THREAD_ID TURN_ID` | Request interruption of an active turn. |
| `settings show THREAD_ID` | Read effective cwd/model/effort/service tier. |
| `settings set THREAD_ID` | Update model, effort, or service tier. |
| `name THREAD_ID NAME` | Set the persisted thread name. |
| `pin` / `unpin THREAD_ID` | Change persisted Codex pin state. |
| `archive` / `unarchive THREAD_ID` | Change persisted archive state. |
| `models` | List models exposed by app-server. |
| `usage` | Read plan, credits, and rate limits. |
| `usage redeem` | Consume one reset credit when enabled per server. |
| `goal get/set/clear THREAD_ID` | Manage Codex-owned thread goal state. |
| `annotate set/get/clear/list/search/prune` | Manage endpoint-scoped local notes. |
| `completion` | Print shell completion instructions or scripts. |

Run `codex-tamer COMMAND --help` for all flags and value constraints.

## History and Pagination

`list`, `search threads`, and `show` use opaque server cursors. Pass returned
cursor strings back exactly; do not interpret them as offsets or timestamps.

`messages --max-turns M` first scans the most recent `M` turns, then flattens
messages, applies `--since` and `--role`, and finally applies `--last N`.
`messages` has no `--first`; use `show --asc` and cursors for older history.

```bash
page=$(codex-tamer show THREAD_ID --last 20 --items full --json)
cursor=$(printf '%s\n' "$page" | jq -r '.turns.nextCursor // empty')
codex-tamer show THREAD_ID --cursor "$cursor" --items full --json
```

## Local Annotations

Annotations are local `codex-tamer` state, not Codex app-server state. Their
location resolves as:

1. `$CODEX_TAMER_STATE/annotations.json`
2. `$XDG_STATE_HOME/codex-tamer/annotations.json`
3. `~/.local/state/codex-tamer/annotations.json`

They are keyed by the selected endpoint and thread ID. `list`, `search
threads`, and `show` project an annotation onto a thread when one exists.

```bash
codex-tamer annotate set THREAD_ID "Waiting for review" --json
codex-tamer annotate search "review" --json
codex-tamer annotate prune --dry-run --json
```

Use `annotate prune` deliberately: without `--dry-run`, it removes local
annotations whose threads app-server reports missing.

## Completion

```bash
codex-tamer completion
source <(codex-tamer completion script bash)
source <(codex-tamer completion script zsh)
codex-tamer completion script fish | source
```

## Development and Verification

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features
cargo build --release
node --test scripts/*.test.mjs
```

Deterministic integration tests use mock Unix-socket and WebSocket app-servers.
Opt-in live checks are documented in [`smoke/README.md`](smoke/README.md).

## Project Structure

```text
src/            Rust CLI, app-server client, normalization, and local state
tests/          Deterministic CLI integration tests
smoke/          Opt-in live app-server checks
skills/         Agent guidance for invoking codex-tamer
scripts/        Installer, bundle packaging, and release utilities
```

## Provenance and License

`codex-tamer` retains the MIT license of `codex-threads`. Released upstream
history is preserved in [`CHANGELOG.md`](CHANGELOG.md); new hard-fork changes
are recorded only under `Unreleased` until the first independent release.
