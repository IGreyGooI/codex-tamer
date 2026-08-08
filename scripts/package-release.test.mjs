import assert from "node:assert/strict";
import {
	chmodSync,
	existsSync,
	lstatSync,
	mkdtempSync,
	mkdirSync,
	readFileSync,
	rmSync,
	symlinkSync,
	writeFileSync,
} from "node:fs";
import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import { basename, dirname, join } from "node:path";
import test from "node:test";

import {
	buildReleaseBundle as buildReleaseBundleImplementation,
	createReleaseArchive,
	releaseArchiveCommand,
} from "./package-release.mjs";

function withTempDirectory(run) {
	const directory = mkdtempSync(join(tmpdir(), "codex-tamer-package-test-"));
	try {
		return run(directory);
	} finally {
		rmSync(directory, { recursive: true, force: true });
	}
}

function createRepositoryFixture(root) {
	mkdirSync(join(root, "scripts"), { recursive: true });
	mkdirSync(join(root, "skills", "codex-tamer", "agents"), { recursive: true });
	writeFileSync(
		join(root, "Cargo.toml"),
		'[package]\nname = "codex-tamer"\nversion = "1.2.3"\n',
	);
	writeFileSync(join(root, "LICENSE"), "MIT\n");
	writeFileSync(join(root, "scripts", "install-bundle.mjs"), "// installer\n");
	writeFileSync(
		join(root, "skills", "codex-tamer", "SKILL.md"),
		"---\nname: codex-tamer\n---\nRun `codex-tamer`.\n",
	);
	writeFileSync(
		join(root, "skills", "codex-tamer", "agents", "openai.yaml"),
		"interface:\n  display_name: Codex Tamer\n",
	);
}

function binaryFixture(target) {
	if (target.startsWith("linux-")) {
		const buffer = Buffer.alloc(64);
		buffer.set([0x7f, 0x45, 0x4c, 0x46, 2, 1, 1]);
		buffer.writeUInt16LE(target.endsWith("aarch64") ? 183 : 62, 18);
		return buffer;
	}
	if (target.startsWith("macos-")) {
		const buffer = Buffer.alloc(64);
		buffer.set([0xcf, 0xfa, 0xed, 0xfe]);
		buffer.writeUInt32LE(target.endsWith("aarch64") ? 0x0100000c : 0x01000007, 4);
		return buffer;
	}
	const buffer = Buffer.alloc(256);
	buffer.set([0x4d, 0x5a]);
	buffer.writeUInt32LE(128, 0x3c);
	buffer.set([0x50, 0x45, 0, 0], 128);
	buffer.writeUInt16LE(0x8664, 132);
	return buffer;
}

function writeBinaryFixture(path, target) {
	const content = binaryFixture(target);
	writeFileSync(path, content);
	chmodSync(path, 0o755);
	return content;
}

function buildReleaseBundle(options) {
	return buildReleaseBundleImplementation({
		...options,
		verifyBinary: () => "codex-tamer 1.2.3",
	});
}

function nativeTarget() {
	const target = {
		"linux-x64": "linux-x86_64",
		"linux-arm64": "linux-aarch64",
		"darwin-x64": "macos-x86_64",
		"darwin-arm64": "macos-aarch64",
		"win32-x64": "windows-x86_64",
	}[`${process.platform}-${process.arch}`];
	assert.ok(target, `unsupported test platform: ${process.platform}-${process.arch}`);
	return target;
}

function compileNativeCliFixture(directory, version) {
	const source = join(directory, "fixture.rs");
	const binary = join(directory, process.platform === "win32" ? "codex-tamer.exe" : "codex-tamer");
	writeFileSync(source, `fn main() { println!("codex-tamer ${version}"); }\n`);
	const compiled = spawnSync("rustc", [source, "-o", binary], { encoding: "utf8" });
	assert.equal(compiled.status, 0, compiled.stderr || compiled.error?.message);
	return binary;
}

