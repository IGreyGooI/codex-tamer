#!/usr/bin/env node
/**
 * Release script for codex-tamer.
 *
 * Usage:
 *   node scripts/release.mjs current
 *   node scripts/release.mjs patch
 *   node scripts/release.mjs minor
 *   node scripts/release.mjs major
 *   node scripts/release.mjs 0.3.0
 */

import { execFileSync } from "node:child_process";
import { existsSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import {
	requireCompatibilityFile,
	updateCompatibilityForRelease,
} from "./release-compatibility.mjs";

const __dirname = dirname(fileURLToPath(import.meta.url));
const ROOT = join(__dirname, "..");
const PACKAGE_NAME = "codex-tamer";
const RELEASE_REPOSITORY = "IGreyGooI/codex-tamer";
const RELEASE_BRANCH = "main";
const RELEASE_WORKFLOW = "release-assets.yml";
const RELEASE_GATE_DISCOVERY_TIMEOUT_MS = 60_000;
const RELEASE_GATE_POLL_MS = 2_000;
const BUMP_ARGS = new Set(["major", "minor", "patch"]);
const VERSION_ARG = /^\d+\.\d+\.\d+$/;
const cargoTomlPath = join(ROOT, "Cargo.toml");
const cargoLockPath = join(ROOT, "Cargo.lock");
const changelogPath = join(ROOT, "CHANGELOG.md");
const compatibilityPath = join(ROOT, "CODEX_COMPATIBILITY.md");
const readmePath = join(ROOT, "README.md");

export function releaseRemoteFromEnvironment(environment = process.env) {
	const configured = environment.CODEX_TAMER_RELEASE_REMOTE;
	const releaseRemote = configured === undefined ? "upstream" : String(configured);
	if (!/^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/.test(releaseRemote)) {
		throw new Error("invalid release remote; set CODEX_TAMER_RELEASE_REMOTE to a git remote name");
	}
	return releaseRemote;
}

function repositoryFromGitHubPushUrl(pushUrl) {
	const scpMatch = String(pushUrl).match(/^git@github\.com:(.+)$/i);
	let repositoryPath;
	if (scpMatch) {
		repositoryPath = scpMatch[1];
	} else {
		let parsed;
		try {
			parsed = new URL(String(pushUrl));
		} catch {
			throw new Error("invalid GitHub push URL for codex-tamer release remote");
		}
		const validHttps =
			parsed.protocol === "https:" &&
			parsed.username === "" &&
			parsed.password === "" &&
			parsed.port === "";
		const validSsh =
			parsed.protocol === "ssh:" &&
			parsed.username === "git" &&
			parsed.password === "" &&
			(parsed.port === "" || parsed.port === "22");
		if (
			parsed.hostname.toLowerCase() !== "github.com" ||
			(!validHttps && !validSsh) ||
			parsed.search !== "" ||
			parsed.hash !== ""
		) {
			throw new Error("invalid GitHub push URL for codex-tamer release remote");
		}
		repositoryPath = parsed.pathname;
	}

	const repository = repositoryPath.replace(/^\/+|\/+$/g, "").replace(/\.git$/i, "");
	if (!/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(repository)) {
		throw new Error("invalid GitHub push URL for codex-tamer release remote");
	}
	return repository;
}

export function validateReleasePushUrl(pushUrl) {
	const repository = repositoryFromGitHubPushUrl(pushUrl);
	if (repository.toLowerCase() !== RELEASE_REPOSITORY.toLowerCase()) {
		throw new Error(
			`release remote repository ${repository} does not match ${RELEASE_REPOSITORY}`,
		);
	}
	return repository;
}

export function validateReleaseRemoteUrls(fetchUrls, pushUrls) {
	if (!Array.isArray(fetchUrls) || fetchUrls.length !== 1) {
		throw new Error("release remote must have exactly one fetch URL");
	}
	if (!Array.isArray(pushUrls) || pushUrls.length !== 1) {
		throw new Error("release remote must have exactly one push URL");
	}
	const [fetchUrl] = fetchUrls;
	const [pushUrl] = pushUrls;
	let fetchRepository;
	try {
		fetchRepository = validateReleasePushUrl(fetchUrl);
	} catch (error) {
		throw new Error(`release remote fetch URL is invalid: ${error.message}`);
	}
	try {
		validateReleasePushUrl(pushUrl);
	} catch (error) {
		throw new Error(`release remote push URL is invalid: ${error.message}`);
	}
	return fetchRepository;
}

export function releaseNotesFromChangelog(content, version) {
	if (typeof content !== "string" || !VERSION_ARG.test(String(version))) {
		throw new Error("release notes require changelog text and a stable semantic version");
	}
	const lines = content.split(/\r?\n/);
	const heading = `## [${version}]`;
	const start = lines.findIndex((line) => line === heading || line.startsWith(`${heading} - `));
	if (start === -1) {
		throw new Error(`CHANGELOG.md has no release section for ${version}`);
	}
	const nextHeading = lines.findIndex(
		(line, index) => index > start && line.startsWith("## ["),
	);
	const notes = lines
		.slice(start + 1, nextHeading === -1 ? undefined : nextHeading)
		.join("\n")
		.trim();
	if (!notes) {
		throw new Error(`CHANGELOG.md release section for ${version} is empty`);
	}
	return notes;
}

export function publicReleaseNotes(content, version) {
	const changes = releaseNotesFromChangelog(content, version);
	return [
		"## Install",
		"",
		"Download the archive for your platform and its adjacent `.sha256` file, verify the checksum, then extract the archive. From the extracted directory run:",
		"",
		"```bash",
		"node install.mjs --json",
		"```",
		"",
		`Node.js 20+ is required. See the [full install instructions](https://github.com/${RELEASE_REPOSITORY}/blob/v${version}/README.md#install).`,
		"",
		"## Changes",
		"",
		changes,
	].join("\n");
}

export function updateReadmeInstallVersion(content, version) {
	if (typeof content !== "string" || !VERSION_ARG.test(String(version))) {
		throw new Error("README update requires text and a stable semantic version");
	}
	const installVersion = /^VERSION=\d+\.\d+\.\d+$/gm;
	const matches = content.match(installVersion) ?? [];
	if (matches.length !== 1) {
		throw new Error("README.md must contain exactly one public install version");
	}
	return content.replace(installVersion, `VERSION=${version}`);
}

export function releasePreflightCommands(nodeExecutable, testPaths) {
	if (!nodeExecutable || !Array.isArray(testPaths) || testPaths.length === 0) {
		throw new Error("release preflight requires Node.js and at least one test file");
	}
	return [
		[nodeExecutable, ["--test", "--experimental-test-coverage", ...testPaths]],
		["cargo", ["fmt", "--check"]],
		["cargo", ["test", "--locked"]],
		[
			"cargo",
			["clippy", "--locked", "--all-targets", "--all-features", "--", "-D", "warnings"],
		],
		["cargo", ["build", "--locked", "--release"]],
	];
}

function runFile(command, args, options = {}) {
	console.log(`$ ${[command, ...args.map((arg) => JSON.stringify(arg))].join(" ")}`);
	try {
		return execFileSync(command, args, {
			encoding: "utf-8",
			stdio: options.silent ? "pipe" : "inherit",
			cwd: ROOT,
			...options,
		});
	} catch (error) {
		if (!options.ignoreError) {
			console.error(`Command failed: ${command} ${args.join(" ")}`);
			process.exit(1);
		}
		return null;
	}
}

function getVersion() {
	const content = readFileSync(cargoTomlPath, "utf-8");
	const match = content.match(/\[package\][\s\S]*?\nversion\s*=\s*"([^"]+)"/);
	if (!match) {
		console.error("Could not find version in Cargo.toml [package] section");
		process.exit(1);
	}
	return match[1];
}

