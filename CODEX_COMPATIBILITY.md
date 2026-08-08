# Codex Compatibility

This file records intentional reviews of the Codex app-server API. It is a
provenance log, not a promise that older or newer app-server builds support
every command. Codex experimental APIs can change between releases.

## Current Hard-Fork Baseline

`codex-tamer` was hard-forked from `kcosr/codex-threads` `0.2.4` at commit
`73485b0861ef0c2a1b78db552fd838c43635dee9`. The fork removes the interactive
TUI and renames the product surface. New commands remain on the same reviewed
Codex `0.146` protocol baseline.

| codex-tamer | Codex app release | Upstream reference | Reviewed integration scope |
| --- | --- | --- | --- |
| 0.3.0 | 0.146 inherited from codex-threads 0.2.4 | `rust-v0.146.0` (`e363b08c9175ac1cbe5893615dd2cb9ddf95043b`) | Headless hard fork; independent wait/result/follow use `thread/resume`, `thread/turns/list`, and existing notifications. Raw history injection adopts the 0.146 stable `thread/inject_items` request with required `threadId` and `items` array. Unix managed startup invokes the reviewed `app-server --listen unix://...` contract, validates initialize `userAgent` and `codexHome`, and relies on 0.146 thread listing to scan session JSONL and repair state-database metadata. `steer` no longer resumes an unloaded thread. No post-0.146 API is assumed. |

## Preserved Upstream Reviews

These rows are the original `codex-threads` compatibility record and remain
the evidence behind the inherited baseline.

| codex-threads | Codex app release | Upstream reference | Reviewed integration scope |
| --- | --- | --- | --- |
| 0.2.4 | 0.146 | `rust-v0.146.0` (`e363b08c9175ac1cbe5893615dd2cb9ddf95043b`) | Persisted thread pin/unpin through `thread/metadata/update`, `thread/list` pin filtering, and `Thread.isPinned` rendering; and direct-input safeguards through `Thread.canAcceptDirectInput`. Persisted occurrence search through `thread/searchOccurrences` remains internally implemented but is not exposed as a CLI command: release 0.146 returns unsupported for legacy-history threads, and legacy remains the default history mode. Experimental fork/history additions remain deferred. Peer `main` already has a post-0.146 persisted-section model that supersedes the boolean pin API; reassess it when targeting a release newer than 0.146 rather than carrying both contracts. |
| 0.2.3 | 0.143 inherited | No newer baseline was recorded | Added provider/source filters, TUI deletion, and detailed rate-limit reset redemption. This release did not document a separate Codex API review, so 0.143 remains the last evidenced baseline. |
| 0.2.2 | 0.143 | `rust-v0.143.0` (`c4d748f586a84a3ed5b6aceb82e9a1db4abb1cda`) | Explicit Codex 0.143 integration update: thread fork, parent/ancestor relationship filters, and expanded reasoning-effort pass-through. |

## Review Checklist

For each intentional Codex release sync:

1. Compare the public app-server protocol and app-server README from the last
   recorded reference to the new exact tag or commit.
2. Classify additions as adopted, intentionally deferred, or irrelevant to the
   headless Agent CLI.
3. Update this file, README, tests, Skill instructions, and the Unreleased
   changelog in the same change as adopted behavior.
4. Add an `Unreleased` `codex-tamer` row rather than rewriting preserved
   `codex-threads` rows.
5. Test capability failures and malformed responses, not only successful calls.
