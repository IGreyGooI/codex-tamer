import assert from "node:assert/strict";
import {
	chmodSync,
	copyFileSync,
	existsSync,
	lstatSync,
	mkdtempSync,
	mkdirSync,
	readFileSync,
	readlinkSync,
	rmSync,
	symlinkSync,
	writeFileSync,
} from "node:fs";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import { delimiter, join } from "node:path";
import test from "node:test";

import {
	defaultInstallDirectories,
	installBundle,
	rollbackActivatedPaths,
	verifyInstalledBinary,
} from "./install-bundle.mjs";

function withTempDirectory(run) {
	const directory = mkdtempSync(join(tmpdir(), "codex-tamer-install-test-"));
	try {
		return run(directory);
	} finally {
		rmSync(directory, { recursive: true, force: true });
	}
}

function createBundle(
	root,
	{
		skillText = "skill-v1\n",
		binaryText = "binary-v1\n",
		target = "linux-x86_64",
		version = "0.2.4",
	} = {},
) {
	const binaryName = target.startsWith("windows-") ? "codex-tamer.exe" : "codex-tamer";
	mkdirSync(join(root, "bin"), { recursive: true });
	mkdirSync(join(root, "skills", "codex-tamer", "agents"), { recursive: true });
	writeFileSync(join(root, "bin", binaryName), binaryText);
	if (process.platform !== "win32") {
		chmodSync(join(root, "bin", binaryName), 0o755);
	}
	writeFileSync(join(root, "skills", "codex-tamer", "SKILL.md"), skillText);
	writeFileSync(
		join(root, "skills", "codex-tamer", "agents", "openai.yaml"),
		"interface:\n  display_name: Codex Tamer\n",
	);
	writeFileSync(
		join(root, "manifest.json"),
		`${JSON.stringify(
			{
				name: "codex-tamer",
				version,
				target,
				binary: `bin/${binaryName}`,
				skill: "skills/codex-tamer",
				installer: "install.mjs",
			},
			null,
			2,
		)}\n`,
	);
}

function nativeInstallFixture() {
	const target = {
		"linux-x64": "linux-x86_64",
		"linux-arm64": "linux-aarch64",
		"darwin-x64": "macos-x86_64",
		"darwin-arm64": "macos-aarch64",
		"win32-x64": "windows-x86_64",
	}[`${process.platform}-${process.arch}`];
	assert.ok(target, `unsupported test platform: ${process.platform}-${process.arch}`);
	return {
		arch: process.arch,
		binaryName: process.platform === "win32" ? "codex-tamer.exe" : "codex-tamer",
		platform: process.platform,
		target,
	};
}

test("installs the binary and skill without rewriting the skill", () => {
	withTempDirectory((directory) => {
		const native = nativeInstallFixture();
		const bundleRoot = join(directory, "bundle");
		const binDir = join(directory, "home", ".local", "bin");
		const skillsDir = join(directory, "home", ".agents", "skills");
		const skillText = [
			"---",
			"name: codex-tamer",
			"metadata:",
			"  requires:",
			"    bins: [\"codex-tamer\"]",
			"---",
			"Run `codex-tamer list --json`.",
			"",
		].join("\n");
		createBundle(bundleRoot, { skillText, target: native.target });

		const result = installBundle({
			bundleRoot,
			binDir,
			skillsDir,
			platform: native.platform,
			arch: native.arch,
			pathValue: `${binDir}${delimiter}${join(directory, "other-bin")}`,
			verifyBinary: (binaryPath) => {
				assert.equal(binaryPath, join(binDir, native.binaryName));
				return "codex-tamer 0.2.4";
			},
		});

		assert.equal(readFileSync(join(binDir, native.binaryName), "utf8"), "binary-v1\n");
		assert.equal(
			readFileSync(join(skillsDir, "codex-tamer", "SKILL.md"), "utf8"),
			skillText,
		);
		if (process.platform !== "win32") {
			assert.equal(lstatSync(join(binDir, native.binaryName)).mode & 0o111, 0o111);
		}
		assert.equal(result.binary.onPath, true);
		assert.equal(result.binary.version, "codex-tamer 0.2.4");
		assert.equal(result.skill.path, join(skillsDir, "codex-tamer"));
	});
});

