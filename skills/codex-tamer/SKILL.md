---
name: codex-tamer
description: Use the headless `codex-tamer` CLI as an Agent-facing frontend to Codex app-server. Invoke it to automatically reuse or start a shared Unix listener for CODEX_HOME, connect to explicit Unix-socket or WebSocket endpoints, handle VS Code stdio attachment limits, discover and inspect Codex threads, read history and status, create or fork threads, send or steer messages, wait for or follow active turns, fetch results, inject raw history items, interrupt turns, and manage thread metadata through machine-readable JSON or NDJSON.
metadata:
  requires:
    bins: ["codex-tamer"]
  cliHelp: "codex-tamer --help"
---

# Codex Tamer

Use `codex-tamer` to observe and control Codex app-server threads. Treat Codex
app-server as the source of truth; do not parse rollout files or invent a local
session index.

Use the CLI as a headless machine interface. Do not look for or claim a
`codex-tamer` TUI.

## Establish the Target

Resolve one endpoint in this order:

1. `--connect ENDPOINT`
2. `--server ALIAS`
3. `CODEX_TAMER_SERVER`
4. The only server in `~/.config/codex-tamer/config.toml`
5. On Unix, the shared listener derived from the canonical `CODEX_HOME`

Resolve the target with `--server ALIAS` when several servers are configured.
Use `--connect ENDPOINT` only for explicit one-off targeting or debugging. Do
not combine `--connect` with `--server` or `CODEX_TAMER_SERVER`.

An absent default config is valid. Ordinary commands automatically reuse or
start the inferred Unix listener. Use lifecycle commands when setup or
diagnostics are the task:

```bash
codex-tamer servers start --json
codex-tamer servers status --json
codex-tamer servers ping --json
codex-tamer servers stop --json
```

`servers ping` and `servers status` only probe; they do not start a stopped
listener. Treat `status = "stopped"` as a confirmed absent listener; an
incompatible, malformed, or insecure target exits `3` instead. `servers stop`
only stops a process recorded as started by `codex-tamer` and must refuse a
reachable external listener.

The default socket is stable for one canonical home:

```text
$XDG_RUNTIME_DIR/codex-tamer/<24-char CODEX_HOME hash>/app-server.sock
```

When `XDG_RUNTIME_DIR` is unset, use
`/tmp/codex-tamer-<UID>/<24-char CODEX_HOME hash>/app-server.sock` to stay within
macOS Unix-socket path limits. Pass `--server managed` to select this synthetic
target explicitly. Treat `managed` as reserved; do not declare it under
`[servers]`.

Require the runtime directories and listener peer to belong to the current UID;
require directory mode `0700`. A configured `XDG_RUNTIME_DIR` must already be a
real current-user `0700` directory and short enough to keep the final socket at
or below the portable 103-byte path limit. Do not move the socket into a WSL
DrvFS `CODEX_HOME`, relax directory permissions, or fall back to TCP.
`codex-tamer` does not write a config file.

Resolve startup inputs in these orders:

- Home: `--codex-home` > `[managed].codex_home` > `CODEX_HOME` > `~/.codex`
- Binary: `--codex` > `[managed].codex` >
  `CODEX_HOME/packages/standalone/current/codex` > `codex` on `PATH`

Require exact reviewed Codex `0.146.0`. Validate both the selected binary and
the connected server's initialize `userAgent` as `codex-tamer/0.146.0` and its
`codexHome`. Remove an inherited `CODEX_INTERNAL_ORIGINATOR_OVERRIDE` when
starting the managed listener so a VS Code shell cannot rebrand it as
`codex_vscode`; never silently fall back after an explicit binary is rejected.
Do not run
`codex app-server daemon bootstrap`; it installs a detached hourly updater.

Use an explicit target when the user selected another listener:

```bash
CODEX_ENDPOINT=unix:///absolute/private/path/codex.sock
codex app-server --listen "$CODEX_ENDPOINT"
codex-tamer --connect "$CODEX_ENDPOINT" servers ping --json
codex --remote "$CODEX_ENDPOINT" --cd "$PWD"
```