function repositoryVersion() {
	const cargoToml = readFileSync(join(import.meta.dirname, "..", "Cargo.toml"), "utf8");
	const match = cargoToml.match(/\[package\][\s\S]*?\nversion\s*=\s*"([^"]+)"/);
	assert.ok(match, "repository Cargo.toml package version");
	return match[1];
}

test("builds a platform bundle containing the native CLI and unchanged skill", () => {
	withTempDirectory((directory) => {
		const target = nativeTarget();
		const binaryName = target.startsWith("windows-") ? "codex-tamer.exe" : "codex-tamer";
		const root = join(directory, "repo");
		const outDir = join(directory, "dist");
		const binary = join(directory, binaryName);
		createRepositoryFixture(root);
		const binaryContent = writeBinaryFixture(binary, target);

		const result = buildReleaseBundle({
			root,
			binary,
			target,
			outDir,
		});

		assert.equal(result.version, "1.2.3");
		assert.equal(result.bundleName, `codex-tamer-1.2.3-${target}`);
		assert.deepEqual(readFileSync(join(result.bundlePath, "bin", binaryName)), binaryContent);
		if (!target.startsWith("windows-")) {
			assert.equal(lstatSync(join(result.bundlePath, "bin", binaryName)).mode & 0o111, 0o111);
		}
		assert.equal(
			readFileSync(join(result.bundlePath, "skills", "codex-tamer", "SKILL.md"), "utf8"),
			"---\nname: codex-tamer\n---\nRun `codex-tamer`.\n",
		);
		assert.equal(readFileSync(join(result.bundlePath, "install.mjs"), "utf8"), "// installer\n");
		assert.equal(readFileSync(join(result.bundlePath, "LICENSE"), "utf8"), "MIT\n");
		assert.deepEqual(JSON.parse(readFileSync(join(result.bundlePath, "manifest.json"), "utf8")), {
			binary: "bin/codex-tamer",
			installer: "install.mjs",
			name: "codex-tamer",
			skill: "skills/codex-tamer",
			target,
			version: "1.2.3",
		});
	});
});

test("uses the exe suffix for Windows bundles", () => {
	withTempDirectory((directory) => {
		const root = join(directory, "repo");
		const binary = join(directory, "codex-tamer.exe");
		createRepositoryFixture(root);
		writeBinaryFixture(binary, "windows-x86_64");

		const result = buildReleaseBundle({
			root,
			binary,
			target: "windows-x86_64",
			outDir: join(directory, "dist"),
		});

		assert.equal(existsSync(join(result.bundlePath, "bin", "codex-tamer.exe")), true);
		assert.equal(result.binaryName, "codex-tamer.exe");
	});
});

test("rejects unsafe target labels", () => {
	withTempDirectory((directory) => {
		const root = join(directory, "repo");
		const binary = join(directory, "codex-tamer");
		createRepositoryFixture(root);
		writeBinaryFixture(binary, "linux-x86_64");

		assert.throws(
			() =>
				buildReleaseBundle({
					root,
					binary,
					target: "../../outside",
					outDir: join(directory, "dist"),
				}),
			/target label/i,
		);
		assert.equal(existsSync(join(directory, "outside")), false);
	});
});

test("refuses to overwrite an existing bundle directory", () => {
	withTempDirectory((directory) => {
		const root = join(directory, "repo");
		const outDir = join(directory, "dist");
		const binary = join(directory, "codex-tamer");
		createRepositoryFixture(root);
		writeBinaryFixture(binary, "linux-x86_64");
		mkdirSync(join(outDir, "codex-tamer-1.2.3-linux-x86_64"), { recursive: true });

		assert.throws(
			() => buildReleaseBundle({ root, binary, target: "linux-x86_64", outDir }),
			/already exists/i,
		);
	});
});