test("replaces an existing managed binary and skill", () => {
	withTempDirectory((directory) => {
		const bundleRoot = join(directory, "bundle");
		const binDir = join(directory, "bin");
		const skillsDir = join(directory, "skills");
		createBundle(bundleRoot, { skillText: "skill-v2\n", binaryText: "binary-v2\n" });
		mkdirSync(join(skillsDir, "codex-tamer"), { recursive: true });
		mkdirSync(binDir, { recursive: true });
		writeFileSync(join(binDir, "codex-tamer"), "binary-v1\n");
		writeFileSync(join(skillsDir, "codex-tamer", "SKILL.md"), "skill-v1\n");

		installBundle({
			bundleRoot,
			binDir,
			skillsDir,
			platform: "linux",
			arch: "x64",
			pathValue: "",
			verifyBinary: () => "codex-tamer 0.2.4",
		});

		assert.equal(readFileSync(join(binDir, "codex-tamer"), "utf8"), "binary-v2\n");
		assert.equal(
			readFileSync(join(skillsDir, "codex-tamer", "SKILL.md"), "utf8"),
			"skill-v2\n",
		);
	});
});

test("reports when the stable launcher directory is not on PATH", () => {
	withTempDirectory((directory) => {
		const bundleRoot = join(directory, "bundle");
		const binDir = join(directory, "bin");
		createBundle(bundleRoot);

		const result = installBundle({
			bundleRoot,
			binDir,
			skillsDir: join(directory, "skills"),
			platform: "linux",
			arch: "x64",
			pathValue: "/usr/bin:/bin",
			verifyBinary: () => "codex-tamer 0.2.4",
		});

		assert.equal(result.binary.onPath, false);
		assert.match(result.binary.pathHint, /\.local|PATH|bin/);
	});
});

test("reports an older codex-tamer that takes precedence on PATH", () => {
	withTempDirectory((directory) => {
		const native = nativeInstallFixture();
		const bundleRoot = join(directory, "bundle");
		const binDir = join(directory, "installed-bin");
		const earlierBinDir = join(directory, "earlier-bin");
		createBundle(bundleRoot, { target: native.target });
		mkdirSync(earlierBinDir, { recursive: true });
		writeFileSync(join(earlierBinDir, native.binaryName), "old binary\n");
		if (process.platform !== "win32") {
			chmodSync(join(earlierBinDir, native.binaryName), 0o755);
		}

		const result = installBundle({
			bundleRoot,
			binDir,
			skillsDir: join(directory, "skills"),
			platform: native.platform,
			arch: native.arch,
			pathValue: `${earlierBinDir}${delimiter}${binDir}`,
			verifyBinary: () => "codex-tamer 0.2.4",
		});

		assert.equal(result.binary.onPath, false);
		assert.equal(result.binary.resolvedPath, join(earlierBinDir, native.binaryName));
		assert.match(result.binary.pathHint, /takes precedence|earlier-bin/);
	});
});

test(
	"keeps PATH comparison case-sensitive on case-sensitive platforms",
	{ skip: process.platform === "win32" },
	() => {
		withTempDirectory((directory) => {
			const bundleRoot = join(directory, "bundle");
			const binDir = join(directory, "Bin");
			const earlierBinDir = join(directory, "bin");
			createBundle(bundleRoot);
			mkdirSync(earlierBinDir, { recursive: true });
			writeFileSync(join(earlierBinDir, "codex-tamer"), "old binary\n");
			chmodSync(join(earlierBinDir, "codex-tamer"), 0o755);

			const result = installBundle({
				bundleRoot,
				binDir,
				skillsDir: join(directory, "skills"),
				platform: "linux",
				arch: "x64",
				pathValue: `${earlierBinDir}${delimiter}${binDir}`,
				verifyBinary: () => "codex-tamer 0.2.4",
			});

			assert.equal(result.binary.onPath, false);
			assert.equal(result.binary.resolvedPath, join(earlierBinDir, "codex-tamer"));
		});
	},
);

