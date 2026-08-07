#!/usr/bin/env node

import {
	accessSync,
	chmodSync,
	constants,
	cpSync,
	existsSync,
	lstatSync,
	mkdirSync,
	readFileSync,
	readdirSync,
	realpathSync,
	renameSync,
	rmSync,
} from "node:fs";
import { homedir } from "node:os";
import {
	basename,
	dirname,
	isAbsolute,
	join,
	relative,
	resolve,
	sep,
	win32,
	posix,
} from "node:path";
import { spawnSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import { fileURLToPath } from "node:url";

const CLI_NAME = "codex-tamer";
const SKILL_NAME = "codex-tamer";

function uniqueSibling(parent, name, suffix) {
	return join(parent, `.${name}.${process.pid}.${randomUUID()}.${suffix}`);
}

function assertRegularFile(path, label) {
	if (!existsSync(path)) {
		throw new Error(`${label} is missing: ${path}`);
	}
	const stat = lstatSync(path);
	if (stat.isSymbolicLink()) {
		throw new Error(`${label} must not be a symbolic link: ${path}`);
	}
	if (!stat.isFile()) {
		throw new Error(`${label} must be a regular file: ${path}`);
	}
}

function runtimeTarget(platform, arch) {
	const key = `${platform}-${arch}`;
	const targets = {
		"linux-x64": "linux-x86_64",
		"linux-arm64": "linux-aarch64",
		"darwin-arm64": "macos-aarch64",
		"darwin-x64": "macos-x86_64",
		"win32-x64": "windows-x86_64",
	};
	const target = targets[key];
	if (!target) {
		throw new Error(`unsupported current platform: ${key}`);
	}
	return target;
}

function readBundleManifest(bundleRoot, platform, arch) {
	const manifestPath = join(bundleRoot, "manifest.json");
	assertRegularFile(manifestPath, "bundle manifest");
	if (lstatSync(manifestPath).size > 64 * 1024) {
		throw new Error("bundle manifest exceeds 64 KiB");
	}
	let manifest;
	try {
		manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
	} catch (error) {
		throw new Error(`bundle manifest is not valid JSON: ${error.message}`);
	}
	if (!manifest || typeof manifest !== "object" || Array.isArray(manifest)) {
		throw new Error("bundle manifest must be a JSON object");
	}
	const currentTarget = runtimeTarget(platform, arch);
	if (manifest.target !== currentTarget) {
		throw new Error(
			`bundle target ${String(manifest.target)} does not match current platform ${currentTarget}`,
		);
	}
	const binaryName = platform === "win32" ? `${CLI_NAME}.exe` : CLI_NAME;
	const expected = {
		name: CLI_NAME,
		binary: `bin/${binaryName}`,
		skill: `skills/${SKILL_NAME}`,
		installer: "install.mjs",
	};
	for (const [field, value] of Object.entries(expected)) {
		if (manifest[field] !== value) {
			throw new Error(`bundle manifest ${field} must be ${value}`);
		}
	}
	if (
		typeof manifest.version !== "string" ||
		!/^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$/.test(manifest.version)
	) {
		throw new Error("bundle manifest version must be valid semver");
	}
	return manifest;
}

function copyRegularDirectory(source, destination, label) {
	const sourceStat = lstatSync(source);
	if (sourceStat.isSymbolicLink()) {
		throw new Error(`${label} must not contain a symbolic link: ${source}`);
	}
	if (!sourceStat.isDirectory()) {
		throw new Error(`${label} must be a directory: ${source}`);
	}
	mkdirSync(destination, { recursive: true });
	for (const entry of readdirSync(source, { withFileTypes: true })) {
		const sourcePath = join(source, entry.name);
		const destinationPath = join(destination, entry.name);
		const stat = lstatSync(sourcePath);
		if (stat.isSymbolicLink()) {
			throw new Error(`${label} must not contain a symbolic link: ${sourcePath}`);
		}
		if (stat.isDirectory()) {
			copyRegularDirectory(sourcePath, destinationPath, label);
		} else if (stat.isFile()) {
			cpSync(sourcePath, destinationPath, { preserveTimestamps: true });
		} else {
			throw new Error(`${label} contains an unsupported file type: ${sourcePath}`);
		}
	}
}

function validateExistingDestination(path, expectedType) {
	if (!existsSync(path)) {
		return;
	}
	const stat = lstatSync(path);
	if (stat.isSymbolicLink()) {
		throw new Error(`refusing to replace symbolic link: ${path}`);
	}
	if (expectedType === "file" && !stat.isFile()) {
		throw new Error(`refusing to replace non-file destination: ${path}`);
	}
	if (expectedType === "directory" && !stat.isDirectory()) {
		throw new Error(`refusing to replace non-directory destination: ${path}`);
	}
}

function activateStagedPath(stagedPath, destinationPath, expectedType) {
	validateExistingDestination(destinationPath, expectedType);
	const backupPath = existsSync(destinationPath)
		? uniqueSibling(dirname(destinationPath), basename(destinationPath), "backup")
		: null;
	if (backupPath) {
		renameSync(destinationPath, backupPath);
	}
	try {
		renameSync(stagedPath, destinationPath);
	} catch (error) {
		if (backupPath && existsSync(backupPath)) {
			renameSync(backupPath, destinationPath);
		}
		throw error;
	}
	return backupPath;
}

function rollbackActivatedPath(destinationPath, backupPath) {
	if (existsSync(destinationPath)) {
		rmSync(destinationPath, { recursive: true, force: true });
	}
	if (backupPath && existsSync(backupPath)) {
		renameSync(backupPath, destinationPath);
	}
}

export function rollbackActivatedPaths(activatedPaths, rollbackPath = rollbackActivatedPath) {
	const errors = [];
	for (const { destinationPath, backupPath } of activatedPaths) {
		try {
			rollbackPath(destinationPath, backupPath);
		} catch (error) {
			errors.push(error);
		}
	}
	if (errors.length > 0) {
		throw new AggregateError(errors, "failed to fully roll back installation");
	}
}

function removeBackup(backupPath) {
	if (backupPath && existsSync(backupPath)) {
		rmSync(backupPath, { recursive: true, force: true });
	}
}

export function pathsOverlap(first, second) {
	const canonicalize = (path) => {
		let current = resolve(path);
		const missingSegments = [];
		while (!existsSync(current)) {
			const parent = dirname(current);
			if (parent === current) {
				break;
			}
			missingSegments.unshift(basename(current));
			current = parent;
		}
		const canonicalParent = existsSync(current) ? realpathSync(current) : current;
		return resolve(canonicalParent, ...missingSegments);
	};
	const contains = (parent, child) => {
		const relation = relative(canonicalize(parent), canonicalize(child));
		return (
			relation === "" ||
			(!isAbsolute(relation) && relation !== ".." && !relation.startsWith(`..${sep}`))
		);
	};
	return contains(first, second) || contains(second, first);
}

function resolveCommandOnPath(pathValue, platform) {
	const pathDelimiter = platform === "win32" ? win32.delimiter : posix.delimiter;
	const pathApi = platform === "win32" ? win32 : posix;
	const names =
		platform === "win32"
			? ["codex-tamer.com", "codex-tamer.exe", "codex-tamer.bat", "codex-tamer.cmd"]
			: [CLI_NAME];
	for (const entry of String(pathValue ?? "")
		.split(pathDelimiter)
		.filter(Boolean)) {
		for (const name of names) {
			const candidate = pathApi.resolve(entry, name);
			try {
				const stat = lstatSync(candidate);
				if (!stat.isFile() && !stat.isSymbolicLink()) {
					continue;
				}
				if (platform !== "win32") {
					accessSync(candidate, constants.X_OK);
				}
				return candidate;
			} catch {
				// Continue to the next PATH candidate.
			}
		}
	}
	return null;
}

export function defaultInstallDirectories({
	platform = process.platform,
	home = homedir(),
	localAppData = process.env.LOCALAPPDATA,
} = {}) {
	if (!home) {
		throw new Error("cannot determine the user home directory");
	}
	const pathApi = platform === "win32" ? win32 : posix;
	const skillsDir = pathApi.join(home, ".agents", "skills");
	const binDir =
		platform === "win32"
			? pathApi.join(localAppData || pathApi.join(home, "AppData", "Local"), CLI_NAME, "bin")
			: pathApi.join(home, ".local", "bin");
	return { binDir, skillsDir };
}

export function verifyInstalledBinary(binaryPath) {
	const result = spawnSync(binaryPath, ["--version"], {
		encoding: "utf8",
		stdio: ["ignore", "pipe", "pipe"],
		timeout: 15_000,
	});
	if (result.error) {
		throw new Error(`failed to execute installed binary: ${result.error.message}`);
	}
	if (result.status !== 0) {
		const detail = String(result.stderr || result.stdout || "").trim();
		throw new Error(
			`installed binary failed version verification with exit ${result.status}${detail ? `: ${detail}` : ""}`,
		);
	}
	return String(result.stdout || result.stderr || "").trim();
}

export function installBundle({
	bundleRoot,
	binDir,
	skillsDir,
	platform = process.platform,
	arch = process.arch,
	pathValue = process.env.PATH,
	verifyBinary = verifyInstalledBinary,
	rollbackPath = rollbackActivatedPath,
}) {
	if (!bundleRoot || !binDir || !skillsDir) {
		throw new Error("bundleRoot, binDir, and skillsDir are required");
	}
	const manifest = readBundleManifest(bundleRoot, platform, arch);
	const binaryName = platform === "win32" ? `${CLI_NAME}.exe` : CLI_NAME;
	const sourceBinary = join(bundleRoot, "bin", binaryName);
	const sourceSkill = join(bundleRoot, "skills", SKILL_NAME);
	assertRegularFile(sourceBinary, "bundled binary");
	assertRegularFile(join(sourceSkill, "SKILL.md"), "bundled skill manifest");

	const destinationBinary = join(binDir, binaryName);
	const destinationSkill = join(skillsDir, SKILL_NAME);
	if (
		pathsOverlap(bundleRoot, destinationBinary) ||
		pathsOverlap(bundleRoot, destinationSkill)
	) {
		throw new Error("install destination overlaps the extracted bundle");
	}
	mkdirSync(binDir, { recursive: true });
	mkdirSync(skillsDir, { recursive: true });
	const stagedBinary = uniqueSibling(binDir, binaryName, "staged");
	const stagedSkill = uniqueSibling(skillsDir, SKILL_NAME, "staged");

	let binaryBackup = null;
	let skillBackup = null;
	try {
		cpSync(sourceBinary, stagedBinary, { preserveTimestamps: true });
		if (platform !== "win32") {
			chmodSync(stagedBinary, 0o755);
		}
		copyRegularDirectory(sourceSkill, stagedSkill, "bundled skill");

		binaryBackup = activateStagedPath(stagedBinary, destinationBinary, "file");
			try {
				skillBackup = activateStagedPath(stagedSkill, destinationSkill, "directory");
			} catch (error) {
				try {
					rollbackActivatedPaths(
						[{ destinationPath: destinationBinary, backupPath: binaryBackup }],
						rollbackPath,
					);
				} catch (rollbackError) {
					throw new AggregateError(
						[error, ...rollbackError.errors],
						`${error.message}; failed to fully roll back installation`,
					);
				}
				binaryBackup = null;
				throw error;
		}

		let version;
		try {
			version = verifyBinary(destinationBinary);
			const versionMatch = String(version).match(/^codex-tamer\s+(\S+)$/);
			if (!versionMatch) {
				throw new Error(`installed binary returned an invalid version: ${String(version)}`);
			}
			if (versionMatch[1] !== manifest.version) {
				throw new Error(
					`binary version ${versionMatch[1]} does not match manifest version ${manifest.version}`,
				);
			}
			} catch (error) {
				try {
					rollbackActivatedPaths([
						{ destinationPath: destinationSkill, backupPath: skillBackup },
						{ destinationPath: destinationBinary, backupPath: binaryBackup },
					], rollbackPath);
				} catch (rollbackError) {
					throw new AggregateError(
						[error, ...rollbackError.errors],
						`${error.message}; failed to fully roll back installation`,
					);
				}
				skillBackup = null;
				binaryBackup = null;
			throw error;
		}

		removeBackup(binaryBackup);
		removeBackup(skillBackup);
		binaryBackup = null;
		skillBackup = null;
		const resolvedPath = resolveCommandOnPath(pathValue, platform);
		const normalizePath = (path) => {
			const canonical = realpathSync(path);
			return platform === "win32" ? canonical.toLowerCase() : canonical;
		};
		const onPath =
			resolvedPath !== null &&
			normalizePath(resolvedPath) === normalizePath(destinationBinary);
		const pathHint = onPath
			? null
			: resolvedPath
				? `${resolvedPath} takes precedence on PATH; put ${binDir} before its directory, then restart Codex or the calling Agent.`
				: `Add ${binDir} to PATH, then restart Codex or the calling Agent.`;
		return {
			ok: true,
			bundle: { version: manifest.version, target: manifest.target },
			binary: {
				path: destinationBinary,
				version,
				onPath,
				resolvedPath,
				pathHint,
			},
			skill: { path: destinationSkill },
		};
	} finally {
		if (existsSync(stagedBinary)) {
			rmSync(stagedBinary, { force: true });
		}
		if (existsSync(stagedSkill)) {
			rmSync(stagedSkill, { recursive: true, force: true });
		}
	}
}

function usage() {
	return [
		"Install the codex-tamer binary and Agent Skill from an extracted release bundle.",
		"",
		"Usage: node install.mjs [--bin-dir PATH] [--skills-dir PATH] [--json]",
		"",
		"The Skill always invokes `codex-tamer` through PATH; it is never rewritten.",
	].join("\n");
}

function parseArguments(args) {
	const parsed = { json: false, binDir: null, skillsDir: null, help: false };
	for (let index = 0; index < args.length; index += 1) {
		const argument = args[index];
		if (argument === "--json") {
			parsed.json = true;
		} else if (argument === "--help" || argument === "-h") {
			parsed.help = true;
		} else if (argument === "--bin-dir" || argument === "--skills-dir") {
			const value = args[index + 1];
			if (!value || value.startsWith("-")) {
				throw new Error(`${argument} requires a path`);
			}
			parsed[argument === "--bin-dir" ? "binDir" : "skillsDir"] = resolve(value);
			index += 1;
		} else {
			throw new Error(`unknown argument: ${argument}`);
		}
	}
	return parsed;
}

function main() {
	try {
		const args = parseArguments(process.argv.slice(2));
		if (args.help) {
			console.log(usage());
			return;
		}
		const defaults = defaultInstallDirectories();
		const result = installBundle({
			bundleRoot: dirname(fileURLToPath(import.meta.url)),
			binDir: args.binDir || defaults.binDir,
			skillsDir: args.skillsDir || defaults.skillsDir,
		});
		if (args.json) {
			console.log(JSON.stringify(result, null, 2));
			return;
		}
		console.log(`Installed ${result.binary.version} at ${result.binary.path}`);
		console.log(`Installed Skill at ${result.skill.path}`);
		if (result.binary.pathHint) {
			console.warn(result.binary.pathHint);
		}
	} catch (error) {
		console.error(`codex-tamer install failed: ${error.message}`);
		process.exitCode = 1;
	}
}

const invokedPath = process.argv[1] ? resolve(process.argv[1]) : null;
if (invokedPath === fileURLToPath(import.meta.url)) {
	main();
}

export { copyRegularDirectory };
