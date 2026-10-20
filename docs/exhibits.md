# Exhibits: what a reviewer sees in a PR

An **exhibit** is a GIF, video, screenshot or text output of a change working,
put into the PR/MR description automatically. Exhibits come from a review's
**live actions** -- the agent-run live tests that already start the real
program -- so they work for any project ralphus can run: a web app, a CLI,
Houdini, Maya, anything with output a person would look at.

## How it works

1. **The agent that made the change asks for exhibits.** Its live handoff (the
   `RALPHUS_LIVE_ACTION:` steps every prompt cell writes) can carry up to four
   lines like

   ```
   EXHIBIT: gif | dragging the density slider from 0 to 1 thins the points
   EXHIBIT: screenshot | the viewport at density 0.5
   ```

   Kinds are `gif`, `video`, `screenshot` and `output` (text). The agent is
   told to ask only for what a reviewer must *see*.

2. **Each request becomes a slot in the PR.** When live actions are created,
   every request follows its handoff into exactly one live action. The PR of
   the branch that action covers gets a managed `## Exhibits` section with
   one block per slot:

   ```markdown
   <!-- ralphus:exhibit:start action=0 slot=0 -->
   - [ ] **GIF** — dragging the density slider from 0 to 1 thins the points _(live action: Scatter slider)_
   <!-- ralphus:exhibit:end action=0 slot=0 -->
   ```

3. **The PR writer may add more** (`infer_exhibits`, on by default). When it
   writes the description it sees the cells' prompts, the diff, and the slots
   already asked for, and can add `EXHIBIT_REQUEST:` lines for anything else a
   reviewer needs to see. A request names the live action that should
   capture it; if the branch has no live action yet, it waits and is
   assigned when they are created (a branch with none gets a small
   "Exhibits" live action of its own).

4. **The live run captures them.** The run's prompt lists its own action's
   slots and gives it a directory (`$RALPHUS_EXHIBIT_DIR`); the agent saves a
   file per slot and reports it with a `RALPHUS_EXHIBIT:` line. Slots it
   leaves unreported get one more turn -- the same agent session resumed on
   claude-code, codex and pi; a fresh run, given the first run's report, on
   backends that cannot resume (ollama). A run can only fill its own
   action's slots.

5. **Files are fitted and uploaded.** The capturing machine fits each file to
   what both forges embed (see limits below), stepping down the review's kind
   preference when it cannot -- a GIF that will not shrink under 10 MiB
   becomes a video, then a still. When the description is next written the
   file is uploaded through the forge's REST API and the slot's block is
   replaced with the image or player. A re-run replaces a slot's exhibit (on
   GitLab the old upload is deleted once the new one is in).

## Capturing

Agents use whatever the project already has (Playwright, a `hython`
flipbook, a `mayapy` playblast, ...). Where nothing better exists, ralphus
ships a portable fallback that needs only the OS's own tools:

```bash
ralphus exhibit screenshot --pid <process id> -o view.png     # one window
ralphus exhibit screenshot --screen -o view.png               # the screen
ralphus exhibit record --pid <process id> --seconds 5 -o drag.gif
ralphus exhibit record --screen --seconds 5 --fps 10 -o drag.mp4   # needs ffmpeg
```

A relative `-o` lands in `$RALPHUS_EXHIBIT_DIR` when that is set.

| OS | Stills | Recording |
|---|---|---|
| Windows | PowerShell + .NET; one window via `PrintWindow` (need not be in front) | ffmpeg `gdigrab`, else stills encoded into a GIF |
| macOS | `screencapture` (screen only) | ffmpeg `avfoundation`, else stills |
| Linux | `grim` (Wayland), ImageMagick `import` (X11; one window with `xdotool`), `gnome-screenshot` | ffmpeg `x11grab`, else stills |

## Forge limits (measured)

| | GitHub (`user-attachments`) | GitLab (`/projects/:id/uploads`) |
|---|---|---|
| Images / GIF | PNG, GIF, WebP, SVG; ≤ 10 MiB | any; ≤ 100 MiB |
| Video | MP4, WebM, MOV; ≤ 100 MiB (paid plans; Free: 10 MB per GitHub's changelog, untested) | ≤ 100 MiB |
| Other files | rejected | uploaded as a link |
| Delete an upload | not possible | yes |
| Private repo | file is private (anonymous 404) | file is private (sign-in) |

GitHub uploads use the token ralphus already has. When that upload fails and
`gh` 2.99+ is installed and logged in, `gh pr edit --attach` is the fallback
(ralphus restores the description afterwards, so it stays the only writer of
what the description says). `ralphus check health` warns a GitHub project
when `gh` is missing, older than 2.99 or logged out.

## Settings

Each layers built-in default → global/`.ralphus.toml` `[review]` → project
review settings → the review's own setting, resolved whenever the review is
read.

| Setting | Default | |
|---|---|---|
| `exhibits_enabled` | `true` | show exhibits in PR/MR descriptions at all |
| `exhibits_max_per_pr` | `6` | slots one PR shows (≥ 1); handoff requests are kept before inferred ones |
| `infer_exhibits` | `true` | let the PR writer add requests |
| `exhibit_kind_preference` | `["gif", "video", "screenshot"]` | delivery order for visual slots |
| `exhibit_max_video_seconds` | `15` | longer recordings are trimmed |

Set them with `ralphus review settings <review> --exhibits-max-per-pr 3`,
`ralphus project review-settings set <project> --exhibit-kind-preference gif,screenshot`,
or the board's review and project settings.

## Limits of this version

- Live runs on a **remote review machine** do not capture exhibits yet; their
  slots are marked "not supported yet".
- Before/after comparisons are not captured.
- A slot whose markers someone edited by hand is skipped (and logged), never
  rewritten.