test("archives a Linux bundle and writes its SHA256", () => {
	withTempDirectory((directory) => {
		const root = join(directory, "repo");
		const binary = join(directory, "codex-tamer");
		createRepositoryFixture(root);
		writeBinaryFixture(binary, "linux-x86_64");
		const bundle = buildReleaseBundle({
			root,
			binary,
			target: "linux-x86_64",
			outDir: join(directory, "dist"),
		});

		const archived = createReleaseArchive(bundle);
		const expectedHash = createHash("sha256")
			.update(readFileSync(archived.archivePath))
			.digest("hex");
		assert.equal(archived.sha256, expectedHash);
		assert.equal(
			readFileSync(archived.checksumPath, "utf8"),
			`${expectedHash}  ${archived.archiveName}\n`,
		);
		const listed = spawnSync("tar", ["-tzf", basename(archived.archivePath)], {
			cwd: dirname(archived.archivePath),
			encoding: "utf8",
		});
		assert.equal(listed.status, 0, listed.stderr);
		const entries = listed.stdout.split(/\r?\n/);
		assert.ok(entries.includes(`${bundle.bundleName}/manifest.json`));
		assert.ok(entries.includes(`${bundle.bundleName}/skills/codex-tamer/SKILL.md`));
	});
});

test("uses native Windows bsdtar instead of Git Bash tar for ZIP archives", () => {
	assert.deepEqual(
		releaseArchiveCommand("windows-x86_64", "bundle.zip", "bundle", "win32", {
			SystemRoot: "C:\\Windows",
		}),
		{
			command: "C:\\Windows\\System32\\tar.exe",
			args: ["-a", "-cf", "bundle.zip", "bundle"],
		},
	);
	assert.throws(
		() => releaseArchiveCommand("windows-x86_64", "bundle.zip", "bundle", "win32", {}),
		/SystemRoot/i,
	);
});

test("archives a Windows bundle as ZIP", () => {
	withTempDirectory((directory) => {
		const root = join(directory, "repo");
		const binary = join(directory, "codex-tamer.exe");
		createRepositoryFixture(root);
		writeBinaryFixture(binary, "windows-x86_64");
		const bundle = buildReleaseBundle({
			root,
			binary,
			target: "windows-x86_64",
			outDir: join(directory, "dist"),
		});

		const archived = createReleaseArchive(bundle);
		assert.match(archived.archiveName, /\.zip$/);
		assert.deepEqual(readFileSync(archived.archivePath).subarray(0, 4), Buffer.from("PK\u0003\u0004"));
		const listed = spawnSync("unzip", ["-Z1", archived.archivePath], { encoding: "utf8" });
		assert.equal(listed.status, 0, listed.stderr);
		assert.match(listed.stdout, new RegExp(`^${bundle.bundleName}/bin/codex-tamer\\.exe$`, "m"));
	});
});

test("the packaging CLI builds an archive from the repository", () => {
	withTempDirectory((directory) => {
		const binary = compileNativeCliFixture(directory, repositoryVersion());
		const target = nativeTarget();
		const packaged = spawnSync(
			process.execPath,
			[
				join(import.meta.dirname, "package-release.mjs"),
				"--binary",
				binary,
				"--target",
				target,
				"--out-dir",
				join(directory, "dist"),
			],
			{ encoding: "utf8" },
		);

		assert.equal(packaged.status, 0, packaged.stderr);
		const result = JSON.parse(packaged.stdout);
		assert.equal(result.target, target);
		assert.equal(existsSync(result.archivePath), true);
		assert.equal(existsSync(result.checksumPath), true);
	});
});

test("the packaging CLI rejects incomplete arguments", () => {
	const rejected = spawnSync(
		process.execPath,
		[join(import.meta.dirname, "package-release.mjs"), "--binary"],
		{ encoding: "utf8" },
	);
	assert.equal(rejected.status, 1);
	assert.match(rejected.stderr, /Usage: node scripts\/package-release\.mjs/);
});

