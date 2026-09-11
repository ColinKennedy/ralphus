      // ---- Liveness signal (RAL-170) ----
      // No new pane output for this long from a still-`running` cell is
      // flagged as possibly-stalled rather than just "quiet" -- long enough
      // to not flag normal pauses between a command's own output lines (a
      // slow build/test step can easily go quiet for tens of seconds), short
      // enough to still be a useful "maybe check on this" signal well before
      // a user would otherwise wonder if something's wrong.
      const PEEK_STALE_WARNING_MS = 60_000;
      /**
       * Formats a liveness timestamp as local "YYYY-MM-DD HH:MM:SS".
       * @param {number} ms
       * @returns {string}
       */
      function fmtActivityTime(ms) {
        const d = new Date(ms);
        const pad = (/** @type {number} */ n) => String(n).padStart(2, "0");
        return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
      }
      /**
       * Formats a duration as a short "Xs" / "XmYs" / "XhYm" relative-age string.
       * @param {number} ms
       * @returns {string}
       */
      function fmtRelativeAge(ms) {
        const totalSec = Math.max(0, Math.floor(ms / 1000));
        if (totalSec < 60) return `${totalSec}s`;
        const totalMin = Math.floor(totalSec / 60);
        if (totalMin < 60) return `${totalMin}m${totalSec % 60}s`;
        const hr = Math.floor(totalMin / 60);
        return `${hr}h${totalMin % 60}m`;
      }
      /**
       * @typedef {object} PeekActivityInfo
       * @property {string} text - Label text for the peek head, or "" if there's no activity data yet.
       * @property {boolean} stale - Whether this has crossed PEEK_STALE_WARNING_MS with no fresh output.
       * @property {string} tip - Tooltip content for the label.
       */
      /**
       * Computes the last-activity label/staleness/tooltip for a peek box's
       * header from its last-fetched `last_activity_ms` (RAL-170). `ended`
       * suppresses the staleness warning — a finished cell's last output
       * isn't "stuck", it's just history.
       * @param {number|null|undefined} lastActivityMs
       * @param {boolean} ended
       * @returns {PeekActivityInfo}
       */
      function peekActivityInfo(lastActivityMs, ended) {
        if (!lastActivityMs) return { text: "", stale: false, tip: "" };
        const ageMs = Date.now() - lastActivityMs;
        const stale = !ended && ageMs >= PEEK_STALE_WARNING_MS;
        const text = `last output ${fmtActivityTime(lastActivityMs)} (${fmtRelativeAge(ageMs)} ago)`;
        const tip = stale
          ? `No new output for ${fmtRelativeAge(ageMs)} even though this cell is still running — it may have stalled.\nA quiet stretch can be normal for some commands (e.g. a slow build/test step) — use this alongside the actual output to judge, not on its own.\nExact last-output time: ${fmtActivityTime(lastActivityMs)}.`
          : `Time the daemon last observed fresh output on this cell's terminal.\nExact time: ${fmtActivityTime(lastActivityMs)}.`;
        return { text, stale, tip };
      }
      /**
       * Re-renders a peek box's last-activity label in place from
       * `peekLastActivity`/`peekEnded`, without needing a full re-render.
       * @param {string} key
       * @returns {void}
       */
      function updatePeekActivityLabel(key) {
        const el = document.getElementById(`peek-activity-${peekCssKey(key)}`);
        if (!el) return;
        const info = peekActivityInfo(peekLastActivity[key], !!peekEnded[key]);
        el.textContent = info.text;
        el.className = "peek-activity" + (info.stale ? " stale" : "");
        if (info.text) el.setAttribute("data-tip", info.tip);
        else el.removeAttribute("data-tip");
      }
      // Renders the expandable box itself (empty string when collapsed) —
      // call right after the button row that includes a matching togglePeek().
      /**
       * Renders the expandable live-pane peek box for a given key, or an
       * empty string when collapsed.
       * @param {string} key
       * @param {number|null} [startedAtMs] - RAL-210/RAL-259: the most recent
       *   start time (epoch ms, local time) of the agent this peek shows —
       *   a task cell's run, a review branch's conflict-resolver/final-proof
       *   cell, or a review's manual-checks generation pass. Shown in the
       *   header when given.
       * @param {number|null} [detachedAtMs] - RAL-288: set only for a task
       *   cell that is cleanly detached (see `detachedBadge`) rather than
       *   genuinely done/failed/cancelled — swaps the "cell ended" wording
       *   below for one that doesn't misread as stuck/stalled.
       * @returns {string}
       */
      function peekBox(key, startedAtMs, detachedAtMs) {
        if (!peekOpen[key]) return "";
        // Reuse the last-fetched content (if any) instead of always starting from
        // "Loading…" — the details pane fully re-renders on every pushed event
        // (unrelated state changes elsewhere on the squad), and resetting this
        // box's height/text each time produced a visible flash/layout-jump even
        // though nothing about the terminal itself had changed (RAL-102 follow-up).
        const cached = peekContent[key];
        const cssKey = peekCssKey(key);
        // Confirmed-ended (not just a single missed poll — see
        // PEEK_MISSING_STRIKE_LIMIT/fetchPeek) shows as a distinct read-only
        // historical record rather than the live auto-refreshing view, per
        // docs/colors.md's "read-only field indicator" convention: --muted,
        // never the caution-reserved --ignored or --danger/--failed.
        const ended = !!peekEnded[key];
        const detached = ended && !!detachedAtMs;
        const headLabel = detached
          ? "Historical record (read-only) — cell detached"
          : ended
            ? "Historical record (read-only) — cell ended"
            : "Live terminal (read-only)";
        const headTip = detached
          ? "This tmux cell was cleanly stopped so a real interactive agent session could take over — not a hang. What's shown is the daemon's persisted record of the stopped process's last captured output, not live output; the live conversation now continues in a separate terminal outside this board.\nRead-only — there was never anything to send to here.\nUse 'Open Agent' to reattach a terminal to that live session, or 'Resume Automation' once done with it — this box switches back to auto-refreshing live output within a poll tick or two once headless execution resumes."
          : ended
            ? "This tmux cell has ended. What's shown is the daemon's persisted record of the pane's last captured output, not live output.\nRead-only — there was never anything to send to here.\nIf a new terminal comes up for this same step — the agent reattaching, or you restarting the cell or proof step — this box switches back to auto-refreshing live output within a poll tick or two on its own. You never need to navigate away and back."
            : "Live output from the agent's tmux pane.\nAuto-refreshes every 2 seconds. Read-only — nothing typed here reaches the agent.\nSwitches to a read-only historical record once the underlying tmux cell ends.";
        // RAL-170: last-activity label, seeded from cached state (if any) so
        // it doesn't blank out on every full re-render -- fetchPeek patches
        // it in place afterward via updatePeekActivityLabel.
        const activity = peekActivityInfo(peekLastActivity[key], ended);
        const activityHtml = activity.text
          ? `<span id="peek-activity-${cssKey}" class="peek-activity${activity.stale ? ' stale' : ''}" data-tip="${activity.tip}">${activity.text}</span>`
          : `<span id="peek-activity-${cssKey}" class="peek-activity"></span>`;
        // RAL-210: cell start time, when the caller has one to show.
        // Re-derived from the (freshly re-rendered) cell data on every
        // call, so a restart's new started_at_ms shows up immediately.
        const startedHtml = startedAtMs
          ? `<span class="peek-activity" data-tip="When this step (cell, conflict resolver, or manual-checks generation) most recently started running.\nUpdates to a new time if it is restarted.\nExact time: ${fmtActivityTime(startedAtMs)}.">started ${fmtActivityTime(startedAtMs)}</span>`
          : "";
        // RAL-397 Phase 2G-A: the Live View renders the transcript tape run
        // through the ANSI-strip/classify pipeline; `peekContent[key]` already
        // holds that rendered text, so it's shown verbatim here. "Show Debug
        // Messages" flips whether the tape's own inline RALPHUS_EVENT telemetry
        // lines are rendered in place or dropped -- no separate stream, no
        // prepended block (that richer Cartographer stream returns in 2G-B).
        const showDebug = peekShowsDebug(key);
        const shown = cached;
        const debugToggleHtml = `<label class="peek-debug-toggle" data-tip="Show ralphus's own diagnostic/telemetry events (session lifecycle, token/cost RALPHUS_EVENT markers) inline, right where they occurred in the terminal output.\nOff by default so routine monitoring only shows what the agent did; the default can be changed globally via the ralphus config file's [live_view] table.\nThis only changes what's rendered here -- the daemon's own logs always keep everything."><input type="checkbox" ${showDebug ? "checked" : ""} onchange="toggleShowDebugMessages('${esc(key)}',this.checked)"> Show Debug Messages</label>`;
        return `<div class="peek-box" data-tip="${headTip}">
            <div class="peek-head"><span><span class="peek-dot${ended ? ' ended' : ''}"></span>${headLabel}${startedHtml}${activityHtml}</span><span style="display:flex;gap:8px;align-items:center">${debugToggleHtml}<button class="copy-btn" data-tip="Copy this terminal's current output to clipboard.\nCopies whatever is visible right now — the live view keeps auto-refreshing after." data-click="copyPeekText" data-key="${esc(key)}">⧉</button><button class="btn" style="padding:1px 7px;font-size:11px" data-click="togglePeekStopProp" data-key="${esc(key)}" data-tip="Collapse this live view.">✕ Hide</button></span></div>
            <div class="peek-pre-wrap">
              <pre id="peek-pre-${cssKey}" class="peek-pre" style="height:${peekPaneHeight}px" tabindex="0" data-key="${esc(key)}" onscroll="onPeekScroll(this.dataset.key)" onkeydown="handlePeekKeydown(event,this.dataset.key)" data-tip="Scroll through the live terminal output.\nClick here then press Ctrl+End to jump to the latest output, or Ctrl+Home to jump to the start.">${shown !== undefined ? esc(shown) : "Loading…"}</pre>
              <button id="peek-jump-${cssKey}" class="peek-jump-btn" style="display:none" data-click="peekScrollToBottom" data-key="${esc(key)}" data-tip="Jump to the latest output.\nAppears once you've scrolled up from the bottom — also triggerable with Ctrl+End while the terminal is focused.">↓ Jump to latest</button>
              <div class="peek-resize-handle" data-peek-key="${key}" data-tip="Drag to resize the live terminal view.\nYour chosen size is kept while you switch between tabs during this browser session."></div>
            </div>
          </div>`;
      }
      /**
       * Writes a plain status message into a peek box's `<pre>`, re-resolving
       * the element by id first — see {@link fetchPeek} for why a reference
       * captured before an `await` cannot be trusted here.
       * @param {string} preId
       * @param {string} text
       * @returns {void}
       */
      function setPeekPreText(preId, text) {
        const el = document.getElementById(preId);
        if (el) el.textContent = text;
      }
      /**
       * Fetches one peek box's live pane content and patches it into the DOM in
       * place. Scrolls the box to the bottom when `forceBottom` is set (a fresh
       * open — there's no prior scroll position worth preserving) or when the
       * box was already scrolled to its bottom before this update (so it keeps
       * following new output rather than getting stuck wherever it happened to
       * be when the box was last recreated).
       *
       * Every state decision is delegated to the pure {@link nextPeekPaneState};
       * this function is only the DOM/IO shell around it.
       *
       * RAL-186 — two rules this function now holds to, both of which it broke
       * before, and which together left a *restarted* cell's or proof
       * step's Live View frozen on its historical log — text and banner both —
       * until something else happened to re-render the details pane. Since
       * RAL-167 that is an SSE event or the 60s reconciliation fallback, so in
       * practice the box sat stale for up to a minute at exactly the moment the
       * user was watching it most closely, and navigating away and back was the
       * only reliable way to clear it:
       *
       * 1. The `<pre>` is re-resolved by id after **every** `await`, never
       *    reused from before one. `#details` is re-rendered wholesale on each
       *    (SSE-debounced) Cartographer event, and a restart emits a burst of
       *    them, so a reference captured before the fetch is routinely detached
       *    from the document by the time the response lands — patching it
       *    updated nothing the user could see.
       * 2. A full re-render is forced on **both** directions of the ended flip,
       *    not just live→ended. `peekBox()` is the only thing that produces the
       *    head banner, dot and tooltip, and since RAL-167 nothing else
       *    re-renders the details pane on a predictable cadence (SSE push plus
       *    a 60s reconciliation fallback replaced the old unconditional 2s
       *    tick). Losing scroll position on that flip is deliberate and
       *    accepted: the pane's whole contents are being replaced.
       * @param {string} key
       * @param {boolean} forceBottom
       * @returns {Promise<void>}
       */
      async function fetchPeek(key, forceBottom) {
        const preId = `peek-pre-${peekCssKey(key)}`;
        // Not currently rendered (navigated to a different cell/tab) —
        // leave peekOpen alone so the toggle is remembered per-cell
        // (RAL-162) and skip fetching until it's rendered again.
        if (!document.getElementById(preId)) return;
        const tapeUrl = peekTranscriptUrlFor(key);
        if (!tapeUrl) { delete peekOpen[key]; delete peekContent[key]; delete peekTape[key]; return; }
        try {
          const before = document.getElementById(preId);
          if (!before) return;
          const atBottom = forceBottom || before.scrollTop + before.clientHeight >= before.scrollHeight - 4;

          // Liveness + fallback content: poll `/pane` for its authoritative
          // `active` flag (RAL-397 Phase 2G-A) and its rendered snapshot. The
          // transcript tape below is the primary content source, but when no
          // `.raw` transcript exists — an older cell that ran before transcript
          // capture, a still-settling fresh attempt, or a session whose
          // pipe-pane capture never engaged — the `/pane` snapshot is shown
          // instead, so the box never dead-ends on "Waiting for output…". A
          // failed probe leaves `live` null so the tape-growth fallback decides.
          /** @type {boolean|null} */
          let live = null;
          /** @type {string|undefined} */
          let paneContent;
          const liveUrl = peekUrlFor(key);
          if (liveUrl) {
            try {
              const lr = await fetch(liveUrl);
              if (lr.ok) {
                /** @type {PeekPaneResponse} */
                const ld = await lr.json();
                live = ld.active ?? null;
                if (typeof ld.content === "string") paneContent = ld.content;
              }
            } catch (_) { /* leave live=null / paneContent undefined; fallbacks decide */ }
          }

          // Content: page the transcript tape when one exists. Seed the tail on
          // first open (probe total, then fetch the last chunk); afterwards
          // follow the tail by fetching only bytes appended past `loadedEnd`.
          // `usingTape` stays false when there is no `.raw` — then the `/pane`
          // snapshot above is rendered as the fallback.
          const prevTape = peekTape[key];
          const prevTotal = prevTape ? prevTape.total : 0;
          let win = prevTape;
          let usingTape = true;
          if (!win) {
            const probe = await fetchTapeRange(tapeUrl, TAPE_PROBE_OFFSET, 1);
            const seed = probe === null
              ? null
              : await fetchTapeRange(tapeUrl, Math.max(0, probe.total - TAPE_CHUNK_BYTES), TAPE_CHUNK_BYTES);
            if (seed === null) usingTape = false;
            else win = tapeAppend(emptyTapeWindow(), { start: seed.start, content: seed.content, total: seed.total, requested: TAPE_CHUNK_BYTES });
          } else {
            const chunk = await fetchTapeRange(tapeUrl, win.loadedEnd, TAPE_CHUNK_BYTES);
            if (chunk !== null) {
              win = tapeAppend(win, { start: chunk.start, content: chunk.content, total: chunk.total, requested: TAPE_CHUNK_BYTES });
              // Don't drop the front while the user is actively paging older
              // history in (that fetch is mid-flight and about to prepend).
              if (atBottom && !peekLoadingOlder.has(key)) win = tapeTrimFront(win, TAPE_MAX_WINDOW_CHARS);
            }
          }
          let grew = false;
          if (usingTape && win) {
            grew = win.total > prevTotal;
            peekTape[key] = win;
          } else {
            // No transcript available — render the /pane snapshot instead.
            delete peekTape[key];
          }

          /** @type {PeekPaneState} */
          const prev = {
            ended: !!peekEnded[key],
            missingStrikes: peekMissingStrikes[key] || 0,
            lastActivityMs: peekLastActivity[key] ?? null,
          };
          const next = nextPeekPaneState(prev, { live, grew, nowMs: Date.now() }, PEEK_MISSING_STRIKE_LIMIT);
          peekEnded[key] = next.state.ended;
          peekMissingStrikes[key] = next.state.missingStrikes;
          peekLastActivity[key] = next.state.lastActivityMs;

          // Render from the tape when we have one (ANSI-strip/classify pipeline,
          // re-resolving the `<pre>` by id per the RAL-186 rule above), else
          // from the `/pane` snapshot fallback.
          if (usingTape && peekTape[key]) renderPeekTape(key);
          else renderPeekFallback(key, paneContent);
          if (next.headerChanged) {
            if (sel.kind) renderDetails();
            if (selectedGuardian) renderReviewDetail();
          }
          const pre = document.getElementById(preId);
          if (!pre) return;
          // A just-revived (or just-ended) box gets pinned to the bottom
          // regardless of where it was scrolled: its content is a different
          // log now, so the old offset means nothing.
          if (atBottom || next.headerChanged) pre.scrollTop = pre.scrollHeight;
          updatePeekJumpVisibility(key);
          updatePeekActivityLabel(key);
        } catch (_) {
          setPeekPreText(preId, "Could not load terminal output (network error).");
        }
      }
      /**
       * Fetches one byte range of a peek key's transcript tape. Returns null on
       * a 404 (no transcript yet — a fresh attempt's `.raw` file appears a
       * moment after the session starts) or any non-OK response, so callers can
       * treat "not ready" distinctly from a thrown network error.
       * @param {string} baseUrl - the `.../pane-transcript` URL from {@link peekTranscriptUrlFor}.
       * @param {number} offset - byte offset to read from (clamped to the file size server-side).
       * @param {number} limit - maximum bytes to read.
       * @returns {Promise<RawTranscriptRange|null>}
       */
      async function fetchTapeRange(baseUrl, offset, limit) {
        const resp = await fetch(`${baseUrl}?offset=${offset}&limit=${limit}`);
        if (!resp.ok) return null;
        return /** @type {RawTranscriptRange} */ (await resp.json());
      }
      /**
       * Renders peek key `key`'s loaded tape window through the pure line
       * pipeline (ANSI-strip + classify, honoring the per-box Debug toggle),
       * scrubs secrets defensively, applies the ended/empty affordances, caches
       * the result in `peekContent[key]`, and patches it into the `<pre>`.
       * @param {string} key
       * @returns {void}
       */
      function renderPeekTape(key) {
        const w = peekTape[key];
        const ended = !!peekEnded[key];
        let text = w ? renderTapeLines(tapeCompleteLines(w, ended), peekShowsDebug(key)) : "";
        text = scrubSecrets(text);
        if (ended) {
          text = text.trim()
            ? `${text}\n\n[Read-only historical record — this terminal session has ended.]`
            : "Terminal cell has ended. No output was recorded before it ended.";
        } else if (text === "") {
          text = "(no output yet)";
        }
        peekContent[key] = text;
        setPeekPreText(`peek-pre-${peekCssKey(key)}`, text);
      }
      /**
       * Renders peek key `key` from the `/pane` snapshot `paneContent` — the
       * fallback used when no `.raw` transcript exists for the cell (an older
       * cell that predates transcript capture, or one whose pipe-pane capture
       * never engaged). The daemon already strips ralphus's own marker lines and
       * redacts `/pane`, but scrub again defensively; applies the same
       * ended/empty affordances as {@link renderPeekTape}. Inline debug is
       * unavailable in this mode (the markers aren't in the `/pane` snapshot),
       * so the "Show Debug Messages" toggle is inert while falling back.
       * @param {string} key
       * @param {string|undefined} paneContent
       * @returns {void}
       */
      function renderPeekFallback(key, paneContent) {
        const ended = !!peekEnded[key];
        let text = scrubSecrets(paneContent || "");
        if (ended) {
          text = text.trim()
            ? `${text}\n\n[Read-only historical record — this terminal session has ended.]`
            : "Terminal cell has ended. No output was recorded before it ended.";
        } else if (text.trim() === "") {
          text = "(no output yet)";
        }
        peekContent[key] = text;
        setPeekPreText(`peek-pre-${peekCssKey(key)}`, text);
      }
      /**
       * Pages an older chunk of the transcript tape in when the user scrolls
       * near the top (RAL-397 Phase 2G-A), prepending it and preserving the
       * viewport so the content under the user's eyes doesn't jump. Guarded by
       * `peekLoadingOlder` so overlapping scroll events don't stack duplicate
       * fetches.
       * @param {string} key
       * @returns {Promise<void>}
       */
      async function loadOlderPeekTape(key) {
        const w = peekTape[key];
        const tapeUrl = peekTranscriptUrlFor(key);
        if (!w || !tapeUrl || w.loadedStart <= 0 || peekLoadingOlder.has(key)) return;
        peekLoadingOlder.add(key);
        try {
          const start = Math.max(0, w.loadedStart - TAPE_CHUNK_BYTES);
          const chunk = await fetchTapeRange(tapeUrl, start, w.loadedStart - start);
          if (chunk === null) return;
          const cssKey = peekCssKey(key);
          const preBefore = document.getElementById(`peek-pre-${cssKey}`);
          const prevHeight = preBefore ? preBefore.scrollHeight : 0;
          const prevTop = preBefore ? preBefore.scrollTop : 0;
          // Re-read the window: a follow-tail poll may have advanced it while
          // this fetch was in flight (its `loadedStart` is unchanged either way).
          const cur = peekTape[key];
          if (!cur) return;
          peekTape[key] = tapePrepend(cur, { start: chunk.start, content: chunk.content, total: cur.total });
          renderPeekTape(key);
          const preAfter = document.getElementById(`peek-pre-${cssKey}`);
          if (preAfter) preAfter.scrollTop = prevTop + (preAfter.scrollHeight - prevHeight);
        } finally {
          peekLoadingOlder.delete(key);
        }
      }
      /**
       * Shows or hides a peek box's jump-to-latest button based on how far its
       * terminal pane is currently scrolled from the bottom.
       * @param {string} key
       * @returns {void}
       */
      function updatePeekJumpVisibility(key) {
        const cssKey = peekCssKey(key);
        const pre = document.getElementById(`peek-pre-${cssKey}`);
        const btn = document.getElementById(`peek-jump-${cssKey}`);
        if (!pre || !btn) return;
        const distance = pre.scrollHeight - pre.scrollTop - pre.clientHeight;
        // Roughly half a viewport of scrollback, with a small floor so the
        // button doesn't flicker in/out right at the bottom during normal reading.
        const threshold = Math.max(40, pre.clientHeight / 2);
        btn.style.display = distance > threshold ? "" : "none";
      }
      /**
       * Handles a scroll event inside a peek box's terminal pane: updates the
       * jump-to-latest button, and — when scrolled near the top with older
       * transcript still on disk (RAL-397 Phase 2G-A) — pages the previous
       * chunk in seamlessly. Scrolling up crosses no "live vs. saved" boundary:
       * it's all one byte-addressed tape.
       * @param {string} key
       * @returns {void}
       */
      function onPeekScroll(key) {
        updatePeekJumpVisibility(key);
        const pre = document.getElementById(`peek-pre-${peekCssKey(key)}`);
        const w = peekTape[key];
        if (!pre || !w) return;
        if (pre.scrollTop <= TAPE_TOP_TRIGGER_PX && w.loadedStart > 0 && !peekLoadingOlder.has(key)) {
          void loadOlderPeekTape(key);
        }
      }
      /**
       * Scrolls a peek box's terminal pane to the latest (bottom-most) output.
       * @param {string} key
       * @returns {void}
       */
      function peekScrollToBottom(key) {
        const pre = document.getElementById(`peek-pre-${peekCssKey(key)}`);
        if (!pre) return;
        pre.scrollTop = pre.scrollHeight;
        updatePeekJumpVisibility(key);
      }
      /**
       * Scrolls a peek box's terminal pane to the earliest (top-most) output.
       * @param {string} key
       * @returns {void}
       */
      function peekScrollToTop(key) {
        const pre = document.getElementById(`peek-pre-${peekCssKey(key)}`);
        if (!pre) return;
        pre.scrollTop = 0;
        updatePeekJumpVisibility(key);
      }
      /**
       * Handles Ctrl+Home / Ctrl+End inside a focused peek box terminal pane —
       * jumps to the earliest or latest output respectively.
       * @param {KeyboardEvent} e
       * @param {string} key
       * @returns {void}
       */
      function handlePeekKeydown(e, key) {
        if (!e.ctrlKey) return;
        if (e.key === "End") { e.preventDefault(); peekScrollToBottom(key); }
        else if (e.key === "Home") { e.preventDefault(); peekScrollToTop(key); }
      }

      // ---- Durable terminal-log attempt history (RAL-154) ----
      // Sibling feature to the live pane peek above: lists every durably
      // persisted attempt (initial run + each tmux reattach) for a cell's
      // terminal log, and lets any one of them be opened and read in full —
      // unlike the peek box, this survives pane death and daemon restarts,
      // and stays keyed the same way (see peekUrlFor) so the same `key`
      // string works for both features.
      /**
       * Resolves a peek key to the daemon API URL for listing that cell's
       * persisted terminal-log attempts (or reading one's content, when
       * `attempt` is given).
       * @param {string} key
       * @param {number} [attempt]
       * @returns {string|null}
       */
      function terminalLogAttemptsUrlFor(key, attempt) {
        const [kind, ...rest] = key.split("|");
        /** @type {string|null} */
        let base = null;
        if (kind === "cell") { const [squadId, ti, si] = rest; base = `/api/squads/${squadId}/cells/${ti}/${si}/terminal-log-attempts`; }
        else if (kind === "proof") { const [squadId, ti, scope, si, vi] = rest; base = `/api/squads/${squadId}/proofs/${ti}/${scope}/${si}/${vi}/terminal-log-attempts`; }
        else if (kind === "guardian") { const [gid, branchId] = rest; base = `/api/guardians/${gid}/branches/${branchId}/terminal-log-attempts`; }
        else if (kind === "guardian-manual") { const [gid] = rest; base = `/api/guardians/${gid}/manual-checks/terminal-log-attempts`; }
        if (base === null) return null;
        return attempt === undefined ? base : `${base}/${attempt}`;
      }
      /**
       * Toggles the attempt-history box open or closed, keyed the same way as
       * the matching peek box. Fetches the attempt list immediately on open.
       * @param {string} key
       * @returns {void}
       */
      function toggleHistory(key) {
        historyOpen[key] = !historyOpen[key];
        if (!historyOpen[key]) {
          delete historyAttempts[key];
          delete historyViewing[key];
        }
        if (sel.kind) renderDetails();
        if (selectedGuardian) renderReviewDetail();
        if (historyOpen[key]) fetchHistoryList(key);
      }
      /**
       * Fetches the list of persisted terminal-log attempts for `key` and
       * re-renders whichever pane owns it.
       * @param {string} key
       * @returns {Promise<void>}
       */
      async function fetchHistoryList(key) {
        const url = terminalLogAttemptsUrlFor(key);
        if (!url) return;
        try {
          const resp = await fetch(url);
          if (!resp.ok) { historyAttempts[key] = []; return; }
          const data = await resp.json();
          historyAttempts[key] = data.attempts || [];
        } catch (_) {
          historyAttempts[key] = [];
        }
        if (sel.kind) renderDetails();
        if (selectedGuardian) renderReviewDetail();
      }
      /**
       * Fetches one historical attempt's content and shows it inline in the
       * attempt-history box, this key's "Open Terminal Log" popup/expanded
       * view. When `attempt` is this key's *most recent* persisted attempt,
       * RAL-296 routes it through the same merged `.../debug-events` stream
       * "Show Debug Messages" shows inline (lifecycle events plus inlined
       * terminal-log excerpts, current-attempt-only) rather than that
       * attempt's bare, un-merged terminal-log content -- so the two views
       * never disagree. Older attempts (picked from the list) keep reading
       * their own raw, un-merged content: browsing *past* attempts is this
       * box's unchanged job, and only the current attempt is in scope for
       * unification.
       * @param {string} key
       * @param {number} attempt
       * @returns {Promise<void>}
       */
      async function viewHistoryAttempt(key, attempt) {
        const attempts = historyAttempts[key] || [];
        const debugUrl = isMostRecentAttempt(attempts, attempt) ? debugEventsUrlFor(key) : null;
        const url = debugUrl || terminalLogAttemptsUrlFor(key, attempt);
        if (!url) return;
        const merged = !!debugUrl;
        historyViewing[key] = { attempt, content: "Loading…", merged };
        if (sel.kind) renderDetails();
        if (selectedGuardian) renderReviewDetail();
        try {
          const resp = await fetch(url);
          if (!resp.ok) { historyViewing[key] = { attempt, content: "Could not load this attempt's log.", merged }; return; }
          if (debugUrl) {
            /** @type {DebugEventEntry[]} */
            const events = await resp.json();
            historyViewing[key] = {
              attempt,
              content: events.length ? scrubSecrets(events.map(formatDebugEvent).join("\n")) : "(no debug events recorded)",
              merged,
            };
          } else {
            const data = await resp.json();
            historyViewing[key] = { attempt, content: scrubSecrets(data.content || "(empty)"), merged };
          }
        } catch (_) {
          historyViewing[key] = { attempt, content: "Could not load this attempt's log (network error).", merged };
        }
        if (sel.kind) renderDetails();
        if (selectedGuardian) renderReviewDetail();
      }
      /**
       * Returns to the attempt list from the single-attempt content view.
       * @param {string} key
       * @returns {void}
       */
      function closeHistoryAttempt(key) {
        delete historyViewing[key];
        if (sel.kind) renderDetails();
        if (selectedGuardian) renderReviewDetail();
      }
      /**
       * Formats a byte count as a short human-readable size ("1.2 KB").
       * @param {number} bytes
       * @returns {string}
       */
      function fmtAttemptSize(bytes) {
        if (bytes < 1024) return `${bytes} B`;
        if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
        return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
      }
      /**
       * Renders the expandable attempt-history box for a given key, or an
       * empty string when collapsed. Shows either the list of persisted
       * attempts, or (once one is picked) that attempt's full content.
       * @param {string} key
       * @returns {string}
       */
      function historyBox(key) {
        if (!historyOpen[key]) return "";
        const viewing = historyViewing[key];
        if (viewing) {
          const viewingTip = viewing.merged
            ? "This most recent attempt's merged debug stream (RAL-296) — the same lifecycle events plus inlined terminal-log excerpts \"Show Debug Messages\" shows inline, current-attempt-only.\nCan still change if this attempt is actively running; becomes a fixed historical record once it ends."
            : "One older, persisted attempt's full terminal-log content, read from durable on-disk storage (RAL-154).\nThis is a fixed historical record — it never changes, even if the cell later reattaches again.";
          return `<div class="history-box" data-tip="${viewingTip}">
              <div class="history-head"><span>Attempt ${viewing.attempt}</span><button class="btn" style="padding:1px 7px;font-size:11px" data-click="closeHistoryAttempt" data-key="${esc(key)}" data-tip="Back to the list of persisted attempts.">← Back</button></div>
              <div class="peek-pre-wrap"><pre class="peek-pre" style="height:${peekPaneHeight}px">${esc(viewing.content)}</pre></div>
            </div>`;
        }
        const attempts = historyAttempts[key];
        const body = attempts === undefined
          ? `<div style="padding:8px;font-size:11px;color:var(--muted)">Loading…</div>`
          : attempts.length === 0
            ? `<div style="padding:8px;font-size:11px;color:var(--muted)">No persisted attempts yet — a durable log is written once this cell's first tmux attempt finishes.</div>`
            : `<div class="history-list">${attempts.map((a) => `<div class="history-item" data-click="viewHistoryAttempt" data-key="${esc(key)}" data-attempt="${a.attempt}" data-tip="Open attempt ${a.attempt}'s full, durably-persisted terminal-log content.\nEach reattach gets its own attempt file, so this survives pane death and daemon restarts.">
                <span>Attempt ${a.attempt}${a.attempt === 0 ? " (initial)" : " (reattach)"}</span>
                <span style="color:var(--muted)">${fmtAttemptSize(a.size_bytes)} · ${new Date(a.modified_ms).toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" })}</span>
              </div>`).join("")}</div>`;
        return `<div class="history-box" data-tip="Every durably-persisted terminal-log attempt for this cell (RAL-154), including reattaches — kept on disk independent of whether the live tmux pane is still around.">
            <div class="history-head"><span>Attempt history</span><button class="btn" style="padding:1px 7px;font-size:11px" data-click="toggleHistory" data-key="${esc(key)}" data-tip="Collapse this attempt history.">✕ Hide</button></div>
            ${body}
          </div>`;
      }
      // RAL-139: drag-to-resize for peek box terminal panes, using a
      // pointer-capture drag mechanism. Called after
      // any render that may include peek boxes — handles are freshly created DOM
      // nodes each time (innerHTML is replaced), so there's no risk of attaching
      // duplicate listeners to stale elements.
      /**
       * Attaches drag-to-resize handlers to every currently-rendered peek box
       * resize handle.
       * @returns {void}
       */
      function attachPeekResizeHandlers() {
        document.querySelectorAll(".peek-resize-handle").forEach((el0) => {
          const handle = /** @type {HTMLElement} */ (el0);
          const key = handle.getAttribute("data-peek-key");
          if (!key) return;
          handle.addEventListener("pointerdown", (/** @type {PointerEvent} */ e) => {
            e.preventDefault();
            handle.setPointerCapture(e.pointerId);
            const startY = e.clientY;
            const startH = peekPaneHeight;
            const pre = document.getElementById(`peek-pre-${peekCssKey(key)}`);
            /**
             * @param {PointerEvent} ev
             * @returns {void}
             */
            const onMove = ev => {
              const delta = ev.clientY - startY;
              const min = 120, max = Math.floor(window.innerHeight * 0.85);
              peekPaneHeight = Math.min(max, Math.max(min, startH + delta));
              if (pre) pre.style.height = peekPaneHeight + "px";
            };
            const onUp = () => {
              handle.removeEventListener("pointermove", onMove);
              handle.removeEventListener("pointerup", onUp);
              updatePeekJumpVisibility(key);
            };
            handle.addEventListener("pointermove", onMove);
            handle.addEventListener("pointerup", onUp);
          });
        });
      }
      // RAL-167: on its own dedicated interval (see the bottom of this
      // script), independent of the main SSE-driven tick() -- live-terminal
      // content isn't Cartographer-backed, so push can't drive it. Refreshes
      // every currently-expanded peek box in place, without touching the
      // rest of the DOM.
      /**
       * Refreshes every currently-expanded peek box in place.
       * @returns {Promise<void>}
       */
      async function pollOpenPeeks() {
        // Replacing a pre's text destroys any in-progress text selection inside it
        // (e.g. the user copying a line from the terminal transcript) even though
        // the rest of the page's periodic re-render is already skipped for the same
        // reason — see userIsSelecting()/RAL-7. Skip this tick entirely and pick back
        // up once the selection is released.
        if (userIsSelecting()) return;
        const keys = Object.keys(peekOpen).filter((k) => peekOpen[k]);
        for (const key of keys) await fetchPeek(key, false);
      }
      /**
       * Closes the graph node context menu, if open.
       * @returns {void}
       */
      function closeGraphMenu() { const m = document.getElementById("graph-node-menu"); if (m) m.remove(); }
      document.addEventListener("click", closeGraphMenu);

      /**
       * Clears the graph-node multi-selection state.
       * @returns {void}
       */
      function clearNodeMultiSel() {
        nodeMultiSel.clear();
      }

      /**
       * Builds the stable selection key for one graph node.
       * @param {"task"|"cell"|"proof"} kind
       * @param {number} ti
       * @param {number} si
       * @param {number} vi
       * @returns {string}
       */
      function graphNodeKey(kind, ti, si, vi) { return `${kind}:${ti}:${si}:${vi}`; }

      /**
       * Converts a graph node location into the shared selection/status shape.
       * @param {string} squadId
       * @param {"task"|"cell"|"proof"} kind
       * @param {number} ti
       * @param {number} si
       * @param {number} vi
       * @returns {GraphNodeSelectionItem|null}
       */
      function graphNodeItem(squadId, kind, ti, si, vi) {
        const squad = findSquad(squadId); if (!squad) return null;
        if (kind === "task") {
          const t = squad.tasks[ti]; if (!t) return null;
          return { key: graphNodeKey(kind, ti, si, vi), squadId, kind, taskIdx: ti, cellIdx: -1, proofIdx: -1, proofScope: "", label: t.name };
        }
        if (kind === "cell") {
          const s = squad.tasks[ti]?.cells?.[si]; if (!s) return null;
          return { key: graphNodeKey(kind, ti, si, vi), squadId, kind, taskIdx: ti, cellIdx: si, proofIdx: -1, proofScope: "", label: s.name ?? s.id };
        }
        const isTaskProof = si < 0;
        const v = isTaskProof ? squad.tasks[ti]?.proof?.[vi] : squad.tasks[ti]?.cells?.[si]?.proof?.[vi];
        if (!v) return null;
        return {
          key: graphNodeKey(kind, ti, si, vi), squadId, kind, taskIdx: ti, cellIdx: si, proofIdx: vi,
          proofScope: isTaskProof ? "task" : "cell", label: v.id || v.kind
        };
      }

      /**
       * Parses a serialized graph-node key back into a selection item.
       * @param {string} squadId
       * @param {string} key
       * @returns {GraphNodeSelectionItem|null}
       */
      function graphNodeItemFromKey(squadId, key) {
        const [kind, ti, si, vi] = key.split(":");
        if ((kind !== "task" && kind !== "cell" && kind !== "proof") || ti === undefined || si === undefined || vi === undefined) return null;
        return graphNodeItem(squadId, kind, +ti, +si, +vi);
      }

      /**
       * Returns the graph nodes currently selected for batch actions.
       * @param {string} squadId
       * @param {GraphNodeSelectionItem} fallback
       * @returns {GraphNodeSelectionItem[]}
       */
      function graphMenuSelection(squadId, fallback) {
        if (nodeMultiSel.has(fallback.key) && nodeMultiSel.size > 1) {
          return /** @type {GraphNodeSelectionItem[]} */ ([...nodeMultiSel].map((key) => graphNodeItemFromKey(squadId, key)).filter(Boolean));
        }
        return [fallback];
      }

      /**
       * Replaces the graph-node selection with exactly one node.
       * @param {string} squadId
       * @param {"task"|"cell"|"proof"} kind
       * @param {number} ti
       * @param {number} si
       * @param {number} vi
       * @returns {void}
       */
      function selectSingleGraphNode(squadId, kind, ti, si, vi) {
        const item = graphNodeItem(squadId, kind, ti, si, vi); if (!item) return;
        nodeMultiSel = new Set([item.key]);
        sel = { kind, taskIdx: ti, cellIdx: si, proofIdx: vi };
        editing = false;
      }

      /**
       * Toggles or replaces the graph-node selection based on modifier keys.
       * @param {MouseEvent} e
       * @param {string} squadId
       * @param {"task"|"cell"|"proof"} kind
       * @param {number} ti
       * @param {number} si
       * @param {number} vi
       * @returns {void}
       */
      function onGraphNodeClick(e, squadId, kind, ti, si, vi) {
        e.stopPropagation();
        const item = graphNodeItem(squadId, kind, ti, si, vi); if (!item) return;
        if (e.shiftKey || e.ctrlKey || e.metaKey) {
          if (nodeMultiSel.has(item.key)) {
            if (nodeMultiSel.size > 1) nodeMultiSel.delete(item.key);
          } else {
            nodeMultiSel.add(item.key);
          }
          sel = { kind, taskIdx: ti, cellIdx: si, proofIdx: vi };
          editing = false;
          renderGraph(); renderDetails(); syncHash(true);
          return;
        }
        pick(kind, ti, si, vi);
      }

      /**
       * Returns whether one graph node is currently part of the multi-selection.
       * @param {"task"|"cell"|"proof"} kind
       * @param {number} ti
       * @param {number} si
       * @param {number} vi
       * @returns {boolean}
       */
      function graphNodeSelected(kind, ti, si, vi) {
        return nodeMultiSel.has(graphNodeKey(kind, ti, si, vi));
      }

      /**
       * Renders one graph-node label for confirmations and failure summaries.
       * @param {GraphNodeSelectionItem} item
       * @returns {string}
       */
      function graphNodeActionLabel(item) {
        return item.kind === "task"
          ? `task "${item.label}"`
          : item.kind === "cell"
            ? `cell "${item.label}"`
            : `proof "${item.label}"`;
      }

      /**
       * Extracts an error message from a failed API response.
       * @param {Response} resp
       * @param {string} fallback
       * @returns {Promise<string>}
       */
      async function responseError(resp, fallback) {
        const body = await resp.json().catch(() => ({}));
        return (body.error && body.error.message) || `${fallback} (HTTP ${resp.status})`;
      }

      /**
       * Shows a batch-action summary, especially when some items failed.
       * @param {string} verbPast
       * @param {GraphNodeSelectionItem[]} items
       * @param {string[]} succeeded
       * @param {string[]} failed
       * @returns {void}
       */
      function reportGraphActionOutcome(verbPast, items, succeeded, failed) {
        if (!failed.length) {
          if (items.length > 1) alert(`${verbPast} ${succeeded.length} selected node(s).`);
          return;
        }
        const head = succeeded.length
          ? `${verbPast} ${succeeded.length} of ${items.length} selected node(s).`
          : `Failed to ${verbPast.toLowerCase()} any selected nodes.`;
        const detail = failed.map((msg) => `- ${msg}`).join("\n");
        alert(`${head}\n\nFailures:\n${detail}`);
      }

      /**
       * Returns whether a graph-node action should be shown for the selection.
       * @param {"restart"|"stop"|"status"|"solo"|"unsolo"} action
       * @param {GraphNodeSelectionItem[]} items
       * @returns {boolean}
       */
      function graphActionShown(action, items) {
        const compat = GRAPH_NODE_ACTION_COMPAT[action];
        if (!items.length || items.some((item) => !compat.includes(/** @type {"task"|"cell"|"proof"} */ (item.kind)))) return false;
        if (action === "solo") return items.some((item) => !!findSquad(item.squadId)?.tasks[item.taskIdx] && !findSquad(item.squadId)?.tasks[item.taskIdx]?.soloed);
        if (action === "unsolo") return items.some((item) => !!findSquad(item.squadId)?.tasks[item.taskIdx]?.soloed);
        return true;
      }

      /**
       * Builds the context-menu HTML rows for the current graph-node selection.
       * @param {GraphNodeSelectionItem[]} items
       * @returns {string[]}
       */
      function graphMenuRows(items) {
        const sameKind = items.every((item) => item.kind === items[0].kind);
        const kind = sameKind ? items[0].kind : "mixed";
        /** @type {string[]} */
        const rows = [];
        if (graphActionShown("restart", items)) {
          const tip = kind === "task"
            ? `Restart ${items.length > 1 ? "every selected task" : "this task"} from its first cell, keeping unrelated tasks alone.\nEach task still dirties downstream squads using its normal restart semantics.\nThis cannot be undone.`
            : kind === "cell"
              ? `Restart ${items.length > 1 ? "every selected cell" : "this cell"} and each cell's own downstream branch.\nUpstream cells stay done and are skipped on re-run.\nThis cannot be undone.`
              : kind === "proof"
                ? `Restart ${items.length > 1 ? "every selected proof step" : "this proof step"} using each proof step's normal task/cell-specific restart path.\nThis cannot be undone.`
                : "Restart every selected node.\nEach task, cell, or proof node keeps its own normal downstream restart semantics.\nThis cannot be undone.";
          const label = kind === "task" ? "⟳ Restart task"
            : kind === "cell" ? "⟳ Restart cell"
              : kind === "proof" ? "⟳ Restart proof"
                : "⟳ Restart selected";
          rows.push(`<div onclick="squadGraphMenuAction(event,'restart')" data-tip="${tip}">${label}</div>`);
        }
        if (graphActionShown("stop", items)) {
          rows.push(`<div class="danger" onclick="squadGraphMenuAction(event,'stop')" data-tip="Stop ${items.length > 1 ? "all selected nodes" : "this node"} and everything downstream of each selected branch in this squad.\nEach node still uses the same downstream-only cascade semantics as its normal Stop action.\nThis cannot be undone.">■ Stop</div>`);
        }
        if (graphActionShown("status", items)) {
          rows.push(`<div onclick="squadGraphMenuAction(event,'status')" data-tip="Manually override ${items.length > 1 ? "all selected nodes" : "this node"} to any valid state.\nA confirmation dialog will list every affected item before applying.">⚙ Set Status</div>`);
        }
        if (graphActionShown("solo", items)) {
          rows.push(`<div onclick="squadGraphMenuAction(event,'solo')" data-tip="Solo every selected task that is not already soloed.\nOther tasks in the same squad pause until each task is un-soloed again.">★ Solo task</div>`);
        }
        if (graphActionShown("unsolo", items)) {
          rows.push(`<div onclick="squadGraphMenuAction(event,'unsolo')" data-tip="Un-solo every selected task that is currently soloed.\nOther paused tasks in the same squad can dispatch again afterward.">☆ Un-solo task</div>`);
        }
        return rows;
      }

      /**
       * Opens the Set Status picker for the current graph-node selection.
       * @param {MouseEvent} e
       * @param {GraphNodeSelectionItem[]} items
       * @returns {void}
       */
      function openGraphStatusPicker(e, items) {
        closeGraphMenu();
        openStatusPicker(e, items);
      }

      /**
       * Confirms a batch graph action before it runs.
       * @param {"restart"|"stop"} action
       * @param {GraphNodeSelectionItem[]} items
       * @returns {boolean}
       */
      function confirmGraphBatchAction(action, items) {
        const msg = action === "restart"
          ? `Restart ${items.length} selected node(s)?\n\n${items.map((item) => `- ${graphNodeActionLabel(item)}`).join("\n")}\n\nEach node keeps its own normal restart/downstream semantics.\nThis cannot be undone.`
          : `Stop ${items.length} selected node(s)?\n\n${items.map((item) => `- ${graphNodeActionLabel(item)}`).join("\n")}\n\nEach node keeps its own downstream-only Stop cascade semantics.\nThis cannot be undone.`;
        return confirm(msg);
      }

      /**
       * Executes one graph-node action for one selected node.
       * @param {"restart"|"stop"|"solo"|"unsolo"} action
       * @param {GraphNodeSelectionItem} item
       * @returns {Promise<Response>}
       */
      function squadGraphItemRequest(action, item) {
        if (action === "restart") {
          if (item.kind === "task") return post(`/api/squads/${item.squadId}/tasks/${item.taskIdx}/restart`);
          if (item.kind === "cell") return post(`/api/squads/${item.squadId}/cells/${item.taskIdx}/${item.cellIdx}/restart`);
          return item.proofScope === "task"
            ? post(`/api/squads/${item.squadId}/tasks/${item.taskIdx}/proof/${item.proofIdx}/restart`)
            : post(`/api/squads/${item.squadId}/cells/${item.taskIdx}/${item.cellIdx}/proof/${item.proofIdx}/restart`);
        }
        if (action === "stop") {
          return post(`/api/squads/${item.squadId}/set-status`, {
            kind: item.kind, task_idx: item.taskIdx, cell_idx: item.cellIdx,
            proof_idx: item.proofIdx, proof_scope: item.proofScope, state: "cancelled"
          });
        }
        return post(`/api/squads/${item.squadId}/tasks/${item.taskIdx}/${action}`);
      }

      /**
       * Executes a batch graph-node action and reports partial failures.
       * @param {"restart"|"stop"|"solo"|"unsolo"} action
       * @param {GraphNodeSelectionItem[]} items
       * @returns {Promise<void>}
       */
      async function squadGraphBatchAction(action, items) {
        if (!items.length) return;
        if ((action === "restart" || action === "stop") && !confirmGraphBatchAction(action, items)) return;
        const succeeded = [];
        const failed = [];
        const past = action === "restart" ? "Restarted"
          : action === "stop" ? "Stopped"
            : action === "solo" ? "Soloed"
              : "Un-soloed";
        for (const item of items) {
          try {
            const resp = await squadGraphItemRequest(action, item);
            if (!resp.ok) failed.push(`${graphNodeActionLabel(item)}: ${await responseError(resp, `${action} failed`)}`);
            else succeeded.push(graphNodeActionLabel(item));
          } catch (_) {
            failed.push(`${graphNodeActionLabel(item)}: network error`);
          }
        }
        reportGraphActionOutcome(past, items, succeeded, failed);
        tick();
        if (tab === "queue") pollQueue();
      }

      /**
       * Runs the selected graph-node context-menu action.
       * @param {MouseEvent} e
       * @param {"restart"|"stop"|"status"|"solo"|"unsolo"} action
       * @returns {Promise<void>}
       */
      async function squadGraphMenuAction(e, action) {
        e.stopPropagation();
        const items = _graphMenuItems; if (!items || !items.length) return;
        if (action === "status") {
          openGraphStatusPicker(e, items);
          return;
        }
        if (items.length === 1) {
          const item = items[0];
          closeGraphMenu();
          if (action === "restart") {
            if (item.kind === "task") await restartTask(item.squadId, item.taskIdx);
            else if (item.kind === "cell") await restartCell(item.squadId, item.taskIdx, item.cellIdx);
            else await restartProof(item.squadId, item.taskIdx, item.cellIdx, item.proofIdx);
            return;
          }
          if (action === "stop") {
            await stopNode(item.squadId, item.kind, item.taskIdx, item.cellIdx, item.proofIdx, item.proofScope || "", item.label);
            return;
          }
          await soloTaskAct(item.squadId, item.taskIdx, action === "solo");
          return;
        }
        closeGraphMenu();
        await squadGraphBatchAction(action, items);
      }

      /**
       * Opens the graph-node context menu for one node or the current multi-selection.
       * @param {MouseEvent} e
       * @param {GraphNodeSelectionItem[]} items
       * @returns {void}
       */
      function openGraphNodeMenu(e, items) {
        e.preventDefault(); e.stopPropagation(); closeGraphMenu();
        _graphMenuItems = items;
        const rows = graphMenuRows(items);
        if (!rows.length) return;
        const menu = document.createElement("div");
        menu.className = "ctx-menu"; menu.id = "graph-node-menu"; menu.innerHTML = rows.join("");
        document.body.appendChild(menu);
        menu.style.left = Math.min(e.clientX, window.innerWidth - 240) + "px";
        menu.style.top = Math.min(e.clientY, window.innerHeight - 120) + "px";
      }

