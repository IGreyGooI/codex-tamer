import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import {
	existsSync,
	mkdtempSync,
	readFileSync,
	rmSync,
	writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { basename, join } from "node:path";
import test from "node:test";

import { assembleReleaseAssets } from "./assemble-release-assets.mjs";

const TARGETS = [
	["linux-x86_64", ".tar.gz"],
	["linux-aarch64", ".tar.gz"],
	["macos-aarch64", ".tar.gz"],
	["macos-x86_64", ".tar.gz"],
	["windows-x86_64", ".zip"],
];

function withTempDirectory(run) {
	const directory = mkdtempSync(join(tmpdir(), "codex-tamer-assets-test-"));
	try {
		return run(directory);
	} finally {
		rmSync(directory, { recursive: true, force: true });
	}
}

function writeAsset(directory, version, target, extension) {
	const archiveName = `codex-tamer-${version}-${target}${extension}`;
	const archivePath = join(directory, archiveName);
	writeFileSync(archivePath, `archive for ${target}\n`);
	const sha256 = createHash("sha256").update(readFileSync(archivePath)).digest("hex");
	writeFileSync(`${archivePath}.sha256`, `${sha256}  ${archiveName}\n`);
	return { archivePath, sha256 };
}

function writeCompleteAssetSet(directory, version = "1.2.3") {
	return TARGETS.map(([target, extension]) =>
		writeAsset(directory, version, target, extension),
	);
}

test("verifies every platform asset and writes deterministic SHA256SUMS", () => {
	withTempDirectory((directory) => {
		const assets = writeCompleteAssetSet(directory);
		const result = assembleReleaseAssets({ directory, version: "1.2.3" });

		assert.equal(result.archives.length, TARGETS.length);
		assert.equal(result.checksums.length, TARGETS.length);
		assert.equal(result.sha256SumsPath, join(directory, "SHA256SUMS"));
		const expected = assets
			.map(({ archivePath, sha256 }) => `${sha256}  ${basename(archivePath)}`)
			.sort()
			.join("\n");
		assert.equal(readFileSync(result.sha256SumsPath, "utf8"), `${expected}\n`);
	});
});

test("rejects an incomplete platform asset set", () => {
	withTempDirectory((directory) => {
		for (const [target, extension] of TARGETS.slice(0, -1)) {
			writeAsset(directory, "1.2.3", target, extension);
		}
		assert.throws(
			() => assembleReleaseAssets({ directory, version: "1.2.3" }),
			/missing release archive.*windows-x86_64/i,
		);
		assert.equal(existsSync(join(directory, "SHA256SUMS")), false);
	});
});

test("rejects a checksum that does not match its archive", () => {
	withTempDirectory((directory) => {
		const assets = writeCompleteAssetSet(directory);
		writeFileSync(assets[0].archivePath, "tampered\n");
		assert.throws(
			() => assembleReleaseAssets({ directory, version: "1.2.3" }),
			/SHA256 mismatch/i,
		);
		assert.equal(existsSync(join(directory, "SHA256SUMS")), false);
	});
});

test("rejects undeclared files in the release upload set", () => {
	withTempDirectory((directory) => {
		writeCompleteAssetSet(directory);
		writeFileSync(join(directory, "codex-tamer-1.2.3-linux-i686.tar.gz"), "unexpected\n");
		assert.throws(
			() => assembleReleaseAssets({ directory, version: "1.2.3" }),
			/unexpected release asset.*linux-i686/i,
		);
		assert.equal(existsSync(join(directory, "SHA256SUMS")), false);
	});
});

test("rejects unsafe versions and malformed adjacent checksum files", () => {
	withTempDirectory((directory) => {
		assert.throws(
			() => assembleReleaseAssets({ directory, version: "../1.2.3" }),
			/valid package version/i,
		);
		const assets = writeCompleteAssetSet(directory);
		writeFileSync(`${assets[0].archivePath}.sha256`, "not a checksum\n");
		assert.throws(
			() => assembleReleaseAssets({ directory, version: "1.2.3" }),
			/malformed checksum file/i,
		);
	});
});