test("archive creation refuses to overwrite an existing archive", () => {
	withTempDirectory((directory) => {
		const root = join(directory, "repo");
		const binary = join(directory, "codex-tamer");
		createRepositoryFixture(root);
		writeBinaryFixture(binary, "linux-x86_64");
		const bundle = buildReleaseBundle({
			root,
			binary,
			target: "linux-x86_64",
			outDir: join(directory, "dist"),
		});
		writeFileSync(join(directory, "dist", `${bundle.bundleName}.tar.gz`), "existing\n");
		assert.throws(() => createReleaseArchive(bundle), /release archive already exists/);
	});
});

test("bundle creation rejects missing binaries and invalid package versions", () => {
	withTempDirectory((directory) => {
		const root = join(directory, "repo");
		createRepositoryFixture(root);
		const options = {
			root,
			binary: join(directory, "missing"),
			target: "linux-x86_64",
			outDir: join(directory, "dist"),
		};
		assert.throws(() => buildReleaseBundle(options), /release binary is missing/);
		writeBinaryFixture(options.binary, "linux-x86_64");
		writeFileSync(
			join(root, "Cargo.toml"),
			'[package]\nname = "codex-tamer"\nversion = "not-semver"\n',
		);
		assert.throws(() => buildReleaseBundle(options), /valid package version/);
	});
});

test("rejects output directories that overlap the Skill source", () => {
	withTempDirectory((directory) => {
		const root = join(directory, "repo");
		const binary = join(directory, "codex-tamer");
		createRepositoryFixture(root);
		writeBinaryFixture(binary, "linux-x86_64");
		assert.throws(
			() =>
				buildReleaseBundle({
					root,
					binary,
					target: "linux-x86_64",
					outDir: join(root, "skills", "codex-tamer", "dist"),
				}),
			/output directory.*overlaps.*Skill source/i,
		);
	});
});

test(
	"rejects output directories that alias the Skill source through a symlink",
	{ skip: process.platform === "win32" },
	() => {
		withTempDirectory((directory) => {
			const root = join(directory, "repo");
			const binary = join(directory, "codex-tamer");
			const outDir = join(directory, "dist-alias");
			createRepositoryFixture(root);
			writeBinaryFixture(binary, "linux-x86_64");
			symlinkSync(join(root, "skills", "codex-tamer"), outDir, "dir");

			assert.throws(
				() => buildReleaseBundle({ root, binary, target: "linux-x86_64", outDir }),
				/output directory.*overlaps.*Skill source/i,
			);
		});
	},
);

test("supports macOS x86_64 bundles", () => {
	withTempDirectory((directory) => {
		const root = join(directory, "repo");
		const binary = join(directory, "codex-tamer");
		createRepositoryFixture(root);
		writeBinaryFixture(binary, "macos-x86_64");
		const result = buildReleaseBundle({
			root,
			binary,
			target: "macos-x86_64",
			outDir: join(directory, "dist"),
		});
		assert.equal(result.target, "macos-x86_64");
	});
});

test("rejects a binary whose format does not match the target", () => {
	withTempDirectory((directory) => {
		const root = join(directory, "repo");
		const binary = join(directory, "codex-tamer");
		createRepositoryFixture(root);
		writeFileSync(binary, "not an ELF binary\n");
		assert.throws(
			() =>
				buildReleaseBundle({
					root,
					binary,
					target: "linux-x86_64",
					outDir: join(directory, "dist"),
				}),
			/binary format.*linux-x86_64/i,
		);
	});
});

test(
	"rejects a native executable that is not codex-tamer",
	{ skip: process.platform !== "linux" || process.arch !== "x64" },
	() => {
		withTempDirectory((directory) => {
			const root = join(directory, "repo");
			createRepositoryFixture(root);
			assert.throws(
				() =>
					buildReleaseBundleImplementation({
						root,
						binary: "/bin/ls",
						target: "linux-x86_64",
						outDir: join(directory, "dist"),
					}),
				/release binary.*codex-tamer 1\.2\.3/i,
			);
		});
	},
);
