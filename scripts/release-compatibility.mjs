import { existsSync, readFileSync, writeFileSync } from "node:fs";

export function requireCompatibilityFile(path) {
	if (!existsSync(path)) {
		throw new Error(`Required compatibility ledger is missing: ${path}`);
	}
}

export function updateCompatibilityForRelease(path, version) {
	requireCompatibilityFile(path);
	let content = readFileSync(path, "utf-8");
	const unreleasedRow = /^\| Unreleased \|/m;
	if (!unreleasedRow.test(content)) {
		return false;
	}
	content = content.replace(unreleasedRow, `| ${version} |`);
	writeFileSync(path, content, "utf-8");
	return true;
}
