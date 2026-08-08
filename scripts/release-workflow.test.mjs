import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import test from "node:test";

import {
	publicReleaseNotes,
	releaseNotesFromChangelog,
	releasePreflightCommands,
	releaseRemoteFromEnvironment,
	updateReadmeInstallVersion,
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
	assert.match(workflow, /workflow_dispatch:\n\s+inputs:\n\s+expected_sha:/);
	assert.match(workflow, /EXPECTED_SHA: \$\{\{ inputs\.expected_sha \}\}/);
	assert.match(workflow, /\[ "\$GITHUB_SHA" != "\$EXPECTED_SHA" \]/);
	assert.match(workflow, /permissions:\n  contents: read/);
	assert.match(
		workflow,
		/release:\n\s+if: github\.event_name == 'push'\n[\s\S]*?permissions:\n      contents: write\n[\s\S]*?runs-on:/,
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
	assert.match(
		workflow,
		/amazonlinux@sha256:694092ae18877ed4e3cb9b643759ba95df1f12af12528fefa18f60f79d4c1568/,
	);
	assert.match(workflow, /--target-dir release-target/);
	assert.match(workflow, /rust_sysroot="\$\(rustc --print sysroot\)"/);
	assert.doesNotMatch(workflow, /--volume "\$CARGO_HOME/);
	assert.match(workflow, /gcc-11\.5\.0-5\.amzn2023\.0\.5/);
	assert.match(workflow, /openssl-devel-1:3\.5\.5-1\.amzn2023\.0\.5/);
	assert.match(workflow, /pkgconf-pkg-config-1\.8\.0-4\.amzn2023\.0\.2/);
	assert.match(workflow, /glibc_max="2\.34"/);
	assert.match(workflow, /detected maximum GLIBC_/);
	assert.match(workflow, /for library in libssl\.so\.3 libcrypto\.so\.3/);
	assert.match(workflow, /does not declare the required \$library dependency/);
	assert.match(workflow, /openssl_symbol_floor="OPENSSL_3\.0\.0"/);
	assert.match(workflow, /requires unsupported OpenSSL symbol versions/);
	assert.match(workflow, /readFileSync\("CHANGELOG\.md"/);
	assert.match(workflow, /import \{ publicReleaseNotes \} from "\.\/scripts\/release\.mjs"/);
	assert.match(workflow, /const notes = publicReleaseNotes\(/);
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
			["https://github.com/IGreyGooI/codex-tamer.git"],
			["git@github.com:IGreyGooI/codex-tamer.git"],
		),
		"IGreyGooI/codex-tamer",
	);
	assert.throws(
		() =>
			validateReleaseRemoteUrls(
				["https://github.com/example/fork.git"],
				["https://github.com/IGreyGooI/codex-tamer.git"],
			),
		/release remote fetch URL/i,
	);
	assert.throws(
		() =>
			validateReleaseRemoteUrls(
				["https://github.com/IGreyGooI/codex-tamer.git"],
				[
					"https://github.com/IGreyGooI/codex-tamer.git",
					"git@github.com:IGreyGooI/codex-tamer.git",
				],
			),
		/exactly one.*push URL/i,
	);
	assert.throws(
		() => validateReleaseRemoteUrls([], ["git@github.com:IGreyGooI/codex-tamer.git"]),
		/exactly one.*fetch URL/i,
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
	const publicNotes = publicReleaseNotes(changelog, "1.2.3");
	assert.match(publicNotes, /## Install/);
	assert.match(publicNotes, /Node\.js 20\+/);
	assert.match(publicNotes, /node install\.mjs --json/);
	assert.match(publicNotes, /blob\/v1\.2\.3\/README\.md#install/);
	assert.match(publicNotes, /## Changes[\s\S]*Agent-first release bundles\./);
});

test("updates the public install example to the release version", () => {
	const readme = [
		"# Install",
		"",
		"```bash",
		"VERSION=1.2.2",
		'ASSET="codex-tamer-${VERSION}-linux-x86_64.tar.gz"',
		"```",
		"",
	].join("\n");
	assert.equal(
		updateReadmeInstallVersion(readme, "1.2.3"),
		readme.replace("VERSION=1.2.2", "VERSION=1.2.3"),
	);
	assert.throws(
		() => updateReadmeInstallVersion("# Install\n", "1.2.3"),
		/exactly one public install version/i,
	);
	assert.throws(
		() => updateReadmeInstallVersion(`${readme}${readme}`, "1.2.3"),
		/exactly one public install version/i,
	);
	assert.throws(
		() => updateReadmeInstallVersion(readme, "next"),
		/stable semantic version/i,
	);

	const releaseScript = readFileSync(join(root, "scripts", "release.mjs"), "utf8");
	assert.match(
		releaseScript,
		/const updatedReadme = prepareReadmeForRelease\(version\);[\s\S]*?updateCargoTomlVersion\(version\);[\s\S]*?writeFileSync\(readmePath, updatedReadme, "utf-8"\);[\s\S]*?runReleasePreflight\(\);/,
	);
	assert.match(
		releaseScript,
		/const releaseCommitPaths = \["Cargo\.toml", "CHANGELOG\.md", "README\.md"\];/,
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
		/runReleasePreflight\(\);[\s\S]*?\["push", releaseRemote, RELEASE_BRANCH\][\s\S]*?runRemoteReleasePreflight\(releaseCommit\);[\s\S]*?\["tag", "-a", `v\$\{version\}`/,
	);
	assert.match(releaseScript, /gh[\s\S]*?workflow[\s\S]*?expected_sha/);
	assert.match(releaseScript, /gh[\s\S]*?run[\s\S]*?watch[\s\S]*?--exit-status/);
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
