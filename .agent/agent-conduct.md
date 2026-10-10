# Agent conduct

Rules for any agent (Claude Code, a ralphus cell, a Guardian resolver) doing
work in this repo. Each one is here because ignoring it has already cost a
real session. The root `AGENTS.md` carries the short form; this file carries
the reasoning and the alternative to reach for instead.

## Never start, stop, or restart the daemon or librarian

Do not run `scripts/build-debug.sh`, `ralphus-daemon serve`,
`ralphus-librarian serve`, or anything else that starts or restarts those two
processes unless the user asks for it in that turn. A persistent dev stack is
normally already running.

A failed `ralphus check health` or a connection-refused `curl` on
`127.0.0.1:7890` is **not** sufficient evidence the daemon is down — transient
port/timing hiccups happen. Launching `build-debug.sh` "to help" once picked
up an in-flight squad mid-scheduling and produced duplicate racing cell
starts.

If a `ralphus` command reports a connection error, ask whether the daemon is
expected to be up rather than starting it. If a real *daemon-authored* error
comes back (a validation message, a 4xx body), that is proof the daemon is
reachable — don't let an earlier flaky probe override it. Plain git operations
on worktrees are not "touching the daemon" and are fine.

## Never launch a second daemon or tmux server from inside a ralphus run

When ralphus itself is driving the work (a cell, a proof step, or a Guardian
feedback/auto-fix pass developing ralphus), do not run anything that starts
another daemon: `scripts/check-initialize-exercises.sh`, `ralphus initialize
<exercise>` (every guided exercise starts its own throwaway daemon), or a
hand-launched `ralphus-daemon serve` on another port.

Every daemon start calls `tmux::reap_orphaned_sessions_at_startup`
(`daemon/src/tmux.rs`), which force-kills every `ralphus_`-prefixed tmux
server **machine-wide** -- not only the new daemon's own. The host daemon's
live sessions go with it, including the pane your agent is running in. The
symptom is a branch/cell that fails with `runner produced no result file ...
os error 2` and a pane tail ending in `tmux server process exit code:
unknown`, repeating on every retry because the retry re-runs the same
command. The isolated state root and port the exercise uses do not help: the
reap is not scoped by them.

Do this instead:

- Reproduce by reading the code: the exercise source in
  `cli/src/commands/initialize/` and its fixtures say what it asserts.
- Verify with `cargo fmt --all -- --check`, `cargo clippy --all-targets --
  -D warnings` and `cargo nextest run -p <crate> --lib` (never `--all-targets`;
  see [`gotchas.md`](gotchas.md)).
- Say plainly in your final message that the initialize exercise was **not**
  run, and let the user run it from their own terminal. CI's
  `initialize-exercises` job runs it on a clean runner.

## Never use `git stash` in this repo

Not bare, not chained with an immediate `pop`, not "just this once", not even
when `git status` shows a clean tree.

The stash stack lives in the shared `.git`, not per-worktree. Many ralphus
task worktrees hang off the same `.git`, and concurrent agents push their own
entries onto that stack. A bare `pop` takes whatever is on top *at that
moment* — which has, in practice, dumped an unrelated task's diff and conflict
markers across files in the wrong worktree. A clean tree makes it worse, not
safer: the push half stores nothing, so the pop half is guaranteed to grab
someone else's entry.

Reach for one of these instead:

- `git diff --stat` / reading the diff — usually already answers "did my
  change touch this area", with zero risk.
- `git show <ref>:path/to/file.rs` or `git diff <ref1> <ref2> -- path` to see
  a file as it stands at another commit.
- `git worktree add <short-scratch-path> <ref>` for a genuinely separate
  checkout when unmodified code must actually be executed (see the MAX_PATH
  gotcha in [`gotchas.md`](gotchas.md) for why the path must be short).

Treat typing `git stash` as a stop-and-ask trigger.

## Extend agent backends through their existing abstractions

When changing an agent, agent backend, model integration, launcher, or its
availability behavior, route the work through the backend abstractions that
already own that behavior. In particular, prefer the concrete Claude Code,
Codex, Pi, and native-agent backend implementations and the shared
`ModelBackend` interface over daemon-side tables or duplicated backend-specific
conditionals.

Backend capabilities are part of that rule. A caller must obtain an answer
such as whether a backend supports thinking, streaming, a model option, or a
launch feature from the backend abstraction, not from a raw backend-name
comparison, a `match`/lookup table keyed by strings, or a second
backend-specific helper elsewhere in the codebase. Add the capability to the
shared abstraction when multiple callers need it, and let each concrete
backend provide its answer. If a layer cannot access the abstraction because
of a dependency boundary, move the query to a layer that can or expose the
answer through its API; do not recreate a name-based mirror. This keeps a new
backend's capability behavior correct in every caller when its implementation
is added or changed.

The daemon orchestrates agent work; it must not become a second source of
truth for how a backend selects or invokes its program. If a capability needs
to be shared across backends, add it to the shared backend abstraction and let
each concrete backend implement it. This keeps backend-specific configuration,
defaults, and launch semantics in the one place that executes them.

