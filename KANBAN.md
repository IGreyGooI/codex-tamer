# Kanban

## Completed

### Bootstrap a local Codex app-server

- [x] Add an explicit `codex-tamer` workflow that can bootstrap and start a
  compatible local Codex app-server.

  Scope and acceptance criteria:

  - Start only the reviewed Codex `app-server --listen` runtime. Keep
    `codex-tamer` as the controller and do not add another model runtime, thread
    index, scheduler, or updater.
  - Keep `codex app-server daemon bootstrap` as a separate explicit opt-in. In
    Codex 0.146 it requires the standalone managed install and starts a detached
    hourly updater that runs the installer.
  - Resolve the `codex` executable explicitly from `PATH` or a caller-provided
    path, honor `CODEX_HOME`, validate the supported Codex version, and invoke it
    with structured arguments rather than a shell command string.
  - Make repeated bootstrap/start calls idempotent and return machine-readable
    status containing the selected Codex binary, Codex home, app-server target,
    and whether configuration was created or reused. Keep diagnostics on
    stderr and never print credentials or bearer tokens.
  - Validate the resulting endpoint with the same app-server handshake used by
    `servers ping`. Do not report success until the endpoint is reachable and
    protocol-compatible.
  - Add deterministic tests with a fake `codex` executable; live listener tests
    must remain opt-in under `smoke/`.
  - Update `README.md`, `skills/codex-tamer/SKILL.md`, `CHANGELOG.md`, and
    `CODEX_COMPATIBILITY.md` with the reviewed upstream command contract and
    operational safety boundary.
