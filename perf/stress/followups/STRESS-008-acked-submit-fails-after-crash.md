# STRESS-008: a submit acknowledged with 201 can end up `failed` after a crash

- **Severity:** medium. No data is lost, but a client was told "accepted"
  for work the daemon then abandons and that needs a manual retry.
- **Found by:** `ralphus-stress` scenario `crash_restart`
- **Found at:** `035ef1e70` (staging, 2026-10-18)
- **Status:** open

## Evidence

The daemon was hard-killed 5 s into a 16-user submit load and restarted on
the same database:

- Restart to first answer: 6.3 s.
- Durability: all 390 acknowledged submits were still present afterwards.
- On startup the daemon logged:

  ```
  ralphus [recovery] squad squad-#: materializing → failed (orphaned on startup)   (x2)
  ```

  The two squads that were in their background materialization step when
  the process died were moved to `failed`.

## Root cause

- **Submit returns before materialization finishes.** It answers `201`
  with `state: materializing` (`server.rs:6870` onward). The rest happens
  in a background follow-up (`run_submit_followup`, around `server.rs:7320`).
- **Recovery fails anything still materializing, by design.** On restart,
  `Store::recover_orphaned_squads` (`daemon/src/store.rs:10599`) moves every
  `materializing` squad to `failed` (`store.rs:10645-10690`). The comment
  there says re-running materialization "isn't wired to anything the
  scheduler can pick up on its own", and that a human resubmits or runs
  `ralphus squad retry`.

## Fix direction

Make materialization restartable. Every phase is already persisted
(`set_squad_materialization_phase`, `server.rs:7192`), so recovery could
re-enqueue the follow-up for `materializing` squads instead of failing them.
At minimum, a failure here should send a mailbox message with remediation
guidance (AGENTS.md, RAL-502), so the submitter learns the squad needs a
retry.

## Harness follow-up

The crash check currently counts an acknowledged squad as "present" even
when its state is `failed`. It should also report how many acknowledged
squads came back `failed`.