## Post-merge work never gates merge/rebase completion (RAL-520)

A review's merge/rebase succeeds the moment its stack is rebuilt — the check
gates, manual-checks generation, and any other settled-state follow-up run in
the independent post-merge worker (`guardian_merge::run_guardian_post_merge`)
against a scratch checkout, and are **advisory**. Never reintroduce code that
makes post-merge work hold the review in `merging`, reject a new merge while a
post-merge job is running, disable/relabel the board's Merge / rebase control,
or fold a post-merge outcome into the review's own status. Post-merge failures
are recorded on the post-merge phase and surfaced separately; the review stays
`in_review`, approvable, and mergeable throughout. New settled-state work
(manual checks, change summary, check gates, auto-build) belongs in that
worker, not in the merge lifecycle.

## Comments and docstrings describe the code as it is now

No "used to", no "originally", no "ported from Python", no narrating a `TODO`
that was removed, no explaining a callee's internals. Test: would the sentence
still make sense to someone who never saw a previous version? If not, cut the
framing and state the current structure and reason — "shared by X and Y so
both call the same logic", not "X used to duplicate this".

This applies to code comments and docstrings project-wide. Architecture
narrative in `AGENTS.md` files is a separate case — those documents are meant
to record where things came from.

## Keep shell and batch scripts in parity

When changing a script with both `scripts/*.sh` and `scripts/*.cmd` entry
points, make the corresponding behavior available in both. Platform-specific
implementation details may differ, but their user-facing options and intent
must remain aligned.

## External CLI calls use long flags

Every invocation this repo makes of another command-line tool -- `git`, `gh`,
`glab`, `cargo`, `docker`, `mkdocs`, `cygpath`, whatever comes next -- spells
its options in long form. `cargo build --package ralphus-daemon`, not
`cargo build -p ralphus-daemon`; `git commit --message`, not `git commit -m`.
This holds in `scripts/`, in daemon-spawned subprocess code, and in the test
helpers that shell out to `git`.

A short flag is a lookup for every later reader, and the daemon's subprocess
call sites are the worst place to pay that cost: they run unattended, so a
misread flag surfaces as a broken squad rather than a compile error. Long
flags also survive review -- `--set-upstream` is checkable at a glance in a
diff, `-u` is not.

Convert only where the tool documents an equivalent. Verify against the
tool's own `--help` rather than assuming a long form exists; several common
ones do not, and these stay short:

- `git -C <path>` and `git -c <key>=<val>` -- no long forms.
- `git checkout -b`/`-B` and `git worktree add -b`/`-B` -- `--branch` is
  rejected by both.
- `git ls-tree -r`/`-z`, `git ls-files -z`, `git clean -d` -- no long forms
  (`--recursive`/`--directories` are rejected).
- `ssh`, `ssh-keygen`, `scp`, `tmux` -- these accept single-dash options
  only, repo-wide.
- `sh -c` / `cmd /C` -- the shell's own invocation flag.
- POSIX coreutils in `scripts/*.sh` (`mkdir -p`, `rm -rf`, `tail -n`): the
  GNU long forms are not portable, and these scripts are documented as
  macOS/Linux, where BSD versions reject them.

Shell test operators (`[ -f ]`, `[ -z ]`, `read -r`) are not external tool
calls and are out of scope.

## Commit messages

Omit the `Claude-Session: https://claude.ai/code/session_...` trailer. Keep
`Co-Authored-By:`.

## Clarifying questions use concrete commands, not terms of art

When a design correction has more than one plausible implementation, asking is
right — but frame the options as literal command sequences or observable
behavior ("the worktree keeps up with pushes; local commits are never lost"),
not ref-mechanics vocabulary like "detached HEAD". Prefer `AskUserQuestion`
options with command-sequence previews over prose descriptions.

## New TOML fields: ask before validating

