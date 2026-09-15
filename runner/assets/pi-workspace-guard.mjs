import * as path from "node:path";

const workspaceRootValue = process.env.RALPHUS_PI_WORKSPACE_ROOT;
if (!workspaceRootValue) {
	throw new Error("Ralphus Pi workspace guard requires RALPHUS_PI_WORKSPACE_ROOT");
}

const workspaceRoot = path.resolve(workspaceRootValue);
const insensitivePaths = process.platform === "win32";
let ready = false;
let failure = "Ralphus Pi workspace guard has not validated the workspace";
let otherWorktrees = [];
let projectWorktreeRoot = workspaceRoot;

function comparablePath(value) {
	let resolved = path.resolve(value);
	if (insensitivePaths && resolved.startsWith("\\\\?\\UNC\\")) {
		resolved = `\\\\${resolved.slice("\\\\?\\UNC\\".length)}`;
	} else if (insensitivePaths && resolved.startsWith("\\\\?\\")) {
		resolved = resolved.slice("\\\\?\\".length);
	}
	return insensitivePaths ? resolved.toLowerCase() : resolved;
}

function samePath(a, b) {
	return comparablePath(a) === comparablePath(b);
}

function isWithin(candidate, root) {
	const relative = path.relative(comparablePath(root), comparablePath(candidate));
	return relative === "" || (!relative.startsWith("..") && !path.isAbsolute(relative));
}

function isOtherProjectWorktree(candidate) {
	return otherWorktrees.some((root) => isWithin(candidate, root));
}

function quote(value) {
	return `'${value.replace(/'/g, "'\\''")}'`;
}

function powerShellQuote(value) {
	return `'${value.replace(/'/g, "''")}'`;
}

function listedWorktrees(output) {
	return output
		.split(/\r?\n/)
		.filter((line) => line.startsWith("worktree "))
		.map((line) => path.resolve(line.slice("worktree ".length)))
		.filter((root) => !samePath(root, projectWorktreeRoot));
}

function commandNamesOtherWorktree(command) {
	const normalized = insensitivePaths ? command.toLowerCase() : command;
	return otherWorktrees.some((root) => {
		const candidate = insensitivePaths ? root.toLowerCase() : root;
		return normalized.includes(candidate);
	});
}

function pathForTool(input) {
	if (typeof input !== "string") return { block: "Pi tool path is not a string" };
	const candidate = path.isAbsolute(input)
		? path.resolve(input)
		: path.resolve(workspaceRoot, input);
	if (isOtherProjectWorktree(candidate)) {
		return { block: `Blocked access to another worktree of this project: ${input}` };
	}
	return { path: candidate };
}

export default function ralphusPiWorkspaceGuard(pi) {
	pi.on("session_start", async () => {
		const root = await pi.exec("git", ["-C", workspaceRoot, "rev-parse", "--show-toplevel"], { timeout: 10_000 });
		if (root.code !== 0 || !samePath(root.stdout.trim(), workspaceRoot)) {
			failure = `Ralphus assigned worktree is unavailable or is not a Git worktree: ${workspaceRoot}`;
			return;
		}
		projectWorktreeRoot = path.resolve(root.stdout.trim());
		const worktrees = await pi.exec("git", ["-C", workspaceRoot, "worktree", "list", "--porcelain"], { timeout: 10_000 });
		if (worktrees.code !== 0) {
			failure = "Ralphus Pi workspace guard could not list this project's worktrees";
			return;
		}
		otherWorktrees = listedWorktrees(worktrees.stdout);
		ready = true;
	});

	pi.on("tool_call", (event) => {
		if (!ready) return { block: true, reason: failure };
		if (event.toolName === "bash" || event.toolName === "powershell") {
			if (typeof event.input.command !== "string") return;
			if (commandNamesOtherWorktree(event.input.command)) {
				return { block: true, reason: "Blocked command targeting another worktree of this project" };
			}
			event.input.command = event.toolName === "powershell"
				? `Set-Location -LiteralPath ${powerShellQuote(workspaceRoot)}; ${event.input.command}`
				: `cd ${quote(workspaceRoot)} && ${event.input.command}`;
			return;
		}
		if (["read", "write", "edit", "grep", "find", "ls"].includes(event.toolName)) {
			if (typeof event.input.path !== "string") {
				if (["grep", "find", "ls"].includes(event.toolName)) event.input.path = workspaceRoot;
				return;
			}
			const result = pathForTool(event.input.path);
			if ("block" in result) return { block: true, reason: result.block };
			event.input.path = result.path;
		}
	});
}
