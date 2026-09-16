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

function comparablePath(value, pathModule, insensitive) {
	let resolved = pathModule.resolve(value);
	if (insensitive && resolved.startsWith("\\\\?\\UNC\\")) {
		resolved = `\\\\${resolved.slice("\\\\?\\UNC\\".length)}`;
	} else if (insensitive && resolved.startsWith("\\\\?\\")) {
		resolved = resolved.slice("\\\\?\\".length);
	}
	return insensitive ? resolved.toLowerCase() : resolved;
}

function samePath(a, b) {
	return comparablePath(a, path, insensitivePaths) === comparablePath(b, path, insensitivePaths);
}

function isWithin(candidate, root, pathModule, insensitive) {
	const relative = pathModule.relative(comparablePath(root, pathModule, insensitive), comparablePath(candidate, pathModule, insensitive));
	return relative === "" || (!relative.startsWith("..") && !pathModule.isAbsolute(relative));
}

// This project nests worktrees under `.git/.ralphus/g/...` of the main
// worktree, so the main worktree -- always present in `otherWorktrees` --
// is an ancestor of the assigned one. Checking `otherWorktrees` alone would
// therefore block the agent from its own assigned files; the assigned
// worktree always wins over any broader root that merely contains it.
function isOtherProjectWorktree(candidate, assigned, otherWorktrees, pathModule, insensitive) {
	if (isWithin(candidate, assigned, pathModule, insensitive)) return false;
	return otherWorktrees.some((root) => isWithin(candidate, root, pathModule, insensitive));
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

// Same "assigned wins" rule as `isOtherProjectWorktree`, applied as a
// substring search over a raw shell command instead of a resolved path:
// mask out every occurrence of the assigned worktree first, so a command
// that only names the assigned worktree isn't blocked merely because that
// path's text also contains a shorter `otherWorktrees` root as a prefix
// (e.g. the assigned worktree nested under the main worktree root).
function commandNamesOtherWorktree(command, assigned, otherWorktrees, pathModule, insensitive) {
	const normalize = (text) => {
		const otherSep = pathModule.sep === "\\" ? "/" : "\\";
		const canonical = text.split(otherSep).join(pathModule.sep);
		const stripped = pathModule.sep === "\\"
			? canonical.replace(/\\\\\?\\UNC\\/g, "\\\\").replace(/\\\\\?\\/g, "")
			: canonical;
		return insensitive ? stripped.toLowerCase() : stripped;
	};
	const normalizedAssigned = normalize(assigned);
	const normalizedCommand = normalize(command);
	const masked = normalizedAssigned ? normalizedCommand.split(normalizedAssigned).join("\0") : normalizedCommand;
	return otherWorktrees.some((root) => masked.includes(normalize(root)));
}

function pathForTool(input) {
	if (typeof input !== "string") return { block: "Pi tool path is not a string" };
	const candidate = path.isAbsolute(input)
		? path.resolve(input)
		: path.resolve(workspaceRoot, input);
	if (isOtherProjectWorktree(candidate, projectWorktreeRoot, otherWorktrees, path, insensitivePaths)) {
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
			if (commandNamesOtherWorktree(event.input.command, projectWorktreeRoot, otherWorktrees, path, insensitivePaths)) {
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

export const __test = { isOtherProjectWorktree, commandNamesOtherWorktree };
