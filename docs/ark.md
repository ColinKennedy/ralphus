# Ark worktree lifecycle

Ark periodically examines every registered git project. Its default policy is
to inspect once per day, consider terminal entities stale after 90 days, and
keep at most 100 ralphus-owned registered worktrees. Configure these values in
the project repository's `.ralphus.toml`:

```toml
[ark]
sweep_interval_days = 1
stale_after_days = 90
max_worktrees = 100
```

`ralphus check health` rejects zero, negative, incorrectly typed, and unknown
Ark settings.

The scheduled sweep only detects stale worktrees and sends one durable mailbox
escalation for each old review. Open reviews are never deletion candidates,
but old open reviews are escalated for a human to inspect. Automatic deletion
is intentionally disabled.

The implemented explicit reap path refuses a worktree when it has uncommitted
changes, any owner is active, any daemon concurrency permit is held, or `HEAD`
is not contained by the branch currently advertised by its upstream remote.
It then pins `HEAD` under `refs/ralphus/ark/<kind>/<entity>/<timestamp>` before
calling `git worktree remove`.

To restore manually, prefer the remote branch reported by the reap operation:

```console
git worktree add -b restored-feature ../restored origin/feature
```

The Ark ref is a secondary local recovery handle. If it is still present, it
can be used directly:

```console
git worktree add --detach ../restored refs/ralphus/ark/squad/squad-1/1700000000000
```
