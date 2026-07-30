# Codex compatibility

This file records intentional reviews of the Codex app-server API. It is a
provenance log, not a promise that older or newer app-server builds will support
every command: Codex experimental APIs can change between releases.

Use the exact upstream tag and commit that were inspected. When a
`codex-threads` change adopts API additions from a new Codex release, update the
`Unreleased` row in the same change. The release script replaces `Unreleased`
with the released `codex-threads` version in the release commit.

| codex-threads | Codex app release | Upstream reference | Reviewed integration scope |
| --- | --- | --- | --- |
| Unreleased | 0.146 | `rust-v0.146.0` (`e363b08c9175ac1cbe5893615dd2cb9ddf95043b`) | Persisted thread pin/unpin through `thread/metadata/update`, `thread/list` pin filtering, and `Thread.isPinned` rendering; persisted message search through `thread/searchOccurrences`; and direct-input safeguards through `Thread.canAcceptDirectInput`. Experimental fork/history additions remain deferred. Peer `main` already has a post-0.146 persisted-section model that supersedes the boolean pin API; reassess it when targeting a release newer than 0.146 rather than carrying both contracts. |
| 0.2.3 | 0.143 inherited | No newer baseline was recorded | Added provider/source filters, TUI deletion, and detailed rate-limit reset redemption. This release did not document a separate Codex API review, so 0.143 remains the last evidenced baseline. |
| 0.2.2 | 0.143 | `rust-v0.143.0` (`c4d748f586a84a3ed5b6aceb82e9a1db4abb1cda`) | Explicit Codex 0.143 integration update: thread fork, parent/ancestor relationship filters, and expanded reasoning-effort pass-through. |

## Review checklist

For each intentional Codex release sync:

1. Compare the public app-server protocol and app-server README from the last
   recorded upstream reference to the new exact tag or commit.
2. Classify additions as adopted, intentionally deferred, or irrelevant to this
   CLI.
3. Update the table above, `README.md`, tests, and `CHANGELOG.md` in the same
   change as adopted user-facing behavior.
4. Keep deferred items in the newest table row so the next review does not
   rediscover them from scratch.