Whenever a new field is added to the task-file schema (`core/src/schema.rs`),
ask the user whether it should be validated in `core/src/validate.rs`, and
if so, how — before writing the validation logic, not after. A field can be
free-form text, a closed set of literals (reject anything outside the set,
like `system_prompt_position`), a numeric range (reject `<= 0`, like
`maximum_budget_usd`), cross-checked against another field, or deliberately
left unvalidated and only sanity-checked server-side at write time (the
existing precedent for `[[review]] proof_scope`'s own daemon-side setter,
which stores whatever string it's given and only filters at *read* time).
None of these is the obviously-correct default — guessing one produces
either an over-strict validator that rejects a value the user actually
wanted, or a silently-lenient one that lets a typo through to the daemon.

Ask concretely: name the candidate value shapes (an enum's literal list, a
numeric bound, "no validation") rather than asking "should this be
validated?" in the abstract.

## Keep `initialize solo-developer` interactive and one-shot setup in parity

`ralphus initialize solo-developer` serves both a first-time user at a terminal and a
scripted first-time installation. Treat its prompts and its flags as two
interfaces to the same setup choices:

- Every new interactive question needs an explicit flag that can provide the
  exact answer non-interactively. A bare `--yes` only accepts the default; it
  does not replace a flag for a non-default value.
- Every new initialization flag needs an equivalent prompt in the interactive
  flow. Do not create script-only configuration that a terminal user cannot
  choose.
- Keep conditions and defaults aligned: if an answer controls whether a later
  question is relevant, both interfaces must take the same branch for the
  same answer. Route both through the same setup option rather than keeping
  separate prompt-only and flag-only state.
- Add parser coverage for the full non-interactive invocation and for invalid
  constrained flag values whenever a setting is added or changed.

This rule applies to every value, including optional hosts, empty/disabled
thresholds, and credentials. Sensitive values must never appear in CLI debug
output or normal command rendering.

## Mailbox errors carry remediation guidance (RAL-502)

Any new mailbox message that reports a failure, a blocked state, or another
error condition must go through `Store::enqueue_error_mailbox_message` (or
`Store::notify_watchers_with_remediation` for a Monitor-tagged notification
tied to a `SquadFailed`/`ReviewFailed` event) in `daemon/src/mailbox.rs` --
never the plain `enqueue_mailbox_message`/`enqueue_mailbox_message_ex`, which
have no remediation parameter and are reserved for purely informational
notices (status changes, heads-ups with no failure to act on).

Both required-remediation calls take a `&Remediation`, whose three variants
mirror `core/src/health_catalog.rs`'s existing `remediation: &'static str`
convention for `ralphus check` findings, but as a small enum instead of free
text so a caller can't hand-wave past picking one:

- `Remediation::AutoFix { action }` -- only when the daemon already performed
  the safe corrective action before this message was enqueued.
- `Remediation::SuggestedCommand { command, purpose }` -- a concrete,
  non-destructive CLI command the recipient can run. Never a destructive or
  unsafe command dressed up as a suggestion.
- `Remediation::ManualInterventionRequired { guidance }` -- no safe automatic
  or scripted fix exists; say what a human must inspect or decide. When in
  doubt between a command and manual guidance, prefer this variant --
  presenting an unsafe or merely-plausible command as a fix is worse than
  admitting none exists.

Prefer naming the exact entity affected (an `EntityUri`, a selector, an id)
in the guidance text over a generic pointer, so the recipient doesn't have to
go hunting for which squad/review/cell the message is about.

**One deliberate exception (RAL-565):** the outcome of a manual check a person
or auto-run started -- `NotifiableEventKind::ReviewAutoRunCheckFailed` -- is
that check's own *result*, not a fault in ralphus or a blocked state. A check
failing usually needs no remediation and never changes the review's state, so
it goes through plain `notify_watchers`. Do not extend this to anything that
reports ralphus itself failing to run something (a preparation failure, a
terminal that would not open): those still carry remediation.

## Every retry loop must notify on exhaustion, once (RAL-504)

Any mechanism in this codebase that retries an operation a bounded number of
times before giving up (a provider/harness rate-limit retry, a session
reattach loop, a PR auto-fix campaign, or any future one) must send a user
notification when it finally gives up — never silently. A retry that is
still in progress is recoverable and must stay non-terminal (no notification
for each failed attempt, and no notification while a later attempt could
still succeed); the moment the loop's own cap is hit and it will not retry
again, fire exactly one notification for that terminal transition.

That notification must, at minimum:

- name the specific operation that stalled (the cell, proof step, review, or
  PR affected — not just "something failed");
- say plainly that automated retries have stopped, so the recipient does not
  wait for a retry that isn't coming; and
- give a specific next action — the precise command to retry (e.g.
  `ralphus cell restart-proof <squad/task/cell> --from <index>` when the
  exact failing proof index is known, `ralphus review reopen <id>`, `ralphus
  cell restart ...`), or say plainly that manual investigation is required
  when no safe command exists.

Send it through the existing mandatory-remediation mailbox path from the
rule above (`Store::enqueue_error_mailbox_message` /
`Store::notify_watchers_with_remediation`) — do not invent a second,
parallel notification mechanism for a new retry loop. Prefer enriching an
*existing* terminal failure notification for that operation (branching on
whether the failure is retry-exhaustion) over adding a distinct new message,
so a recipient watching one entity still gets exactly one failure
notification per real failure. Use `crate::mailbox::is_retry_exhaustion_error`
as the reference for how an existing exhaustion message is distinguished
from an ordinary failure by its wording, and add any new retry loop's own
terminal message text to that classifier (and its test) so it is recognized
too. Once retries are genuinely exhausted, `MailboxPriority::High` is
enough — reserve `Urgent` for failures that are not a routine, expected
"the retry budget ran out".

## Report what you could not verify

If part of the suite could not run (see the dev-daemon exe lock in
[`gotchas.md`](gotchas.md)), say which tests were skipped and why. Never let
a partial run be reported as full coverage.

Related: [`gotchas.md`](gotchas.md) for the environmental pitfalls these rules
keep bumping into.
