# vendor/

`vendor/psmux` is a git submodule of ralphus's fork of psmux,
`https://github.com/ColinKennedy/psmux.git` (`.gitmodules`), tracking the
fork's `ralphus` branch. psmux is the Windows tmux-alternative ralphus drives
(`daemon/src/tmux.rs`, `daemon/src/psmux_client.rs`); the release build embeds
it and CI's `psmux-integration` job builds it from this submodule.

## Never commit to `ralphus` or `master` directly

- **`master`** mirrors upstream psmux (`https://github.com/psmux/psmux.git`).
  It is how upstream changes are ingested. Never commit to it, rebase it, or
  push anything to it other than what came from upstream.
- **`ralphus`** is the integration branch: upstream psmux plus ralphus's own
  changes, landed only by a deliberate final merge. Never commit, push,
  rebase, or force-push to it directly.

All work on psmux for ralphus happens on a **feature branch** in the fork,
cut from `ralphus` (e.g. `ralphus-pipe-max-bytes`), pushed to the fork, and
merged into `ralphus` by the maintainer. While a feature branch is unmerged,
the submodule gitlink may pin that branch's commit; once it is merged, re-pin
to the merge commit on `ralphus`.

To pin a new psmux commit from a ralphus worktree without checking the
submodule out: `git update-index --cacheinfo 160000,<full-sha>,vendor/psmux`.

## ralphus changes carried on the fork

| Feature branch | Change | Used by |
|---|---|---|
| `ralphus-pipe-max-bytes` | `pipe-max-bytes` server option: a byte cap (with one truncation marker) for `pipe-pane`'s in-server direct file sink; read-only `#{pipe-max-bytes-effective}` format reporting the applied cap; `pipe-pane -F <path>`, a direct file sink that takes the path literally (any filename; UNC/device/remote paths are still refused) | `daemon/src/tmux.rs` `start_file_sink_transcript` |
