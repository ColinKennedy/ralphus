import assert from "node:assert/strict";
import { win32 as windowsPath } from "node:path";
import test from "node:test";

process.env.RALPHUS_PI_WORKSPACE_ROOT = "C:\\unused-in-pure-tests";
const { __test } = await import("../runner/assets/pi-workspace-guard.mjs");

const assigned = "C:\\repo\\.git\\.ralphus\\g\\g96\\wt-RAL-410";
const otherWorktrees = [
	"C:\\repo",
	"C:\\repo\\.git\\.ralphus\\g\\g96\\review",
	"C:\\repo\\.git\\.ralphus\\g\\g96\\wt-RAL-412",
];

function targetsOtherWorktree(candidate) {
	return __test.isOtherProjectWorktree(
		candidate,
		assigned,
		otherWorktrees,
		windowsPath,
		true,
	);
}

function commandTargetsOtherWorktree(command) {
	return __test.commandNamesOtherWorktree(
		command,
		assigned,
		otherWorktrees,
		windowsPath,
		true,
	);
}

test("assigned review worktree wins over its containing main worktree", () => {
	for (const candidate of [
		assigned,
		`${assigned}\\scripts\\check_endpoint_cli_parity.py`,
		"C:/repo/.git/.ralphus/g/g96/wt-RAL-410/scripts/check_endpoint_cli_parity.py",
		"c:\\REPO\\.git\\.ralphus\\g\\g96\\WT-ral-410\\README.md",
		"\\\\?\\C:\\repo\\.git\\.ralphus\\g\\g96\\wt-RAL-410\\README.md",
	]) {
		assert.equal(targetsOtherWorktree(candidate), false, candidate);
	}
});

test("main and sibling review worktrees remain blocked", () => {
	for (const candidate of [
		"C:\\repo\\README.md",
		"C:/repo/.git/.ralphus/g/g96/review/README.md",
		"C:\\repo\\.git\\.ralphus\\g\\g96\\wt-RAL-412\\README.md",
		"\\\\?\\C:\\repo\\.git\\.ralphus\\g\\g96\\wt-RAL-412\\README.md",
	]) {
		assert.equal(targetsOtherWorktree(candidate), true, candidate);
	}
	assert.equal(targetsOtherWorktree("D:\\unrelated\\README.md"), false);
});

test("shell path detection permits assigned spellings but blocks other worktrees", () => {
	for (const command of [
		"git status",
		`cd ${assigned} && git status`,
		"cd C:/repo/.git/.ralphus/g/g96/wt-RAL-410 && git status",
		"cd \\\\?\\C:\\repo\\.git\\.ralphus\\g\\g96\\wt-RAL-410 && git status",
	]) {
		assert.equal(commandTargetsOtherWorktree(command), false, command);
	}

	for (const command of [
		"cd C:\\repo && git status",
		"cd C:/repo/.git/.ralphus/g/g96/review && git status",
		"cd C:\\repo\\.git\\.ralphus\\g\\g96\\wt-RAL-412 && git status",
	]) {
		assert.equal(commandTargetsOtherWorktree(command), true, command);
	}
});