function parseVersion(version) {
	const match = version.match(/^(\d+)\.(\d+)\.(\d+)$/);
	if (!match) {
		return null;
	}
	return {
		major: Number.parseInt(match[1], 10),
		minor: Number.parseInt(match[2], 10),
		patch: Number.parseInt(match[3], 10),
	};
}

function formatVersion(parts) {
	return `${parts.major}.${parts.minor}.${parts.patch}`;
}

function bumpVersion(currentVersion, bumpArg) {
	if (VERSION_ARG.test(bumpArg)) {
		return bumpArg;
	}
	const parts = parseVersion(currentVersion);
	if (!parts) {
		console.error(`Current version "${currentVersion}" is not valid semver`);
		process.exit(1);
	}
	if (bumpArg === "patch") {
		parts.patch += 1;
	} else if (bumpArg === "minor") {
		parts.minor += 1;
		parts.patch = 0;
	} else if (bumpArg === "major") {
		parts.major += 1;
		parts.minor = 0;
		parts.patch = 0;
	}
	return formatVersion(parts);
}

function escapeRegex(value) {
	return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function updateCargoTomlVersion(newVersion) {
	let content = readFileSync(cargoTomlPath, "utf-8");
	const versionRegex = /(\[package\][\s\S]*?\nversion\s*=\s*")[^"]*(")/;
	if (!versionRegex.test(content)) {
		console.error("Cargo.toml [package] version not found");
		process.exit(1);
	}
	content = content.replace(versionRegex, `$1${newVersion}$2`);
	writeFileSync(cargoTomlPath, content, "utf-8");
}

function updateCargoLockVersion(newVersion) {
	if (!existsSync(cargoLockPath)) {
		return;
	}
	let content = readFileSync(cargoLockPath, "utf-8");
	const packageRegex = new RegExp(
		`(\\[\\[package\\]\\]\\nname = "${escapeRegex(PACKAGE_NAME)}"\\nversion = ")[^"]*(")`
	);
	if (!packageRegex.test(content)) {
		console.error(`Cargo.lock package entry not found for ${PACKAGE_NAME}`);
		process.exit(1);
	}
	content = content.replace(packageRegex, `$1${newVersion}$2`);
	writeFileSync(cargoLockPath, content, "utf-8");
}

function prepareReadmeForRelease(newVersion) {
	const content = readFileSync(readmePath, "utf-8");
	try {
		return updateReadmeInstallVersion(content, newVersion);
	} catch (error) {
		console.error(`Error: ${error.message}`);
		process.exit(1);
	}
}

function ensureCleanMain() {
	const branch = runFile("git", ["branch", "--show-current"], { silent: true }).trim();
	if (branch !== RELEASE_BRANCH) {
		console.error(
			`Error: releases must be run from ${RELEASE_BRANCH}; current branch is ${branch || "(detached)"}.`
		);
		process.exit(1);
	}
	const status = runFile("git", ["status", "--porcelain"], { silent: true });
	if (status && status.trim()) {
		console.error("Error: Uncommitted changes detected. Commit or stash first.");
		console.error(status);
		process.exit(1);
	}
}

function ensureTools() {
	runFile("git", ["--version"], { silent: true });
	runFile(process.execPath, ["--version"], { silent: true });
	runFile("cargo", ["--version"], { silent: true });
	runFile("gh", ["--version"], { silent: true });
}

function ensureReleasePushTarget(releaseRemote) {
	const fetchUrls = runFile("git", ["remote", "get-url", "--all", releaseRemote], {
		silent: true,
	})
		.split(/\r?\n/)
		.filter(Boolean);
	const pushUrls = runFile("git", ["remote", "get-url", "--push", "--all", releaseRemote], {
		silent: true,
	})
		.split(/\r?\n/)
		.filter(Boolean);
	try {
		validateReleaseRemoteUrls(fetchUrls, pushUrls);
	} catch (error) {
		console.error(`Error: ${error.message}`);
		process.exit(1);
	}
}

function ensureSyncedMain(releaseRemote) {
	runFile(
		"git",
		[
			"fetch",
			releaseRemote,
			`refs/heads/${RELEASE_BRANCH}:refs/remotes/${releaseRemote}/${RELEASE_BRANCH}`,
		],
		{ silent: true }
	);
	const local = runFile("git", ["rev-parse", RELEASE_BRANCH], { silent: true }).trim();
	const remote = runFile("git", ["rev-parse", `${releaseRemote}/${RELEASE_BRANCH}`], {
		silent: true,
	}).trim();
	if (local !== remote) {
		console.error(
			`Error: ${RELEASE_BRANCH} must match ${releaseRemote}/${RELEASE_BRANCH}. Run git pull --ff-only first.`,
		);
		process.exit(1);
	}
}

function ensureTagAvailable(version, releaseRemote) {
	const tagExists = runFile("git", ["rev-parse", "-q", "--verify", `refs/tags/v${version}`], {
		silent: true,
		ignoreError: true,
	});
	if (tagExists) {
		console.error(`Error: tag v${version} already exists.`);
		process.exit(1);
	}

	const remoteTagExists = runFile(
		"git",
		["ls-remote", "--tags", releaseRemote, `refs/tags/v${version}`],
		{ silent: true },
	);
	if (remoteTagExists && remoteTagExists.trim()) {
		console.error(`Error: tag v${version} already exists on ${releaseRemote}.`);
		process.exit(1);
	}
}

function readValidatedChangelogForRelease(version) {
	const content = readFileSync(changelogPath, "utf-8");
	if (!content.includes("## [Unreleased]")) {
		console.error("Error: No [Unreleased] section found in CHANGELOG.md");
		process.exit(1);
	}
	if (content.includes(`## [${version}]`)) {
		console.error(`Error: CHANGELOG.md already contains a [${version}] section`);
		process.exit(1);
	}
	const unreleasedMatch = content.match(/## \[Unreleased\]\n([\s\S]*?)(?=\n## \[|$)/);
	if (
		!unreleasedMatch ||
		!unreleasedMatch[1].trim() ||
		unreleasedMatch[1].trim() === "_No unreleased changes._"
	) {
		console.error("Error: CHANGELOG.md has no release notes under [Unreleased]");
		process.exit(1);
	}
	return content;
}

function validateChangelogForRelease(version) {
	readValidatedChangelogForRelease(version);
}

function updateChangelogForRelease(version) {
	const date = new Date().toISOString().split("T")[0];
	let content = readValidatedChangelogForRelease(version);
	content = content.replace(/## \[Unreleased\]/, `## [${version}] - ${date}`);
	writeFileSync(changelogPath, content, "utf-8");
}

function validateCompatibilityForRelease() {
	try {
		requireCompatibilityFile(compatibilityPath);
	} catch (error) {
		console.error(`Error: ${error.message}`);
		process.exit(1);
	}
}

function addUnreleasedSection() {
	let content = readFileSync(changelogPath, "utf-8");
	const original = content;
	content = content.replace("# Changelog\n\n", "# Changelog\n\n## [Unreleased]\n\n_No unreleased changes._\n\n");
	if (content === original) {
		console.error("Error: Could not add [Unreleased] section to CHANGELOG.md");
		process.exit(1);
	}
	writeFileSync(changelogPath, content, "utf-8");
}

function runReleasePreflight() {
	const testPaths = readdirSync(join(ROOT, "scripts"), { withFileTypes: true })
		.filter((entry) => entry.isFile() && entry.name.endsWith(".test.mjs"))
		.map((entry) => join("scripts", entry.name))
		.sort();
	for (const [command, args] of releasePreflightCommands(process.execPath, testPaths)) {
		runFile(command, args);
	}
}

function failRelease(message) {
	console.error(`Error: ${message}`);
	process.exit(1);
}

function discoverReleaseGateRun(releaseCommit) {
	const deadline = Date.now() + RELEASE_GATE_DISCOVERY_TIMEOUT_MS;
	while (Date.now() < deadline) {
		const output = runFile(
			"gh",
			[
				"run",
				"list",
				"--repo",
				RELEASE_REPOSITORY,
				"--workflow",
				RELEASE_WORKFLOW,
				"--event",
				"workflow_dispatch",
				"--commit",
				releaseCommit,
				"--limit",
				"10",
				"--json",
				"databaseId,headSha",
			],
			{ silent: true, timeout: 30_000 },
		);
		let runs;
		try {
			runs = JSON.parse(output);
		} catch (error) {
			failRelease(`GitHub returned malformed release-gate JSON: ${error.message}`);
		}
		const run = runs.find((candidate) => candidate.headSha === releaseCommit);
		if (run && Number.isSafeInteger(run.databaseId) && run.databaseId > 0) {
			return run.databaseId;
		}
		Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, RELEASE_GATE_POLL_MS);
	}
	failRelease(`timed out discovering the release-gate run for ${releaseCommit}`);
}

function runRemoteReleasePreflight(releaseCommit) {
	runFile("gh", [
		"workflow",
		"run",
		RELEASE_WORKFLOW,
		"--repo",
		RELEASE_REPOSITORY,
		"--ref",
		RELEASE_BRANCH,
		"-f",
		`expected_sha=${releaseCommit}`,
	]);
	const runId = discoverReleaseGateRun(releaseCommit);
	runFile("gh", [
		"run",
		"watch",
		String(runId),
		"--repo",
		RELEASE_REPOSITORY,
		"--exit-status",
	]);
	const report = JSON.parse(
		runFile(
			"gh",
			[
				"run",
				"view",
				String(runId),
				"--repo",
				RELEASE_REPOSITORY,
				"--json",
				"event,headSha,conclusion",
			],
			{ silent: true, timeout: 30_000 },
		),
	);
	if (
		report.event !== "workflow_dispatch" ||
		report.headSha !== releaseCommit ||
		report.conclusion !== "success"
	) {
		failRelease(`release-gate run ${runId} did not validate exact commit ${releaseCommit}`);
	}
}

function main(args = process.argv.slice(2), environment = process.env) {
	const releaseArg = args[0];
	if (
		args.length !== 1 ||
		!releaseArg ||
		(!BUMP_ARGS.has(releaseArg) && releaseArg !== "current" && !VERSION_ARG.test(releaseArg))
	) {
		console.error("Usage: node scripts/release.mjs <current|major|minor|patch|X.Y.Z>");
		process.exit(1);
	}

	let releaseRemote;
	try {
		releaseRemote = releaseRemoteFromEnvironment(environment);
	} catch (error) {
		console.error(`Error: ${error.message}`);
		process.exit(1);
	}

	const currentVersion = getVersion();
	const version =
		releaseArg === "current" ? currentVersion : bumpVersion(currentVersion, releaseArg);
	if (!VERSION_ARG.test(version)) {
		console.error(`Release version "${version}" must be stable semver (X.Y.Z)`);
		process.exit(1);
	}

	ensureCleanMain();
	ensureTools();
	ensureReleasePushTarget(releaseRemote);
	ensureSyncedMain(releaseRemote);
	ensureTagAvailable(version, releaseRemote);
	validateChangelogForRelease(version);
	validateCompatibilityForRelease();
	const updatedReadme = prepareReadmeForRelease(version);

	if (version !== currentVersion) {
		updateCargoTomlVersion(version);
		updateCargoLockVersion(version);
	}
	writeFileSync(readmePath, updatedReadme, "utf-8");
	updateChangelogForRelease(version);
	const compatibilityUpdated = updateCompatibilityForRelease(compatibilityPath, version);

	runReleasePreflight();

	const releaseCommitPaths = ["Cargo.toml", "CHANGELOG.md", "README.md"];
	if (existsSync(cargoLockPath)) {
		releaseCommitPaths.splice(1, 0, "Cargo.lock");
	}
	if (compatibilityUpdated) {
		releaseCommitPaths.push("CODEX_COMPATIBILITY.md");
	}
	runFile("git", ["add", ...releaseCommitPaths]);
	runFile("git", ["commit", "-m", `Release v${version}`]);
	runFile("git", ["push", releaseRemote, RELEASE_BRANCH]);
	const releaseCommit = runFile("git", ["rev-parse", "HEAD"], { silent: true }).trim();
	runRemoteReleasePreflight(releaseCommit);
	runFile("git", ["tag", "-a", `v${version}`, "-m", `codex-tamer v${version}`]);
	runFile("git", ["push", releaseRemote, `v${version}`]);

	addUnreleasedSection();
	runFile("git", ["add", "CHANGELOG.md"]);
	runFile("git", ["commit", "-m", "Prepare for next release"]);
	runFile("git", ["push", releaseRemote, RELEASE_BRANCH]);
}

const invokedPath = process.argv[1] ? resolve(process.argv[1]) : null;
if (invokedPath === fileURLToPath(import.meta.url)) {
	main();
}
