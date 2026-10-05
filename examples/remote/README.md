# Live remote-machine checks

Real task files that drive every remote path through the loopback machine
provider ([`../providers/loopback.py`](../providers/loopback.py)). They
complement `ralphus initialize <exercise> --remote` (deterministic, run in
CI by `scripts/check-initialize-exercises.sh`) with the paths that need an agent
or a human: Ollama prompt cells and proofs, agent conflict resolution, review
preparation, and cancel/restart.

Set up a daemon as described at the top of [`setup.sh`](setup.sh) (a
`loopback:lb` machine target, ideally `RALPHUS_LOOPBACK_STRICT=1`), run
`bash examples/remote/setup.sh <work-dir>`, then `ralphus submit` each file.

| File | Covers | Expected outcome |
|---|---|---|
| `l1-remote-cell-proofs.toml` | worktree provisioned under `remote_root`; command cell commits and pushes on the machine; cell env and proof env reach the remote runner; cell + task proofs run remotely | squad `done`; proof output shows `LIVE_ENV=from-cell` / `from-proof` |
| `l2-remote-review.toml` | two stacked remote tasks feeding a review whose merge runs on the machine; the project check gate (`python check.py`) runs remotely | review `in_review`, both branches `done` |
| `l3-remote-agents-conflict.toml` | Ollama prompt cell + prompt proof on the machine; a deliberate rebase conflict resolved by the review's Ollama agent on the machine; `[[review.prepare]]` with its own env in the post-merge checkout | review `in_review`, second branch `conflict_resolved`; `prepared.txt` in `review-postmerge` reads `from-prepare` |
| `l4-cancel-restart.toml` | `squad cancel`, `cell restart`, `machine cleanup` against a running remote cell | the remote process dies on cancel; restart re-dispatches to the machine |

L3 needs a local Ollama with `qwen3:8b`; its agent outcomes depend on the
model, so a failing prompt proof there is worth reading before treating it as
an infrastructure failure. `ralphus review feedback <review>~0 "<text>"` on the
L3 review also exercises the remote feedback round (agent, commit, and push
all on the machine).
