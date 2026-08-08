#!/usr/bin/env node

import {
	closeSync,
	chmodSync,
	cpSync,
	existsSync,
	lstatSync,
	mkdirSync,
	openSync,
	readSync,
	readFileSync,
	renameSync,
	rmSync,
	writeFileSync,
} from "node:fs";
import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { basename, dirname, join, resolve, win32 } from "node:path";
import { fileURLToPath } from "node:url";

import { copyRegularDirectory, pathsOverlap } from "./install-bundle.mjs";

const SUPPORTED_TARGETS = new Set([
	"linux-x86_64",
	"linux-aarch64",
	"macos-aarch64",
	"macos-x86_64",
	"windows-x86_64",
]);

function readPackageVersion(root) {
	const cargoToml = readFileSync(join(root, "Cargo.toml"), "utf8");
	const match = cargoToml.match(/\[package\][\s\S]*?\nversion\s*=\s*"([^"]+)"/);
	if (!match || !/^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$/.test(match[1])) {
		throw new Error("could not read a valid package version from Cargo.toml");
	}
	return match[1];
}

function assertSafeTarget(target) {
	if (!SUPPORTED_TARGETS.has(target)) {
		throw new Error(`unsupported or unsafe target label: ${target}`);
	}
}

function assertRegularFile(path, label) {
	if (!existsSync(path)) {
		throw new Error(`${label} is missing: ${path}`);
	}
	const stat = lstatSync(path);
	if (stat.isSymbolicLink() || !stat.isFile()) {
		throw new Error(`${label} must be a regular file: ${path}`);
	}
}

function readBinaryPrefix(path) {
	const size = Math.min(lstatSync(path).size, 64 * 1024);
	const buffer = Buffer.alloc(size);
	const descriptor = openSync(path, "r");
	try {
		const bytesRead = readSync(descriptor, buffer, 0, buffer.length, 0);
		return buffer.subarray(0, bytesRead);
	} finally {
		closeSync(descriptor);
	}
}

function detectBinaryTarget(path) {
	const binary = readBinaryPrefix(path);
	if (
		binary.length >= 20 &&
		binary[0] === 0x7f &&
		binary.subarray(1, 4).toString("ascii") === "ELF" &&
		binary[5] === 1
	) {
		const machine = binary.readUInt16LE(18);
		return machine === 62 ? "linux-x86_64" : machine === 183 ? "linux-aarch64" : null;
	}
	if (
		binary.length >= 8 &&
		binary[0] === 0xcf &&
		binary[1] === 0xfa &&
		binary[2] === 0xed &&
		binary[3] === 0xfe
	) {
		const cpuType = binary.readUInt32LE(4);
		return cpuType === 0x01000007
			? "macos-x86_64"
			: cpuType === 0x0100000c
				? "macos-aarch64"
				: null;
	}
	if (binary.length >= 64 && binary[0] === 0x4d && binary[1] === 0x5a) {
		const peOffset = binary.readUInt32LE(0x3c);
		if (
			peOffset + 6 <= binary.length &&
			binary.subarray(peOffset, peOffset + 4).equals(Buffer.from([0x50, 0x45, 0, 0]))
		) {
			return binary.readUInt16LE(peOffset + 4) === 0x8664 ? "windows-x86_64" : null;
		}
	}
	return null;
}

export function verifyReleaseBinary(binary, expectedVersion) {
	const result = spawnSync(binary, ["--version"], {
		encoding: "utf8",
		stdio: ["ignore", "pipe", "pipe"],
		timeout: 15_000,
	});
	const expected = `codex-tamer ${expectedVersion}`;
	if (result.error) {
		throw new Error(
			`release binary failed identity check; expected ${expected}: ${result.error.message}`,
		);
	}
	const reported = String(result.stdout || result.stderr || "").trim();
	if (result.status !== 0 || reported !== expected) {
		const detail = reported || `exit ${String(result.status)}`;
		throw new Error(`release binary failed identity check; expected ${expected}, received ${detail}`);
	}
	return reported;
}

export function buildReleaseBundle({
	root,
	binary,
	target,
	outDir,
	verifyBinary = verifyReleaseBinary,
}) {
	if (!root || !binary || !target || !outDir) {
		throw new Error("root, binary, target, and outDir are required");
	}
	assertSafeTarget(target);
	assertRegularFile(binary, "release binary");
	const detectedTarget = detectBinaryTarget(binary);
	if (detectedTarget !== target) {
		throw new Error(
			`binary format ${detectedTarget || "unknown"} does not match target ${target}`,
		);
	}
	const version = readPackageVersion(root);
	verifyBinary(binary, version);
	assertRegularFile(join(root, "scripts", "install-bundle.mjs"), "bundle installer");
	assertRegularFile(join(root, "skills", "codex-tamer", "SKILL.md"), "Skill manifest");
	assertRegularFile(join(root, "LICENSE"), "license");
	assertRegularFile(join(root, "README.md"), "README");

	const skillSource = join(root, "skills", "codex-tamer");
	if (pathsOverlap(skillSource, outDir)) {
		throw new Error("output directory overlaps the Skill source");
	}
	const bundleName = `codex-tamer-${version}-${target}`;
	const bundlePath = join(outDir, bundleName);
	if (existsSync(bundlePath)) {
		throw new Error(`bundle already exists: ${bundlePath}`);
	}
	mkdirSync(outDir, { recursive: true });
	const stagedPath = join(outDir, `.${bundleName}.${process.pid}.staged`);
	if (existsSync(stagedPath)) {
		throw new Error(`staging path already exists: ${stagedPath}`);
	}

	const binaryName = target.startsWith("windows-") ? "codex-tamer.exe" : "codex-tamer";
	try {
		mkdirSync(join(stagedPath, "bin"), { recursive: true });
		cpSync(binary, join(stagedPath, "bin", binaryName), { preserveTimestamps: true });
		if (!target.startsWith("windows-")) {
			chmodSync(join(stagedPath, "bin", binaryName), 0o755);
		}
		copyRegularDirectory(skillSource, join(stagedPath, "skills", "codex-tamer"), "Skill");
		cpSync(join(root, "scripts", "install-bundle.mjs"), join(stagedPath, "install.mjs"));
		cpSync(join(root, "LICENSE"), join(stagedPath, "LICENSE"));
		cpSync(join(root, "README.md"), join(stagedPath, "README.md"));
		const manifest = {
			name: "codex-tamer",
			version,
			target,
			binary: `bin/${binaryName}`,
			skill: "skills/codex-tamer",
			installer: "install.mjs",
		};
		writeFileSync(join(stagedPath, "manifest.json"), `${JSON.stringify(manifest, null, 2)}\n`);
		renameSync(stagedPath, bundlePath);
	} catch (error) {
		if (existsSync(stagedPath)) {
			rmSync(stagedPath, { recursive: true, force: true });
		}
		throw error;
	}

	return { version, target, bundleName, bundlePath, binaryName };
}

