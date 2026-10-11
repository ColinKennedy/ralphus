# cli/src/commands/initialize/

The `ralphus initialize ...` commands. Two kinds live here:

- **Guided exercises** — `machine`, `mailbox`, `review`, `triage`, `waypoint`.
  Each starts its own throwaway daemon through `exercise.rs` (`Exercise`),
  builds a fixture, drives it live, and prints what it proved. The canonical
  list is `EXERCISES` in `mod.rs`.
- **`solo-developer`** — the interactive, one-shot new-machine solo-developer setup. Not an exercise;
  its prompt/flag symmetry rules are in
  [`.agent/agent-conduct.md`](../../../../.agent/agent-conduct.md).

(`initialize git` is a one-liner in `../misc.rs`, not part of this folder.)

## The one exemption: `followup`

`followup.rs` (`ralphus initialize followup`) proves the deferred-prophecy
follow-up flow end to end. It cannot follow the rules below: a `deferred`
prophecy is only ever written by a prompt cell (`RALPHUS_PROPHECY:` markers
are parsed from an agent's reply), the follow-up squad it drafts is always
prompt cells, and its review merge is a git fast-forward in the daemon's own
checkout, which has no `--remote` form. So it is not in `EXERCISES`; it is in
`NON_EXERCISE_MODULES` in `cli/tests/initialize_exercise_ci_parity.rs` with
that reason, and `scripts/check-initialize-exercises.sh` still runs it in CI,
locally only. It still calls no model: its "agent" is a `raw` profile whose
executable is a renamed copy of the `ralphus` binary that prints canned
replies. Do not add another exemption without the same written reason.

## Every exercise must work in every mode, with no LLM, and run in CI

These are the documented first-run checks, so they are tests as much as demos.
All of the following hold for **every** exercise, with no exceptions:

1. **Every exercise works in every mode.** Each one must pass both locally
   (no flag) and fully remote (`--remote`: every cell and review on the strict
   loopback machine, `REMOTE_MACHINE`). Adding an exercise means it works in
   both. The only allowed exception is one that is remote by nature (`machine`),
   and its `--remote`-ness must be explicit in the code and in
   `scripts/check-initialize-exercises.sh`.
2. **Every exercise works alongside every other exercise.** Each owns its
   state root, database, token, port (`reserve_port`), and daemon. Never share
   a fixed port, path, project name, or machine name between exercises, and
   never touch the user's real daemon or global config. Any exercise must be
   runnable in any order, or concurrently with another.
   (Isolation covers state, not tmux: a daemon's startup reap kills every
   `ralphus_` tmux server machine-wide, so an exercise must never be run from
   inside a ralphus cell or feedback pass; see
   [`.agent/agent-conduct.md`](../../../../.agent/agent-conduct.md).)
3. **No LLM, ever.** An exercise must pass with no model, API key, network
   access, or agent backend available. Use raw `command` cells/proofs only —
   never a `prompt` cell, an agent, or anything that calls
   Claude, Codex, Ollama, or similar. If a behavior can only be shown with a
   model, it is not an exercise.
4. **Local or remote, same code path.** Differences between modes go through
   `ExerciseOptions.remote` / `Exercise::remote()` and `REMOTE_MACHINE`, not a
   second copy of the exercise.
5. **Hooked into CI/CD, and kept hooked in.** CI's `initialize-exercises` job
   (`.github/workflows/ci.yml`) runs `scripts/check-initialize-exercises.sh`,
   which runs every exercise in every mode. A new exercise is not done until
   it is in `EXERCISES`, in that script's list, and green in that job.
   `cli/tests/initialize_exercise_ci_parity.rs` fails the normal
   `cargo test` when any of those drift apart, or when an exercise source
   mentions an LLM backend. Do not weaken that test to get a change in.

## Adding or changing an exercise

- Build on `exercise::Exercise` (`Exercise::start(kind, options)`); take the
  shared `ExerciseOptions` (`--state-dir`, `--remote`, `--stop`).
- Add the module to `mod.rs` and its name to `EXERCISES`; wire the subcommand
  in `../mod.rs` (`parse` and `dispatch`) and `help_map.rs`.
- Add the name to `exercises=` in `scripts/check-initialize-exercises.sh`.
- Run it yourself both ways before calling it done:

  ```bash
  cargo build -p ralphus-cli -p ralphus-daemon -p ralphus-runner
  bash scripts/check-initialize-exercises.sh --only <name>
  ```

  Windows runs only the remote variant in CI (local cells need psmux, which
  only the `psmux-integration` job builds); Ubuntu runs both. Say so plainly if
  you could only run one mode.
- Comments describe the current code only, and logging follows
  [`.agent/logging-policy.md`](../../../../.agent/logging-policy.md): stdout is
  the exercise's output, diagnostics go to stderr with the `ralphus [exercise]`
  prefix.

## The answers file (RAL-576)

`ralphus initialize solo-developer` writes an answers file at the end of each run,
recording every setting's final value and the source it came from (flag,
file, prompt, environment, or default). The file is TOML, lives next to the
global ralphus config (`~/.config/ralphus/answers-latest.toml` + a
timestamped backup), and can be replayed with `--answers-file <path>`:

```bash
ralphus initialize solo-developer --answers-file ~/.config/ralphus/answers-latest.toml [flags]
```

**Precedence**: flag > answers file > interactive prompt/default. A loaded
answers file only fills options a flag left unset. Sensitive values
(forge tokens, URLs with embedded credentials) are redacted in the written
file and must be supplied via flag or environment variable on replay.

Keys are the flag names without `--`, with `-` as `_`:
`--review-auto-submit-pr-stack` becomes `review_auto_submit_pr_stack`.

The answers file format (`version = 1`) includes the `value`, `source`, and
whether the setting was `asked` interactively. Unknown or malformed files
are rejected with a clear error; upgrade is not supported (future versions
will fail with a version mismatch, never silent data loss).
