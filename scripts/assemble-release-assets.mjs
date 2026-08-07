#!/usr/bin/env node

import { createHash } from "node:crypto";
import {
	closeSync,
	existsSync,
	lstatSync,
	openSync,
	readdirSync,
	readFileSync,
	readSync,
	renameSync,
	rmSync,
	writeFileSync,
} from "node:fs";
import { basename, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const TARGETS = [
	["linux-x86_64", ".tar.gz"],
	["linux-aarch64", ".tar.gz"],
	["macos-aarch64", ".tar.gz"],
	["macos-x86_64", ".tar.gz"],
	["windows-x86_64", ".zip"],
];

function assertRegularFile(path, label) {
	if (!existsSync(path)) {
		throw new Error(`${label} is missing: ${path}`);
	}
	const stat = lstatSync(path);
	if (stat.isSymbolicLink() || !stat.isFile()) {
		throw new Error(`${label} must be a regular file: ${path}`);
	}
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

function readAdjacentChecksum(checksumPath, archiveName) {
	assertRegularFile(checksumPath, "adjacent checksum");
	if (lstatSync(checksumPath).size > 4 * 1024) {
		throw new Error(`malformed checksum file: ${checksumPath}`);
	}
	const content = readFileSync(checksumPath, "utf8");
	const match = content.match(/^([0-9a-f]{64})  ([^\r\n]+)\r?\n?$/);
	if (!match || match[2] !== archiveName) {
		throw new Error(`malformed checksum file: ${checksumPath}`);
	}
	return match[1];
}

export function assembleReleaseAssets({ directory, version }) {
	if (!directory || !version) {
		throw new Error("directory and version are required");
	}
	if (!/^\d+\.\d+\.\d+$/.test(version)) {
		throw new Error("version must be a valid package version (X.Y.Z)");
	}
	const resolvedDirectory = resolve(directory);
	if (!existsSync(resolvedDirectory) || !lstatSync(resolvedDirectory).isDirectory()) {
		throw new Error(`release asset directory is missing: ${resolvedDirectory}`);
	}
	const sha256SumsPath = join(resolvedDirectory, "SHA256SUMS");
	if (existsSync(sha256SumsPath)) {
		throw new Error(`refusing to overwrite existing SHA256SUMS: ${sha256SumsPath}`);
	}
	const expectedNames = new Set(
		TARGETS.flatMap(([target, extension]) => {
			const archiveName = `codex-tamer-${version}-${target}${extension}`;
			return [archiveName, `${archiveName}.sha256`];
		}),
	);
	for (const entry of readdirSync(resolvedDirectory, { withFileTypes: true })) {
		if (!expectedNames.has(entry.name)) {
			throw new Error(`unexpected release asset: ${entry.name}`);
		}
	}

	const verified = [];
	for (const [target, extension] of TARGETS) {
		const archiveName = `codex-tamer-${version}-${target}${extension}`;
		const archivePath = join(resolvedDirectory, archiveName);
		const checksumPath = `${archivePath}.sha256`;
		assertRegularFile(archivePath, `missing release archive for ${target}`);
		const expected = readAdjacentChecksum(checksumPath, archiveName);
		const actual = sha256File(archivePath);
		if (actual !== expected) {
			throw new Error(`SHA256 mismatch for ${archiveName}: expected ${expected}, received ${actual}`);
		}
		verified.push({ archivePath, checksumPath, archiveName, sha256: actual });
	}

	const lines = verified
		.map(({ archiveName, sha256 }) => `${sha256}  ${archiveName}`)
		.sort()
		.join("\n");
	const stagedPath = join(resolvedDirectory, `.SHA256SUMS.${process.pid}.staged`);
	try {
		writeFileSync(stagedPath, `${lines}\n`, { flag: "wx" });
		renameSync(stagedPath, sha256SumsPath);
	} finally {
		if (existsSync(stagedPath)) {
			rmSync(stagedPath, { force: true });
		}
	}

	return {
		version,
		archives: verified.map(({ archivePath }) => archivePath),
		checksums: verified.map(({ checksumPath }) => checksumPath),
		sha256SumsPath,
	};
}

function parseArguments(args) {
	const parsed = { directory: null, version: null };
	for (let index = 0; index < args.length; index += 2) {
		const key = args[index];
		const value = args[index + 1];
		if (!value || !["--dir", "--version"].includes(key)) {
			throw new Error(
				"Usage: node scripts/assemble-release-assets.mjs --dir DIR --version X.Y.Z",
			);
		}
		parsed[key === "--dir" ? "directory" : "version"] =
			key === "--dir" ? resolve(value) : value;
	}
	return parsed;
}

function main() {
	try {
		const result = assembleReleaseAssets(parseArguments(process.argv.slice(2)));
		console.log(JSON.stringify(result, null, 2));
	} catch (error) {
		console.error(`codex-tamer release asset assembly failed: ${error.message}`);
		process.exitCode = 1;
	}
}

const invokedPath = process.argv[1] ? resolve(process.argv[1]) : null;
if (invokedPath === fileURLToPath(import.meta.url)) {
	main();
}