test("restores the previous installation when binary verification fails", () => {
	withTempDirectory((directory) => {
		const bundleRoot = join(directory, "bundle");
		const binDir = join(directory, "bin");
		const skillsDir = join(directory, "skills");
		createBundle(bundleRoot, { skillText: "skill-bad\n", binaryText: "binary-bad\n" });
		mkdirSync(join(skillsDir, "codex-tamer"), { recursive: true });
		mkdirSync(binDir, { recursive: true });
		writeFileSync(join(binDir, "codex-tamer"), "binary-good\n");
		writeFileSync(join(skillsDir, "codex-tamer", "SKILL.md"), "skill-good\n");

		assert.throws(
			() =>
				installBundle({
					bundleRoot,
					binDir,
					skillsDir,
					platform: "linux",
					arch: "x64",
					pathValue: "",
					verifyBinary: () => {
						throw new Error("version probe failed");
					},
				}),
			/version probe failed/,
		);
		assert.equal(readFileSync(join(binDir, "codex-tamer"), "utf8"), "binary-good\n");
		assert.equal(
			readFileSync(join(skillsDir, "codex-tamer", "SKILL.md"), "utf8"),
			"skill-good\n",
		);
	});
});

test("attempts every resource rollback when an earlier restore fails", () => {
	const attempts = [];
	assert.throws(
		() =>
			rollbackActivatedPaths(
				[
					{ destinationPath: "/install/skill", backupPath: "/backup/skill" },
					{ destinationPath: "/install/binary", backupPath: "/backup/binary" },
				],
				(destinationPath) => {
					attempts.push(destinationPath);
					if (destinationPath === "/install/skill") {
						throw new Error("skill restore failed");
					}
				},
			),
		(error) => {
			assert(error instanceof AggregateError);
			assert.match(error.message, /failed to fully roll back installation/i);
			assert.match(error.errors[0].message, /skill restore failed/);
			return true;
		},
	);
	assert.deepEqual(attempts, ["/install/skill", "/install/binary"]);
});

test("restores the binary when the Skill rollback fails during installation", () => {
	withTempDirectory((directory) => {
		const bundleRoot = join(directory, "bundle");
		const binDir = join(directory, "bin");
		const skillsDir = join(directory, "skills");
		const destinationBinary = join(binDir, "codex-tamer");
		const destinationSkill = join(skillsDir, "codex-tamer");
		const attempts = [];
		createBundle(bundleRoot, { skillText: "skill-new\n", binaryText: "binary-new\n" });
		mkdirSync(destinationSkill, { recursive: true });
		mkdirSync(binDir, { recursive: true });
		writeFileSync(destinationBinary, "binary-old\n");
		writeFileSync(join(destinationSkill, "SKILL.md"), "skill-old\n");

		assert.throws(
			() =>
				installBundle({
					bundleRoot,
					binDir,
					skillsDir,
					platform: "linux",
					arch: "x64",
					pathValue: "",
					verifyBinary: () => {
						throw new Error("version probe failed");
					},
					rollbackPath: (destinationPath, backupPath) => {
						attempts.push(destinationPath);
						if (destinationPath === destinationSkill) {
							throw new Error("skill restore failed");
						}
						rollbackActivatedPaths([{ destinationPath, backupPath }]);
					},
				}),
			(error) => {
				assert(error instanceof AggregateError);
				assert.match(error.message, /version probe failed.*failed to fully roll back/i);
				return true;
			},
		);
		assert.deepEqual(attempts, [destinationSkill, destinationBinary]);
		assert.equal(readFileSync(destinationBinary, "utf8"), "binary-old\n");
	});
});

test(
	"rejects symlinks in a bundled skill",
	{ skip: process.platform === "win32" },
	() => {
		withTempDirectory((directory) => {
		const bundleRoot = join(directory, "bundle");
		createBundle(bundleRoot);
		const outside = join(directory, "outside");
		writeFileSync(outside, "do not copy\n");
		symlinkSync(outside, join(bundleRoot, "skills", "codex-tamer", "linked"));

		assert.throws(
			() =>
				installBundle({
					bundleRoot,
					binDir: join(directory, "bin"),
					skillsDir: join(directory, "skills"),
					platform: "linux",
					arch: "x64",
					pathValue: "",
					verifyBinary: () => "codex-tamer 0.2.4",
				}),
			/bundled skill.*symbolic link/i,
		);
		assert.equal(existsSync(join(directory, "skills", "codex-tamer")), false);
		assert.equal(readlinkSync(join(bundleRoot, "skills", "codex-tamer", "linked")), outside);
		});
	},
);