Treat `CODEX_ENDPOINT` as a shell variable that keeps these example commands
consistent, not as a `codex-tamer` target source. Pass it through
`--connect "$CODEX_ENDPOINT"` or store the endpoint in the config file.

The official VS Code Codex 0.146 integration starts a private stdio-only
app-server and exposes no listener setting. `codex-tamer` cannot attach after
that stdio session has started, steer its active turn, or inject into it. Do not
start another server and claim that it controls that live runtime.

The shared app-server still reads the same `CODEX_HOME` state database and
scans session JSONL to repair persisted metadata. This makes persisted threads
discoverable; it does not transfer live ownership from a private stdio process.
Only clients connected to the same explicit endpoint share loaded-thread and
active-turn state. Use `codex --remote` for future live-controllable sessions.

On Windows, automatic Unix startup is unsupported. Require a configured
`ws://` or `wss://` endpoint; do not offer a Unix command as recovery.

Remember that `codex-tamer` requests the Codex experimental API capability.
Treat an `experimentalApi` capability error as a server compatibility problem.

Top-level and per-server `model` and `model_reasoning_effort` are defaults for
new threads or turns. They do not configure, select, start, or identify the
app-server.

## Apply the Safety Boundary

Assume that the inherited default is yolo mode: thread creation, automatic
resume, and turn start force approval policy `never` and full-access sandboxing.
Do not treat the CLI as a sandbox.

Use the product default when the user asks to operate Codex: it intentionally
sets approval policy `never` and full-access sandboxing. Pass global
`--no-yolo` only when the user explicitly asks to preserve app-server defaults:

```bash
codex-tamer --no-yolo new --cwd /absolute/worktree "Inspect only" --json
```

Expect a command to fail if the app-server policy requires an interactive
approval request. Do not assume this headless CLI will approve it; configure an
appropriate noninteractive policy at the trusted endpoint.

## Use Machine Output

Pass `--json` whenever reading IDs, state, cursors, timestamps, or results.
Parse stdout as JSON and treat stderr as diagnostics. Always check the process
exit code, including after successfully parsing stdout.

Interpret exit codes as:

| Code | Meaning |
| --- | --- |
| `0` | Command succeeded or a blocking turn completed. |
| `1` | A blocking turn reached `failed` or `interrupted`. |
| `2` | Usage, validation, config, argument, or local lookup error. |
| `3` | App-server, transport, capability, protocol, or timeout error. |
| `130` | Local Ctrl-C; the remote turn may still be running. |

Expect command-specific JSON objects rather than one universal response
envelope. On codes `2`, `3`, and `130`, inspect stderr. During NDJSON streaming,
retain already-received records but do not assume success until the process
exits `0`.

Allow up to 120 seconds without an app-server message for every ordinary
JSON-RPC request. Treat turn command `--timeout` values as separate operation
deadlines rather than overrides for this transport wait.

## Follow the Agent Workflow

1. Discover candidate threads with JSON.
2. Disambiguate by server, cwd, status, updated time, name, and preview.
3. Read status before mutating a thread.
4. Read the smallest useful history window.
5. Choose `send`, `steer`, or `interrupt` deliberately.
6. Preserve returned server, thread, turn, item, and cursor IDs exactly.

### Discover Threads

List recent work. Pair `--since` with updated-descending order so the scan can
stop once it reaches older threads:

```bash
codex-tamer list --since 24h --limit 50 --sort updated --desc --json
```

Without `--sort updated --desc`, a `--since` query must scan every server page
to avoid missing recent threads in server-defined order. `--limit` caps the
returned matches, not the amount of persisted history scanned. This can be
slow for large `CODEX_HOME` session stores, especially across WSL DrvFS.

Search metadata and previews:

```bash
codex-tamer search threads "release process" --limit 20 --json
```

Project compact fields only when useful:

```bash
codex-tamer list --since 24h --limit 50 --sort updated --desc --json \
  | jq '{threads:[.threads[] | {id,name,cwd,status:.status.type,updatedAt,preview}]}'
```

