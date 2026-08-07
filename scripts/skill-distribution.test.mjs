import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import test from "node:test";

const root = resolve(import.meta.dirname, "..");

test("the packaged Skill declares the codex-tamer PATH dependency", () => {
	const skill = readFileSync(join(root, "skills", "codex-tamer", "SKILL.md"), "utf8");
	const frontmatter = skill.match(/^---\n([\s\S]*?)\n---\n/);
	assert.ok(frontmatter, "SKILL.md must start with YAML frontmatter");
	assert.match(frontmatter[1], /metadata:\n  requires:\n    bins: \["codex-tamer"\]/);
	assert.match(frontmatter[1], /cliHelp: "codex-tamer --help"/);
});