test("rejects a bundle built for another platform before changing the installation", () => {
	withTempDirectory((directory) => {
		const bundleRoot = join(directory, "bundle");
		createBundle(bundleRoot, { target: "windows-x86_64" });

		assert.throws(
			() =>
				installBundle({
					bundleRoot,
					binDir: join(directory, "bin"),
					skillsDir: join(directory, "skills"),
					platform: "linux",
					arch: "x64",
					pathValue: "",
					verifyBinary: () => "codex-tamer 0.2.4",
				}),
			/bundle target windows-x86_64.*current platform linux-x86_64/i,
		);
		assert.equal(existsSync(join(directory, "bin", "codex-tamer")), false);
	});
});

test("rejects install destinations that overlap the extracted bundle", () => {
	withTempDirectory((directory) => {
		const bundleRoot = join(directory, "bundle");
		createBundle(bundleRoot);
		assert.throws(
			() =>
				installBundle({
					bundleRoot,
					binDir: join(directory, "bin"),
					skillsDir: join(bundleRoot, "skills"),
					platform: "linux",
					arch: "x64",
					pathValue: "",
					verifyBinary: () => "codex-tamer 0.2.4",
				}),
			/install destination.*overlaps.*bundle/i,
		);
	});
});

test(
	"rejects install destinations that alias the extracted bundle through a symlink",
	{ skip: process.platform === "win32" },
	() => {
		withTempDirectory((directory) => {
			const bundleRoot = join(directory, "bundle");
			const skillsAlias = join(directory, "skills-alias");
			createBundle(bundleRoot);
			symlinkSync(join(bundleRoot, "skills"), skillsAlias, "dir");

			assert.throws(
				() =>
					installBundle({
						bundleRoot,
						binDir: join(directory, "bin"),
						skillsDir: skillsAlias,
						platform: "linux",
						arch: "x64",
						pathValue: "",
						verifyBinary: () => "codex-tamer 0.2.4",
					}),
				/install destination.*overlaps.*bundle/i,
			);
		});
	},
);

test("restores the previous installation when the binary version differs from the manifest", () => {
	withTempDirectory((directory) => {
		const bundleRoot = join(directory, "bundle");
		const binDir = join(directory, "bin");
		const skillsDir = join(directory, "skills");
		createBundle(bundleRoot, { version: "0.2.4" });
		mkdirSync(join(skillsDir, "codex-tamer"), { recursive: true });
		mkdirSync(binDir, { recursive: true });
		writeFileSync(join(binDir, "codex-tamer"), "binary-good\n");
		writeFileSync(join(skillsDir, "codex-tamer", "SKILL.md"), "skill-good\n");

		assert.throws(
			() =>
				installBundle({
					bundleRoot,
					binDir,
					skillsDir,
					platform: "linux",
					arch: "x64",
					pathValue: "",
					verifyBinary: () => "codex-tamer 9.9.9",
				}),
			/binary version 9\.9\.9.*manifest version 0\.2\.4/i,
		);
		assert.equal(readFileSync(join(binDir, "codex-tamer"), "utf8"), "binary-good\n");
		assert.equal(
			readFileSync(join(skillsDir, "codex-tamer", "SKILL.md"), "utf8"),
			"skill-good\n",
		);
	});
});

test("uses stable per-user install directories", () => {
	assert.deepEqual(
		defaultInstallDirectories({ platform: "linux", home: "/home/alice" }),
		{
			binDir: "/home/alice/.local/bin",
			skillsDir: "/home/alice/.agents/skills",
		},
	);
	assert.deepEqual(
		defaultInstallDirectories({
			platform: "win32",
			home: "C:\\Users\\Alice",
			localAppData: "C:\\Users\\Alice\\AppData\\Local",
		}),
		{
			binDir: "C:\\Users\\Alice\\AppData\\Local\\codex-tamer\\bin",
			skillsDir: "C:\\Users\\Alice\\.agents\\skills",
		},
	);
});

