# Exhibits: what a reviewer sees in a PR

An **exhibit** is a GIF, video, screenshot or terminal text of a change working,
put into the PR/MR description automatically. Exhibits come from a review's
**live actions** -- the agent-run live tests that already start the real
program -- so they work for any project ralphus can run: a web app, a CLI,
a Qt desktop app, anything with output a person would look at.

## How it works

1. **The agent that made the change asks for exhibits.** Its live handoff (the
   `RALPHUS_LIVE_ACTION:` steps every prompt cell writes) can carry up to four
   lines like

   ```
   EXHIBIT: gif | dragging the zoom slider from 0 to 1 magnifies the chart
   EXHIBIT: screenshot | the chart at 50% zoom
   ```

   Kinds are `gif`, `video`, `screenshot` and `terminal-text` (the text a
   command printed, or lines from a log, shown as a text block of at most
   4,000 characters; the earlier name `output` is still accepted). The agent is
   told to ask only for what a reviewer must *see*.

   A request can list fallbacks after `||`, in order, each with its own
   description:

   ```
   EXHIBIT: gif | dragging the zoom slider from 0 to 1 magnifies the chart || screenshot | the chart magnified at zoom 1
   ```

   It is still one slot. Whatever is delivered is described by the pair that
   matches the delivered kind (PR checklist, caption fallback, image alt text,
   board panel). Every fallback needs its own description; one without is
   dropped and logged. A review's `exhibit_kind_preference` narrows the list,
   keeping order and descriptions.

2. **Each request becomes a slot in the PR.** When live actions are created,
   every request follows its handoff into exactly one live action. The PR of
   the branch that action covers gets a managed `## Exhibits` section with
   one block per slot:

   ```markdown
   <!-- ralphus:exhibit:start action=0 slot=0 -->
   - [ ] **GIF** — dragging the zoom slider from 0 to 1 magnifies the chart _(live action: Zoom slider)_
   <!-- ralphus:exhibit:end action=0 slot=0 -->
   ```

   A cell can add `| compare=review-upstream` to a request to suggest showing
   how the program looked before the change (the review's base branch on its
   remote; the only value). This is a hint for the PR writer, not a decision.

3. **The PR writer may add more** (`infer_exhibits`, on by default). When it
   writes the description it sees the cells' prompts, the diff, and the slots
   already asked for, and can add `EXHIBIT_REQUEST:` lines for anything else a
   reviewer needs to see. A request names the live action that should
   capture it; if the branch has no live action yet, it waits and is
   assigned when they are created (a branch with none gets a small
   "Exhibits" live action of its own). The writer also sees the finished
   cells' `EXHIBIT:` lines for the branch, including `compare=review-upstream`
   hints, before any slot exists for them.

   **Placing a request in the description, and before/after.** A request may
   carry a `"key"` and `"compare": "review-upstream"`. The writer can then put
   `<<exhibit:KEY:after>>` and `<<exhibit:KEY:review-upstream>>` anywhere in the
   description, including inside a markdown table cell, in whatever layout suits
   the change. Each token becomes a marker pair; ralphus fills only the text
   between the markers (media plus a short caption) and imposes no layout. The
   markers are fixed when the PR is created. Ralphus never appends a
   `review-upstream` block, and it needs an upstream capture only for
   `review-upstream` markers that are present in the description; a slot with
   the hint but no such marker needs none. A description with no tokens renders
   exactly as before.

   **Projects can also require exhibits deterministically**, with recipes in
   any `.ralphus.toml`:

   ```toml
   [[exhibit.recipe]]
   name = "flow-recording"
   kind = "gif"                      # gif | video | screenshot | terminal-text
   describe = "the checkout flow in tests/e2e/smoke.spec.ts"
   paths = ["src/**", "*.tsx"]      # relative to this .ralphus.toml's folder
   command = "node tools/record_flow.js --out <<RALPHUS_EXHIBIT_DIR>>"
   ```

   `<<RALPHUS_EXHIBIT_DIR>>` in a recipe's `command` becomes the run's exhibit
   directory on whichever machine runs the live action.

   When a PR is written, each changed file is checked against the recipes in
   the `.ralphus.toml` files from its own folder up to the repository root;
   the nearest file wins for a recipe `name`, so a subfolder refines or
   overrides a parent's recipe only for the files inside it. A matching
   recipe becomes a slot like any other (its `command` is passed to the
   capturing agent). An unreadable or invalid recipe file is skipped and
   logged. Recipes apply whenever `exhibits_enabled` is on, regardless of
   `infer_exhibits`.