export function releaseArchiveCommand(
	target,
	archiveName,
	bundleName,
	platform = process.platform,
	environment = process.env,
) {
	if (!target.startsWith("windows-")) {
		return { command: "tar", args: ["-czf", archiveName, bundleName] };
	}
	if (platform !== "win32") {
		return { command: "zip", args: ["-qr", archiveName, bundleName] };
	}
	const systemRoot = environment.SystemRoot || environment.SYSTEMROOT;
	if (!systemRoot || !win32.isAbsolute(systemRoot)) {
		throw new Error("SystemRoot must be an absolute path to create a Windows ZIP archive");
	}
	return {
		command: win32.join(systemRoot, "System32", "tar.exe"),
		args: ["-a", "-cf", archiveName, bundleName],
	};
}

function sha256File(path) {
	const hash = createHash("sha256");
	const descriptor = openSync(path, "r");
	try {
		const buffer = Buffer.alloc(64 * 1024);
		let bytesRead;
		while ((bytesRead = readSync(descriptor, buffer, 0, buffer.length, null)) > 0) {
			hash.update(buffer.subarray(0, bytesRead));
		}
	} finally {
		closeSync(descriptor);
	}
	return hash.digest("hex");
}

export function createReleaseArchive(bundle) {
	const { bundlePath, bundleName, target } = bundle;
	if (!bundlePath || !bundleName || !target) {
		throw new Error("bundlePath, bundleName, and target are required");
	}
	const outDir = dirname(bundlePath);
	const archiveName = `${bundleName}${target.startsWith("windows-") ? ".zip" : ".tar.gz"}`;
	const archivePath = join(outDir, archiveName);
	const checksumPath = `${archivePath}.sha256`;
	if (existsSync(archivePath) || existsSync(checksumPath)) {
		throw new Error(`release archive already exists: ${archivePath}`);
	}

	const { command, args } = releaseArchiveCommand(target, archiveName, bundleName);
	const archived = spawnSync(command, args, {
		cwd: outDir,
		encoding: "utf8",
		stdio: ["ignore", "pipe", "pipe"],
	});
	if (archived.error || archived.status !== 0) {
		if (existsSync(archivePath)) {
			rmSync(archivePath, { force: true });
		}
		const detail = archived.error?.message || String(archived.stderr || "").trim();
		throw new Error(`failed to create release archive${detail ? `: ${detail}` : ""}`);
	}
	if (
		target.startsWith("windows-") &&
		!readBinaryPrefix(archivePath).subarray(0, 4).equals(Buffer.from("PK\u0003\u0004"))
	) {
		rmSync(archivePath, { force: true });
		throw new Error("failed to create release archive: output is not a ZIP file");
	}
	const sha256 = sha256File(archivePath);
	writeFileSync(checksumPath, `${sha256}  ${basename(archivePath)}\n`);
	return { ...bundle, archiveName, archivePath, checksumPath, sha256 };
}

function parseArguments(args) {
	const parsed = { binary: null, target: null, outDir: null };
	for (let index = 0; index < args.length; index += 2) {
		const key = args[index];
		const value = args[index + 1];
		if (!value || !["--binary", "--target", "--out-dir"].includes(key)) {
			throw new Error(
				"Usage: node scripts/package-release.mjs --binary PATH --target TARGET --out-dir DIR",
			);
		}
		parsed[key === "--binary" ? "binary" : key === "--target" ? "target" : "outDir"] =
			key === "--target" ? value : resolve(value);
	}
	return parsed;
}

function main() {
	try {
		const args = parseArguments(process.argv.slice(2));
		const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
		const bundle = buildReleaseBundle({ root, ...args });
		const result = createReleaseArchive(bundle);
		console.log(JSON.stringify(result, null, 2));
	} catch (error) {
		console.error(`codex-tamer packaging failed: ${error.message}`);
		process.exitCode = 1;
	}
}

const invokedPath = process.argv[1] ? resolve(process.argv[1]) : null;
if (invokedPath === fileURLToPath(import.meta.url)) {
	main();
}
