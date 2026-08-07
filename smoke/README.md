# codex-tamer Smoke Tests

Deterministic mock coverage lives in `tests/mock_smoke.rs` and runs with
`cargo test`. It launches mock Codex app-servers over Unix sockets and TCP
WebSockets, then exercises the compiled headless CLI and its JSON output.

This directory contains opt-in checks against a real Codex app-server. The
smoke harness has no TUI path.

## Default Live Smoke

Point the script at an already-running endpoint:

```bash
CODEX_ENDPOINT=unix:///var/run/user/1000/codex.sock \
  smoke/live_smoke.sh

CODEX_ENDPOINT=ws://127.0.0.1:8765 \
  smoke/live_smoke.sh
```

For an authenticated WebSocket endpoint, provide one token source:

```bash
CODEX_ENDPOINT=wss://codex.example.test \
CODEX_AUTH_TOKEN_ENV=CODEX_APP_SERVER_TOKEN \
  smoke/live_smoke.sh
```

`CODEX_AUTH_TOKEN=literal-token` is also supported for isolated testing, but an
environment variable or secret manager is preferable. Never commit a token.

The default script:

- builds `target/debug/codex-tamer` if needed;
- writes a temporary one-server config and disposable working directory;
- runs `servers ping`, `models`, promptless `new`, `status`, `settings show`,
  `name`, and `goal get/set/clear`;
- validates configured model/effort reporting and goal round-tripping;
- avoids model work and usage charges;
- removes its temporary files when it exits.

The endpoint is real state. The script creates a real thread even when model
work is disabled, so do not point it at an app-server where that is unwanted.

## Include One Real Turn

Enable the model-backed path explicitly:

```bash
RUN_CODEX_TURN=1 \
CODEX_MODEL=gpt-5.5 \
CODEX_EFFORT=high \
CODEX_ENDPOINT=unix:///var/run/user/1000/codex.sock \
  smoke/live_smoke.sh
```

This sends a small prompt to the created thread and waits for the final JSON
response. It requires model access and can incur usage.

## Include Archive Mutations

Set `RUN_ARCHIVE=1` to exercise `archive` and `unarchive` against the disposable
thread. Those commands are covered by mock tests by default because live
archive behavior can depend on local session-store state.

## Override the Binary

Use an existing build without rebuilding:

```bash
BIN=/absolute/path/to/codex-tamer \
CODEX_ENDPOINT=unix:///path/to/codex.sock \
  smoke/live_smoke.sh
```
