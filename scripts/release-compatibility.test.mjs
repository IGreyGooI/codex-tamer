import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import {
	requireCompatibilityFile,
	updateCompatibilityForRelease,
} from "./release-compatibility.mjs";

function withTempDirectory(run) {
	const directory = mkdtempSync(join(tmpdir(), "codex-threads-release-test-"));
	try {
		run(directory);
	} finally {
		rmSync(directory, { recursive: true, force: true });
	}
}

test("stamps the Unreleased compatibility row", () => {
	withTempDirectory((directory) => {
		const path = join(directory, "CODEX_COMPATIBILITY.md");
		writeFileSync(
			path,
			[
				"| codex-threads | Codex app release |",
				"| --- | --- |",
				"| Unreleased | 0.146 |",
				"",
			].join("\n"),
		);

		assert.equal(updateCompatibilityForRelease(path, "0.3.0"), true);
		assert.match(readFileSync(path, "utf-8"), /^\| 0\.3\.0 \| 0\.146 \|$/m);
	});
});

test("leaves a compatibility ledger without an Unreleased row unchanged", () => {
	withTempDirectory((directory) => {
		const path = join(directory, "CODEX_COMPATIBILITY.md");
		const content = "| codex-threads | Codex app release |\n| 0.2.3 | 0.143 |\n";
		writeFileSync(path, content);

		assert.equal(updateCompatibilityForRelease(path, "0.3.0"), false);
		assert.equal(readFileSync(path, "utf-8"), content);
	});
});

test("rejects a missing compatibility ledger", () => {
	withTempDirectory((directory) => {
		const path = join(directory, "CODEX_COMPATIBILITY.md");
		assert.throws(
			() => requireCompatibilityFile(path),
			/Required compatibility ledger is missing/,
		);
		assert.throws(
			() => updateCompatibilityForRelease(path, "0.3.0"),
			/Required compatibility ledger is missing/,
		);
	});
});