4. **The live run captures them.** The run's prompt lists its own action's
   slots and names a directory, which the runner creates on the machine
   running the live action (the review's machine, local or remote) in that
   machine's per-user cache; on the daemon's own machine
   `$RALPHUS_EXHIBIT_DIR` names it too. The agent saves a file per slot and
   reports it with a `RALPHUS_EXHIBIT:` line. Slots it
   leaves unreported get one more turn -- the same agent session resumed on
   claude-code, codex and pi; a fresh run, given the first run's report, on
   backends that cannot resume (ollama). A run can only fill its own
   action's slots.

5. **The capturing machine fits and uploads them.** When the live run ends,
   the machine that ran it fits each file to what both forges embed (see
   limits below; trimming and converting use `ffmpeg` when that machine has
   it), stepping down the review's kind preference when it cannot -- a GIF
   that will not shrink under 10 MiB becomes a video, then a still. Every GIF
   ralphus encodes (this shrinking, and `ralphus exhibit record`) uses an
   undithered palette (`paletteuse=dither=none`): dithering speckles the flat
   background of an app window. A GIF that already fits is uploaded as it
   is, so the run's prompt hands the agent the same ffmpeg filter for GIFs it
   encodes itself. The machine then uploads the file straight to the forge's REST API, into the repository the
   review's PRs/MRs are filed in -- an upload needs no PR/MR to exist yet.
   The daemon never receives the file: it records the URL, and when the
   description is next written the slot's block shows the image or player.
   The forge token reaches a remote machine only in the environment of that
   one upload, never in the spec the runner reads or the agent's
   environment. A re-run replaces a slot's exhibit (on GitLab the old upload
   is deleted once the new one is in).

   When a run on the daemon's own machine cannot upload yet (no forge token,
   the forge refused), its file is kept in the daemon machine's cache and
   uploaded when the PR/MR description is next written, with the `gh`
   fallback below. A remote machine keeps nothing: a file it cannot upload is
   reported missing with the reason. The remote machine's `ralphus-runner`
   must be at least as new as the daemon for this; an older one leaves the
   slots missing with a note saying so.

## Capturing

Agents use whatever the project already has (Playwright, a screen-capture
script, a Qt test recording, ...). Where nothing better exists, ralphus
ships a portable fallback that needs only the OS's own tools:

```bash
ralphus exhibit screenshot --pid <process id> -o view.png     # one window
ralphus exhibit screenshot --window "My App" -o view.png      # one window, by title
ralphus exhibit screenshot --screen -o view.png               # the screen
ralphus exhibit record --pid <process id> --seconds 5 -o drag.gif
ralphus exhibit record --screen --seconds 5 --fps 10 -o drag.mp4   # needs ffmpeg
```

A relative `-o` lands in `$RALPHUS_EXHIBIT_DIR` when that is set. On X11,
`--pid` finds a window through the `_NET_WM_PID` its toolkit records; GTK and
Qt record it, but some toolkits (Tk, for one) never do, so use `--window`
with the title for those.

| OS | Stills | Recording |
|---|---|---|
| Windows | PowerShell + .NET; one window via `PrintWindow` (need not be in front), cut to its visible frame, title bar included | one window: `PrintWindow` frames from one PowerShell process (framed like the still; ends when the window is closed or minimized), encoded with ffmpeg or, without it, in-process; the screen: ffmpeg `gdigrab`, else stills encoded into a GIF |
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

GitHub uploads use the token ralphus already has. For a live run on the
daemon's own machine, when that upload fails and
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

- Before/after comparisons are not captured.
- A slot whose markers someone edited by hand is skipped (and logged), never
  rewritten.
