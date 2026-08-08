import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import test from "node:test";

import {
	releaseNotesFromChangelog,
	releasePreflightCommands,
	releaseRemoteFromEnvironment,
	validateReleaseRemoteUrls,
	validateReleasePushUrl,
} from "./release.mjs";

const root = join(import.meta.dirname, "..");

test("tag workflow builds, verifies, and uploads every supported platform bundle", () => {
	const workflow = readFileSync(join(root, ".github", "workflows", "release-assets.yml"), "utf8");
	for (const target of [
		"linux-x86_64",
		"linux-aarch64",
		"macos-aarch64",
		"macos-x86_64",
		"windows-x86_64",
	]) {
		assert.match(workflow, new RegExp(`bundle_target: ${target}\\b`));
	}
	assert.match(workflow, /tags:\s*\["v\*"\]/);
	assert.match(workflow, /permissions:\n  contents: read/);
	assert.match(
		workflow,
		/release:\n[\s\S]*?permissions:\n      contents: write\n[\s\S]*?runs-on:/,
	);
	assert.match(workflow, /node scripts\/package-release\.mjs/);
	assert.match(workflow, /archive_path=.*archivePath/);
	assert.match(
		workflow,
		/if \[ "\$\{\{ matrix\.bundle_target \}\}" = "windows-x86_64" \]; then[\s\S]*?unzip -q "\$archive_path" -d "\$extract_root"[\s\S]*?else[\s\S]*?tar -xf "\$archive_path" -C "\$extract_root"/,
	);
	assert.match(workflow, /tar -xf "\$archive_path"/);
	assert.match(workflow, /node "\$\{bundle_path\}\/install\.mjs"/);
	assert.match(workflow, /node scripts\/assemble-release-assets\.mjs/);
	assert.match(workflow, /gh release create/);
	assert.match(workflow, /gh release upload/);
	assert.equal(workflow.match(/node-version: "20"/g)?.length, 2);
	assert.match(
		workflow,
		/- runner: macos-15\n\s+rust_target: aarch64-apple-darwin/,
	);
	assert.match(workflow, /Verify Linux ABI baseline/);
	assert.match(workflow, /GLIBC_2\.34/);
	assert.match(workflow, /libssl\\\.so\\\.3/);
	assert.match(workflow, /libcrypto\\\.so\\\.3/);
	assert.match(workflow, /readFileSync\("CHANGELOG\.md"/);
	assert.match(workflow, /--notes-file "\$release_notes"/);
	assert.match(workflow, /Verify tag provenance/);
	assert.match(
		workflow,
		/git merge-base --is-ancestor "\$GITHUB_SHA" refs\/remotes\/origin\/main/,
	);
	assert.doesNotMatch(workflow, /--generate-notes/);
	assert.match(workflow, /existing_is_draft/);
	assert.match(workflow, /Refusing to modify published GitHub Release/);
	for (const line of workflow.split("\n").filter((entry) => /^\s*-?\s*uses:/.test(entry))) {
		assert.match(line, /@[0-9a-f]{40}\s*$/, `action must be pinned by commit: ${line}`);
	}
	assert.equal(
		workflow.match(/persist-credentials: false/g)?.length,
		2,
		"each checkout must avoid persisting the GitHub token",
	);
});

test("the local release script validates a configurable private release remote", () => {
	assert.equal(releaseRemoteFromEnvironment({}), "upstream");
	assert.equal(
		releaseRemoteFromEnvironment({ CODEX_TAMER_RELEASE_REMOTE: "private-origin" }),
		"private-origin",
	);
	assert.throws(
		() => releaseRemoteFromEnvironment({ CODEX_TAMER_RELEASE_REMOTE: "--upload-pack=bad" }),
		/invalid release remote/i,
	);
	assert.equal(
		validateReleasePushUrl("https://github.com/IGreyGooI/codex-tamer.git"),
		"IGreyGooI/codex-tamer",
	);
	assert.equal(
		validateReleasePushUrl("git@github.com:IGreyGooI/codex-tamer.git"),
		"IGreyGooI/codex-tamer",
	);
	assert.throws(
		() => validateReleasePushUrl("https://github.com/IGreyGooI/another-repo.git"),
		/does not match.*IGreyGooI\/codex-tamer/i,
	);
	assert.throws(
		() => validateReleasePushUrl("https://token@github.com/IGreyGooI/codex-tamer.git"),
		/invalid GitHub push URL/i,
	);
	assert.equal(
		validateReleaseRemoteUrls(
			"https://github.com/IGreyGooI/codex-tamer.git",
			"git@github.com:IGreyGooI/codex-tamer.git",
		),
		"IGreyGooI/codex-tamer",
	);
	assert.throws(
		() =>
			validateReleaseRemoteUrls(
				"https://github.com/example/fork.git",
				"https://github.com/IGreyGooI/codex-tamer.git",
			),
		/release remote fetch URL/i,
	);
});

test("extracts curated notes from the exact tagged changelog section", () => {
	const changelog = [
		"# Changelog",
		"",
		"## [1.2.3] - 2026-08-07",
		"",
		"### Added",
		"",
		"- Agent-first release bundles.",
		"",
		"## [1.2.2] - 2026-08-01",
		"",
		"- Previous notes.",
		"",
	].join("\n");
	assert.equal(
		releaseNotesFromChangelog(changelog, "1.2.3"),
		"### Added\n\n- Agent-first release bundles.",
	);
	assert.throws(
		() => releaseNotesFromChangelog(changelog, "9.9.9"),
		/no release section for 9\.9\.9/i,
	);
	assert.throws(
		() => releaseNotesFromChangelog("## [1.2.3]\n\n## [1.2.2]\nnotes\n", "1.2.3"),
		/release section for 1\.2\.3 is empty/i,
	);
});

test("the local release script runs the complete locked preflight before tagging", () => {
	assert.deepEqual(releasePreflightCommands("/usr/bin/node", ["scripts/example.test.mjs"]), [
		[
			"/usr/bin/node",
			["--test", "--experimental-test-coverage", "scripts/example.test.mjs"],
		],
		["cargo", ["fmt", "--check"]],
		["cargo", ["test", "--locked"]],
		[
			"cargo",
			["clippy", "--locked", "--all-targets", "--all-features", "--", "-D", "warnings"],
		],
		["cargo", ["build", "--locked", "--release"]],
	]);
	const releaseScript = readFileSync(join(root, "scripts", "release.mjs"), "utf8");
	assert.match(
		releaseScript,
		/runReleasePreflight\(\);[\s\S]*?\["tag", `v\$\{version\}`\]/,
	);
});

test("the local release script leaves GitHub Release creation to the tag workflow", () => {
	const releaseScript = readFileSync(join(root, "scripts", "release.mjs"), "utf8");
	assert.doesNotMatch(releaseScript, /"release"\s*,\s*"create"/s);
	assert.doesNotMatch(releaseScript, /gh release create/);
	const readme = readFileSync(join(root, "README.md"), "utf8");
	assert.match(readme, /tag workflow/i);
});

test("the README documents unauthenticated public release installation", () => {
	const readme = readFileSync(join(root, "README.md"), "utf8");
	const normalizedReadme = readme.replace(/\s+/g, " ");
	assert.match(normalizedReadme, /do not need[^.]*GitHub authentication/i);
	assert.match(readme, /curl --fail --location --remote-name/);
	assert.match(readme, /sha256sum -c "\$\{ASSET\}\.sha256"/);
	assert.match(readme, /node install\.mjs --json/);
	assert.doesNotMatch(readme, /For the private repository/i);
});