test(
	"the packaged installer CLI performs an end-to-end local install",
	{ skip: process.platform !== "linux" || process.arch !== "x64" },
	() => {
		withTempDirectory((directory) => {
		const bundleRoot = join(directory, "bundle");
		const binDir = join(directory, "installed-bin");
		const skillsDir = join(directory, "installed-skills");
		createBundle(bundleRoot, {
			binaryText: "#!/bin/sh\nprintf 'codex-tamer 0.2.4\\n'\n",
		});
		copyFileSync(join(import.meta.dirname, "install-bundle.mjs"), join(bundleRoot, "install.mjs"));

		const installed = spawnSync(
			process.execPath,
			[
				join(bundleRoot, "install.mjs"),
				"--bin-dir",
				binDir,
				"--skills-dir",
				skillsDir,
				"--json",
			],
			{ encoding: "utf8", env: { ...process.env, PATH: "/usr/bin:/bin" } },
		);

		assert.equal(installed.status, 0, installed.stderr);
		const result = JSON.parse(installed.stdout);
		assert.equal(result.ok, true);
		assert.equal(result.binary.version, "codex-tamer 0.2.4");
		assert.equal(result.binary.onPath, false);
		assert.equal(existsSync(join(binDir, "codex-tamer")), true);
		assert.equal(existsSync(join(skillsDir, "codex-tamer", "SKILL.md")), true);
		});
	},
);

test("the packaged installer CLI rejects unknown arguments", () => {
	const rejected = spawnSync(process.execPath, [join(import.meta.dirname, "install-bundle.mjs"), "--wat"], {
		encoding: "utf8",
	});
	assert.equal(rejected.status, 1);
	assert.match(rejected.stderr, /unknown argument: --wat/);
});

test("the packaged installer CLI prints help and validates option values", () => {
	const script = join(import.meta.dirname, "install-bundle.mjs");
	const help = spawnSync(process.execPath, [script, "--help"], { encoding: "utf8" });
	assert.equal(help.status, 0, help.stderr);
	assert.match(help.stdout, /Usage: node install\.mjs/);
	const missingValue = spawnSync(process.execPath, [script, "--bin-dir"], { encoding: "utf8" });
	assert.equal(missingValue.status, 1);
	assert.match(missingValue.stderr, /--bin-dir requires a path/);
	const optionLikeValue = spawnSync(process.execPath, [script, "--skills-dir", "--json"], {
		encoding: "utf8",
	});
	assert.equal(optionLikeValue.status, 1);
	assert.match(optionLikeValue.stderr, /--skills-dir requires a path/);
});

test(
	"binary verification reports spawn and nonzero-exit failures",
	{ skip: process.platform === "win32" },
	() => {
		withTempDirectory((directory) => {
		assert.throws(
			() => verifyInstalledBinary(join(directory, "missing")),
			/failed to execute installed binary/,
		);
		const failing = join(directory, "failing");
		writeFileSync(failing, "#!/bin/sh\nprintf 'bad version probe\\n' >&2\nexit 7\n");
		chmodSync(failing, 0o755);
		assert.throws(
			() => verifyInstalledBinary(failing),
			/version verification with exit 7: bad version probe/,
		);
		});
	},
);

test("rejects malformed and inconsistent bundle manifests", () => {
	withTempDirectory((directory) => {
		const bundleRoot = join(directory, "bundle");
		createBundle(bundleRoot);
		writeFileSync(join(bundleRoot, "manifest.json"), "not json\n");
		const options = {
			bundleRoot,
			binDir: join(directory, "bin"),
			skillsDir: join(directory, "skills"),
			platform: "linux",
			arch: "x64",
			pathValue: "",
			verifyBinary: () => "codex-tamer 0.2.4",
		};
		assert.throws(() => installBundle(options), /manifest is not valid JSON/);
		createBundle(bundleRoot);
		const manifestPath = join(bundleRoot, "manifest.json");
		const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
		writeFileSync(manifestPath, `${JSON.stringify({ ...manifest, name: "other" })}\n`);
		assert.throws(() => installBundle(options), /manifest name must be codex-tamer/);
	});
});