Use `list --parent THREAD_ID` for direct spawned children and
`list --ancestor THREAD_ID` for spawned descendants. Do not confuse these
`parentThreadId` edges with `forkedFromId` history forks.

### Check State

Read one thread without forcing a resume:

```bash
codex-tamer status THREAD_ID --json
```

Use `--load` only when current liveness matters and loading the thread is
acceptable:

```bash
codex-tamer status THREAD_ID --load --json
```

Treat a non-null `activeTurnId` as an active turn. Preserve that exact ID for
`steer` or `interrupt`.

### Read History

Read a small flattened window first:

```bash
codex-tamer messages THREAD_ID --last 8 --max-turns 50 --json
```

Use role filters to isolate intent or responses:

```bash
codex-tamer messages THREAD_ID --role user --last 10 --max-turns 100 --json
codex-tamer messages THREAD_ID --role assistant --last 3 --max-turns 50 --json
```

Apply `messages` limits in the correct order:

1. Scan the newest `--max-turns M` turns.
2. Flatten user and assistant messages.
3. Apply `--since` and `--role`.
4. Apply final `--last N`.

Increase `--max-turns` when a requested role or message may be outside the
recent scan. Do not invent `messages --first`; use `show --asc` and cursors for
older exact history.

```bash
page=$(codex-tamer show THREAD_ID --last 20 --items full --json)
cursor=$(printf '%s\n' "$page" | jq -r '.turns.nextCursor // empty')
codex-tamer show THREAD_ID --cursor "$cursor" --items full --json
```

Pass opaque cursor strings back exactly. Do not interpret them as offsets or
timestamps.

## Start Independent Work

Always pass an absolute working directory:

```bash
codex-tamer new --cwd /absolute/worktree/path \
  "Perform the requested task" --no-wait --json
```

Use a separate `codex-tamer` process per independent thread when running work
concurrently. Record each returned `server`, `threadId`, and `turnId` with the
calling Agent's own task state.

After a no-wait start, any controller holding the IDs can reattach:

```bash
codex-tamer wait THREAD_ID TURN_ID --timeout 3600 --json
```

Use blocking mode when the caller needs the result now:

```bash
codex-tamer new --cwd /absolute/worktree/path \
  "Analyze the failure and report findings" --json
```

Use `fork` to derive a thread from persisted history:

```bash
codex-tamer fork THREAD_ID --last-turn TURN_ID --json
```

## Control a Thread

Choose the operation by intent:

- Use blocking `send` to start a new turn and wait for its response.
- Use `send --no-wait` to start a new turn and return after acceptance.
- Use `wait` to reattach to an accepted turn and block for its terminal result.
- Use `result` for a one-shot persisted snapshot without subscribing.
- Use `events follow` to follow an existing turn as NDJSON.
- Use `steer` to add input to the current active turn.
- Use `interrupt` to request cancellation of the current active turn.

Wait for a new turn:

```bash
codex-tamer send THREAD_ID "Continue and report the result" --json
```

Return after acceptance:

```bash
codex-tamer send THREAD_ID "Run the requested checks" --no-wait --json
```

Stream one new turn as NDJSON:

```bash
codex-tamer send THREAD_ID "Stream the analysis" --json --stream
```

After `--no-wait`, keep both accepted IDs. Choose a later operation explicitly:

```bash
codex-tamer wait THREAD_ID TURN_ID --json
codex-tamer result THREAD_ID TURN_ID --json
codex-tamer events follow THREAD_ID TURN_ID
```

`wait` returns an aggregate terminal result. `result` cursor-pages through up
to the newest 200 turns by default without resuming the thread; increase
`--max-turns` for an older target. `events follow` first emits `attached`, then
replays persisted assistant content as `assistantMessage` records with
`source = "snapshot"`, and follows live or polled events through the terminal
record. Deduplicate or append content using event order and `itemId`; do not
discard snapshot records merely because they predate attachment. Do not infer
remote cancellation from a local timeout or Ctrl-C.

Treat a timeout as exit `3`. Treat Ctrl-C as exit `130` and read the selected
server, thread, and turn from stderr. Neither condition interrupts the remote
turn; call `interrupt` explicitly when cancellation is intended.

