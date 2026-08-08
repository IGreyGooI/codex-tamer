import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import test from "node:test";

const root = resolve(import.meta.dirname, "..");

test("repository text keeps LF line endings on every checkout platform", () => {
	const attributes = readFileSync(join(root, ".gitattributes"), "utf8");
	assert.match(attributes, /^\* text=auto eol=lf$/m);
});

test("the packaged Skill declares the codex-tamer PATH dependency", () => {
	const skill = readFileSync(join(root, "skills", "codex-tamer", "SKILL.md"), "utf8");
	const frontmatter = skill.match(/^---\n([\s\S]*?)\n---\n/);
	assert.ok(frontmatter, "SKILL.md must start with YAML frontmatter");
	assert.match(frontmatter[1], /metadata:\n  requires:\n    bins: \["codex-tamer"\]/);
	assert.match(frontmatter[1], /cliHelp: "codex-tamer --help"/);
});

test("the packaged Skill explains the VS Code stdio attachment boundary", () => {
	const skill = readFileSync(join(root, "skills", "codex-tamer", "SKILL.md"), "utf8");
	const normalized = skill.replace(/\s+/g, " ");
	assert.match(normalized, /VS Code[^.]*stdio/i);
	assert.match(normalized, /cannot attach[^.]*after[^.]*started/i);
	assert.match(normalized, /CODEX_ENDPOINT[^.]*shell variable[^.]*not[^.]*target/i);
	assert.match(normalized, /same `CODEX_HOME` state database/i);
	assert.match(normalized, /persisted threads discoverable/i);
	assert.match(normalized, /does not transfer live ownership/i);
});

test("the packaged Skill documents managed shared-listener setup", () => {
	const skill = readFileSync(join(root, "skills", "codex-tamer", "SKILL.md"), "utf8");
	const normalized = skill.replace(/\s+/g, " ");
	assert.match(skill, /codex-tamer servers start --json/);
	assert.match(skill, /codex-tamer servers status --json/);
	assert.match(skill, /codex-tamer servers stop --json/);
	assert.match(skill, /\$XDG_RUNTIME_DIR\/codex-tamer\/<24-char CODEX_HOME hash>\/app-server\.sock/);
	assert.match(normalized, /absent default config is valid/i);
	assert.match(normalized, /servers ping[^.]*do not start/i);
	assert.match(normalized, /status = "stopped"[^.]*confirmed absent listener/i);
	assert.match(normalized, /incompatible[^.]*malformed[^.]*insecure[^.]*exits `3`/i);
	assert.match(normalized, /daemon bootstrap[^.]*hourly updater/i);
	assert.match(normalized, /On Windows[^.]*automatic Unix startup is unsupported/i);
	assert.match(normalized, /Require a configured `ws:\/\/` or `wss:\/\/` endpoint/i);
	assert.match(normalized, /model_reasoning_effort` are defaults/i);
	assert.match(normalized, /do not configure, select, start, or identify the app-server/i);
});

test("the packaged Skill preserves explicit endpoint commands", () => {
	const skill = readFileSync(join(root, "skills", "codex-tamer", "SKILL.md"), "utf8");
	assert.match(skill, /codex app-server --listen "\$CODEX_ENDPOINT"/);
	assert.match(skill, /codex-tamer --connect "\$CODEX_ENDPOINT" servers ping --json/);
	assert.match(skill, /codex --remote "\$CODEX_ENDPOINT" --cd "\$PWD"/);
});

test("the packaged Skill bounds recent discovery and documents the RPC wait", () => {
	const skill = readFileSync(join(root, "skills", "codex-tamer", "SKILL.md"), "utf8");
	const normalized = skill.replace(/\s+/g, " ");
	assert.match(
		skill,
		/codex-tamer list --since 24h --limit 50 --sort updated --desc --json/,
	);
	assert.match(normalized, /`--limit` caps the returned matches, not the amount[^.]*scanned/i);
	assert.match(normalized, /120 seconds without an app-server message/i);
});