When a terminal turn fails, preserve the structured `error` object when
present; report it together with `status` and the process exit code.

Steer only after reading the active ID:

```bash
status=$(codex-tamer status THREAD_ID --json)
active_turn=$(printf '%s\n' "$status" | jq -r '.activeTurnId // empty')
codex-tamer steer THREAD_ID "$active_turn" "Prioritize the failing test" --json
```

`steer` never resumes an unloaded persisted thread. If the selected endpoint
does not own that active turn, report the runtime boundary instead of treating a
newly resumed snapshot as the original live session. `send`, `settings set`,
and `inject` may resume persisted state; use them only when another runtime is
not actively writing the same thread.

Interrupt explicitly:

```bash
codex-tamer interrupt THREAD_ID TURN_ID --json
```

Do not send disruptive input to an active thread unless the user asked for that
behavior. Do not replace `steer` with `send`; they have different semantics.

Use `inject` only when the request intentionally changes model-visible history
without starting a user turn. Pass a non-empty JSON array of raw Responses API
item objects, up to 16 MiB:

```bash
codex-tamer inject THREAD_ID --items-file /path/to/items.json --json
printf '%s\n' '[{"type":"message","role":"user","content":[]}]' \
  | codex-tamer inject THREAD_ID --items-file - --json
```

Do not use `inject` as a substitute for ordinary `send` or `steer` input.

## Understand Output Shapes

Use these root shapes for compact parsing:

- `list --json`: `{ server, threads, nextCursor, backwardsCursor }`
- `search threads --json`: `{ server, results, nextCursor, backwardsCursor }`
- `show --json`: `{ server, thread, turns }`
- `messages --json`: `{ server, threadId, messages, nextCursor, truncated }`
- `status --json`: `{ server, reachable, loadedThreadIds, nextCursor }`
- `status THREAD_ID --json`:
  `{ server, threadId, thread, activeTurnId, truncated }`
- blocking `new` or `send`:
  `{ server, threadId, turnId, status, progress, assistantResponses, finalAssistantText }`
- no-wait `new` or `send`:
  `{ type, server, threadId, turnId, status }`
- `wait`:
  `{ server, threadId, turnId, status, progress, assistantResponses, finalAssistantText }`
- `result`:
  `{ server, threadId, turnId, status, assistantResponses, finalAssistantText, turn }`
- `events follow`: NDJSON from `attached`, including snapshot/live assistant
  records, through the terminal record
- `inject`: `{ server, threadId, status, itemCount }`

Expect stream `type` values including `accepted`, `progress`,
`assistantMessage`, `completed`, `failed`, and `interrupted`. Use `itemId` to
keep separate assistant messages distinct when present.

Treat nested thread and turn objects as Codex-version-dependent. Avoid depending
on undocumented experimental fields.

## Manage Metadata Deliberately

Use persisted Codex-owned operations when required:

```bash
codex-tamer name THREAD_ID "Readable name" --json
codex-tamer pin THREAD_ID --json
codex-tamer unpin THREAD_ID --json
codex-tamer archive THREAD_ID --json
codex-tamer unarchive THREAD_ID --json
codex-tamer settings show THREAD_ID --json
codex-tamer goal get THREAD_ID --json
```

Use local annotations only as controller-side notes. Do not present them as
Codex app-server state:

```bash
codex-tamer annotate set THREAD_ID "Waiting for review" --json
codex-tamer annotate search "review" --json
codex-tamer annotate prune --dry-run --json
```

Run prune without `--dry-run` only when intentionally deleting local notes for
threads reported missing.

## Avoid Common Errors

- Do not use stale IDs without rechecking server, cwd, preview, and status.
- Do not merge results or cursors from different server aliases.
- Do not expose bearer tokens in output or repository files.
- Do not assume an empty thread list means Codex has no sessions.
- Do not assume successful JSON parsing means the command exited successfully.
- Do not lose the `threadId` or `turnId` returned by `--no-wait`.
- Do not assume local annotations are shared Codex metadata.
- Do not dump large raw histories into the user's context; project only fields
  needed for the requested decision.
