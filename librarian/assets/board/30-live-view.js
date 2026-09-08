      // ---- Remote Open Agent terminal relay (RAL-355 Phase 10) ----
      // The browser counterpart of `ralphus cell remote-terminal`
      // (cli/src/terminal_relay.rs): a cell whose `machine` is set has no
      // window to open on the daemon's own desktop the way `openAgentTerminal`
      // does, so this mints a one-shot ticket (POST .../terminal-ticket),
      // connects a raw WebSocket directly to the daemon's terminal-relay
      // listener (one port above the daemon API -- see
      // docs/machine-providers.md's "The `terminal` verb" section), and
      // renders it into an xterm.js instance in a modal. Direct
      // browser-to-daemon, NOT proxied through this librarian process the way
      // `/api/events` SSE is: unlike SSE (a very long plain HTTP response,
      // which `tiny_http` handles fine), a WebSocket needs a protocol
      // *upgrade* `tiny_http` cannot do -- the same limitation that made the
      // daemon bind its own dedicated port in the first place (see
      // `Cargo.toml`'s `tungstenite` dependency comment). This assumes the
      // daemon is reachable at the same hostname the browser used to reach
      // this board, just on a different port -- true whenever daemon and
      // librarian are co-located (every build script in this repo runs them
      // that way); a deployment that puts them on genuinely different hosts
      // needs its own reachability plan for the relay port, same as it would
      // for `ralphus cell remote-terminal` run from a third machine.

      /** @typedef {{term: any, ws: WebSocket, squadId: string, ti: number, si: number}} RemoteTerminalSession */
      /** @type {RemoteTerminalSession|null} the one remote-terminal modal open at a time -- only one can be, since it's a modal */
      let remoteTerminalSession = null;

      /**
       * Renders the "Remote Terminal" terminal-menu item -- only offered for
       * a cell whose `machine` is set (a local cell keeps using "Open
       * Agent"/`openAgentMenuItem` instead, which the call site is
       * responsible for choosing between). Mirrors `openAgentMenuItem`'s
       * disabled-reason style, plus the daemon's own eligibility checks
       * (`server::mint_terminal_ticket_route`) so a doomed request never
       * reaches the network: still-running (no remote detach mechanism yet),
       * no session to resume, or a non-Claude-Code agent (this release only
       * supports Claude Code remotely, mirroring
       * `ralphus_core::agent_resume::is_claude_agent`).
       * @param {string} key
       * @param {string} clickAction
       * @param {{[k: string]: string|number}} clickData
       * @param {boolean} isRunning
       * @param {string} [agentSessionId]
       * @param {string} [agent]
       * @returns {string}
       */
      function remoteTerminalMenuItem(key, clickAction, clickData, isRunning, agentSessionId, agent) {
        if (isRunning) {
          return terminalMenuItem(key, "Remote Terminal", "", {},
            "Not available — this cell is still running remotely, and the remote terminal relay has no detach mechanism yet (unlike local Open Agent). Wait for it to finish, then try again.",
            true);
        }
        if (!agentSessionId) {
          return terminalMenuItem(key, "Remote Terminal", "", {},
            "Not available yet — no resumable agent session has been recorded for this cell.",
            true);
        }
        if (!["claude", "anthropic", "claude-code", "claude-cli"].includes(agent || "")) {
          return terminalMenuItem(key, "Remote Terminal", "", {},
            `Not available — the remote terminal relay only supports Claude Code cells this release; this cell's agent is ${JSON.stringify(agent)}.`,
            true);
        }
        return terminalMenuItem(key, "Remote Terminal", clickAction, clickData,
          "Attach an interactive terminal to this cell's resumed Claude Code session, running on its remote machine, over the daemon's WebSocket relay.\nWho/when: you want to keep working with the agent interactively on a remote cell — the same thing \"Open Agent\" gives a local cell.\nOnly one terminal can be attached to this cell at a time; closing this window ends the session (no reattach in this release).");
      }

      /**
       * Mints a terminal-relay ticket and opens the remote-terminal modal,
       * connecting an xterm.js instance to it over WebSocket. See the
       * "Remote Open Agent terminal relay" block comment above for the full
       * design.
       * @param {string} squadId
       * @param {number} ti
       * @param {number} si
       * @returns {Promise<void>}
       */
      async function openRemoteTerminal(squadId, ti, si) {
        let resp;
        try {
          resp = await post(`/api/squads/${squadId}/cells/${ti}/${si}/terminal-ticket`);
        } catch (_) {
          alert("Failed to open remote terminal: network error");
          return;
        }
        if (!resp.ok) {
          const e = await resp.json().catch(() => ({}));
          alert(`Failed to open remote terminal: ${((e.error || {}).message) || "unknown error"}`);
          return;
        }
        const data = await resp.json();
        byId("modal-root").innerHTML = `<div class="modal-bg" onclick="if(event.target===this)closeRemoteTerminal()"><div class="modal" style="width:900px;max-width:96vw">
          <h2>Remote Terminal <button class="btn" style="float:right" onclick="closeRemoteTerminal()" data-tip="Close this terminal — ends the remote session immediately.\nThis cannot be undone; reopen from the cell to start a fresh session.">✕</button></h2>
          <div id="remote-terminal-container" style="height:520px;background:var(--terminal-bg)" data-tip="Live remote terminal — bytes typed here go straight to the resumed Claude Code process on the remote machine."></div>
        </div></div>`;
        const container = /** @type {HTMLElement} */ (byId("remote-terminal-container"));
        const XTerm = /** @type {any} */ (window).Terminal;
        const XTermFitAddon = /** @type {any} */ (window).FitAddon;
        // xterm.js's own theme option takes a literal color string, not CSS --
        // read `--terminal-bg`'s resolved value so this still has one source
        // of truth (docs/colors.md) instead of a second hardcoded hex.
        const terminalBg = getComputedStyle(document.documentElement).getPropertyValue("--terminal-bg").trim();
        const term = new XTerm({ convertEol: true, cursorBlink: true, theme: { background: terminalBg } });
        const fit = new XTermFitAddon.FitAddon();
        term.loadAddon(fit);
        term.open(container);
        fit.fit();
        const proto = location.protocol === "https:" ? "wss:" : "ws:";
        const params = new URLSearchParams({
          ticket: data.ticket, squad_id: squadId, task_idx: String(ti), cell_idx: String(si),
          cols: String(term.cols), lines: String(term.rows),
        });
        const ws = new WebSocket(`${proto}//${location.hostname}:${data.port}${data.path}?${params}`);
        ws.binaryType = "arraybuffer";
        remoteTerminalSession = { term, ws, squadId, ti, si };
        ws.onmessage = (ev) => {
          if (typeof ev.data === "string") {
            term.write(ev.data);
          } else {
            term.write(new Uint8Array(/** @type {ArrayBuffer} */ (ev.data)));
          }
        };
        ws.onclose = () => term.write("\r\n\x1b[33m[ralphus] terminal session ended.\x1b[0m\r\n");
        ws.onerror = () => term.write("\r\n\x1b[31m[ralphus] connection error.\x1b[0m\r\n");
        term.onData((/** @type {string} */ chunk) => {
          if (ws.readyState === WebSocket.OPEN) ws.send(chunk);
        });
      }

      /**
       * Tears down the active remote-terminal session (if any) and closes
       * its modal -- called from the modal's own close button/backdrop
       * click, in place of the generic `closeModal()`, since this modal
       * alone needs to end a live WebSocket session first.
       * @returns {void}
       */
      function closeRemoteTerminal() {
        if (remoteTerminalSession) {
          try { remoteTerminalSession.ws.close(); } catch (_) { /* already closed */ }
          try { remoteTerminalSession.term.dispose(); } catch (_) { /* already disposed */ }
          remoteTerminalSession = null;
        }
        closeModal();
      }

      // ---- Live tmux pane peek (RAL-102) ----
      // The "Read-only terminal" button toggles an inline, auto-refreshing box
      // showing a cell/proof/resolver's tmux pane content — never sends
      // input. Keyed by a stable string so `pollOpenPeeks` (on its own 2s
      // interval — see the bottom of this script) can refresh every
      // currently-expanded box without disturbing anything else on the page
      // (same spirit as preserveUserState).
      //
      // ---- RAL-186: pure peek state machine ----
      // Everything between the BEGIN/END markers below is deliberately free of
      // DOM access, `fetch`, and module-level state, so test/peek-state.test.mjs
      // can slice it straight out of this file and drive it under `node --test`.
      // board.html has no build step and no test harness of its own; this
      // marker-delimited region is how the live view's trickiest logic — the
      // ended/live transitions that RAL-186 was filed against — gets real
      // automated coverage instead of manual eyeballing. Keep it pure: anything
      // added here must stay callable with no browser present.
      // RALPHUS-PEEK-STATE-MACHINE:BEGIN
      /**
       * @typedef {object} PeekPaneResponse
       * @property {boolean} [active] - Whether the daemon found a live tmux cell for this key.
       * @property {string} [content] - Live pane content, or the persisted historical snapshot when inactive.
       * @property {number|null} [last_activity_ms] - Unix-epoch-ms of the last observed fresh output (RAL-170).
       */
      /**
       * @typedef {object} PeekPaneState
       * @property {boolean} ended - Confirmed-ended (past PEEK_MISSING_STRIKE_LIMIT), not just one transient miss.
       * @property {number} missingStrikes - Consecutive inactive polls; reset to 0 by any active response.
       * @property {number|null} lastActivityMs - Last-fetched `last_activity_ms`, or null when there is none.
       * @property {string} text - Text the box should display.
       */
      /**
       * @typedef {object} PeekPaneTransition
       * @property {PeekPaneState} state - The state to store for this key.
       * @property {boolean} headerChanged - Whether `ended` flipped, in either direction.
       */
      // A lone `active:false` poll is often just a transient psmux hiccup
      // (see PSMUX_CRASH_NOTES.local.md) rather than a real session death —
      // confirmed live: a session flashed "terminal session has ended" in
      // this exact peek box while the daemon's own Cartographer trail showed
      // a completely clean run with no missed-cell detection at all.
      // Mirrors the daemon's own tolerance (`MISSING_SESSION_STRIKE_LIMIT`,
      // 3 misses at its 500ms poll interval — daemon/src/runner.rs) scaled to
      // this box's slower 2s poll cadence.
      const PEEK_MISSING_STRIKE_LIMIT = 2;
      /**
       * Sanitizes a peek key into a value safe to use inside a DOM element id.
       * @param {string} key
       * @returns {string}
       */
      function peekCssKey(key) { return key.replace(/[^a-zA-Z0-9_-]/g, "_"); }
      /**
       * Resolves a peek key to the daemon API URL for its live tmux pane content.
       * @param {string} key
       * @returns {string|null}
       */
      function peekUrlFor(key) {
        const [kind, ...rest] = key.split("|");
        if (kind === "cell") { const [squadId, ti, si] = rest; return `/api/squads/${squadId}/cells/${ti}/${si}/pane?lines=500`; }
        if (kind === "proof") { const [squadId, ti, scope, si, vi] = rest; return `/api/squads/${squadId}/proofs/${ti}/${scope}/${si}/${vi}/pane?lines=500`; }
        if (kind === "guardian") { const [gid, branchId] = rest; return `/api/guardians/${gid}/branches/${branchId}/pane?lines=500`; }
        if (kind === "guardian-manual") { const [gid] = rest; return `/api/guardians/${gid}/manual-checks/pane?lines=500`; }
        return null;
      }
      /**
       * Folds one `/pane` response into a peek box's next state.
       *
       * A single inactive poll is not immediately treated as "ended" —
       * `missingStrikes` must reach `missingLimit` first (see
       * PEEK_MISSING_STRIKE_LIMIT). Below the limit the box keeps showing
       * whatever it last displayed rather than flashing to "ended" and back.
       * Once confirmed ended, `content` (if any) is the daemon's persisted
       * last-pane-content snapshot (RAL-102 follow-up) — a read-only historical
       * record, not fresh output.
       *
       * RAL-186: the reverse transition matters just as much. When a restarted
       * cell or proof step brings a new tmux pane up under the same
       * (deterministic, index-derived) name, the very next active response must
       * clear `ended` *and* report `headerChanged`, because the "Historical
       * record (read-only)" banner, its grey dot and its tooltip are only
       * produced by a full `peekBox()` render. Reporting the flip in one
       * direction but not the other is exactly what left a restarted step's
       * Live View sitting on its stale historical log until the user navigated
       * away and back.
       * @param {PeekPaneState} prev
       * @param {PeekPaneResponse} data
       * @param {number} missingLimit
       * @returns {PeekPaneTransition}
       */
      function nextPeekPaneState(prev, data, missingLimit) {
        if (data.active) {
          return {
            state: {
              ended: false,
              missingStrikes: 0,
              // RAL-170: liveness signal, always fresh since it's re-fetched
              // alongside the pane content itself -- never backfilled/stale.
              lastActivityMs: data.last_activity_ms ?? null,
              text: data.content || "(no output yet)",
            },
            headerChanged: prev.ended,
          };
        }
        const missingStrikes = prev.missingStrikes + 1;
        if (missingStrikes < missingLimit) {
          // Not confirmed yet -- keep showing the last known content instead
          // of flashing "ended" for what may be a transient miss.
          return {
            state: { ended: prev.ended, missingStrikes, lastActivityMs: prev.lastActivityMs, text: prev.text || "Loading…" },
            headerChanged: false,
          };
        }
        return {
          state: {
            ended: true,
            missingStrikes,
            lastActivityMs: null, // a finished cell's last output is history, not a liveness signal
            text: (data.content && data.content.trim())
              ? data.content + "\n\n[Read-only historical record — this terminal session has ended.]"
              : "Terminal cell has ended. No output was recorded before it ended.",
          },
          headerChanged: !prev.ended,
        };
      }
      // RALPHUS-PEEK-STATE-MACHINE:END

      // ---- RAL-288 Stage 5: Live View debug/agent split ----
      // The daemon now always strips ralphus's own `RALPHUS_EVENT:`/
      // `RALPHUS_TMUX_DONE` marker lines before ever serving Live View pane
      // text (`daemon/src/server.rs::strip_ralphus_pane_markers`), and the
      // runner's own `ralphus [...]`-prefixed diagnostic prints were
      // relocated to Cartographer-only (`runner/src/{execute,agent_backend,
      // main}.rs`) rather than also landing in the pane. So `peekContent`
      // is pure agent output unconditionally now -- there is nothing left to
      // strip client-side, replacing the old subtractive design (this
      // region used to hold `isRalphusDebugLine`/`stripDebugLines`, deleted
      // along with the `test/debug-strip.test.mjs` coverage that existed only
      // for them).
      //
      // "Show Debug Messages" is additive instead: when checked, it fetches
      // that entity's own merged, current-attempt-only debug stream
      // (`.../debug-events`, RAL-296) -- lifecycle events with any
      // terminal-log excerpt already inlined at its own event -- and prepends
      // it, in chronological order, ahead of the (unmodified) live pane text.
      // No separate "---- footer ----" block: the two used to be two
      // differently-sourced views (a disconnected Cartographer-only footer
      // here, vs. the durable-attempt viewer below reading raw, un-merged
      // terminal-log content) that could disagree with each other; RAL-296
      // unified them onto this one endpoint, which the attempt-history box's
      // "view latest attempt" path (see `viewHistoryAttempt`) also now reads
      // from for its "popup/expanded" presentation of the same stream. This
      // is still a deliberate approximation, not true line-by-line
      // interleaving -- tmux pane captures carry no reliable per-line
      // timestamps to splice against (`daemon/src/timeline.rs`'s module doc
      // comment documents the same limitation for the whole-squad timeline
      // this reuses).
      /** @type {{[key: string]: DebugEventEntry[]}} cached debug-event rows per peek key, fetched only while that pane's checkbox is checked. */
      let peekDebugEvents = {};
      // RALPHUS-DEBUG-STREAM:BEGIN
      // Pure, DOM/fetch/module-state-free logic for the unified debug/
      // terminal-log stream (RAL-296) -- kept free of `peekContent`/
      // `peekDebugEvents`/other module-level state so it can be sliced out
      // and evaluated standalone by test/board-debug-stream.mjs, the same
      // pattern test/board-peek-state.mjs and test/board-merge-button.mjs use
      // for their own regions. See librarian/AGENTS.md's "Testing --
      // Frontend" section before moving these markers.
      /**
       * The `/debug-events` endpoint for peek key `key` (RAL-296: all four
       * kinds are wired -- cell, proof step, guardian branch resolver,
       * guardian manual-checks generation).
       * @param {string} key
       * @returns {string|null}
       */
      function debugEventsUrlFor(key) {
        const [kind, ...rest] = key.split("|");
        if (kind === "cell") { const [squadId, ti, si] = rest; return `/api/squads/${squadId}/cells/${ti}/${si}/debug-events`; }
        if (kind === "proof") { const [squadId, ti, scope, si, vi] = rest; return `/api/squads/${squadId}/proofs/${ti}/${scope}/${si}/${vi}/debug-events`; }
        if (kind === "guardian") { const [gid, branchId] = rest; return `/api/guardians/${gid}/branches/${branchId}/debug-events`; }
        if (kind === "guardian-manual") { const [gid] = rest; return `/api/guardians/${gid}/manual-checks/debug-events`; }
        return null;
      }
      /**
       * One cached debug-event row rendered as a human-readable line, with
       * its inlined terminal-log excerpt (if any) indented beneath it --
       * mirrors `daemon/src/timeline.rs::render_entry`'s plain-text rendering
       * of the same `SquadTimelineEntry` shape.
       * @param {DebugEventEntry} e
       * @returns {string}
       */
      function formatDebugEvent(e) {
        const head = `[${new Date(e.at_ms).toLocaleTimeString()}] ${e.source}: ${e.message}`;
        if (!e.log_excerpt) return head;
        const excerpt = e.log_excerpt.split("\n").map((l) => `    | ${l}`).join("\n");
        return `${head}\n${excerpt}`;
      }
      /**
       * Whether `attempt` is the most recent of `attempts` (RAL-296) -- the
       * one `viewHistoryAttempt` routes through the merged `.../debug-events`
       * stream instead of that attempt's raw, un-merged terminal-log content.
       * An empty `attempts` list (nothing fetched yet) is never "most recent".
       * @param {AttemptMeta[]} attempts
       * @param {number} attempt
       * @returns {boolean}
       */
      function isMostRecentAttempt(attempts, attempt) {
        return attempts.length > 0 && attempt === Math.max(...attempts.map((a) => a.attempt));
      }
      // RALPHUS-DEBUG-STREAM:END
      /**
       * Peek key `key`'s current display text: cached debug events (RAL-296:
       * lifecycle events with their terminal-log excerpts already inlined),
       * in chronological order, ahead of the live pane's own tail -- when
       * "Show Debug Messages" is checked and any are cached for this key.
       * Otherwise just the clean base pane content.
       * @param {string} key
       * @returns {string}
       */
      function currentPeekDisplayText(key) {
        const base = peekContent[key] || "";
        if (!peekShowsDebug(key)) return base;
        const events = peekDebugEvents[key];
        if (!events || !events.length) return base;
        return `${events.map(formatDebugEvent).join("\n")}\n\n${base}`;
      }
      /**
       * Fetches and caches peek key `key`'s debug events, if its checkbox is
       * currently checked and this kind has an endpoint for it. Best-effort:
       * a failed fetch just leaves whatever was cached before untouched.
       * @param {string} key
       * @returns {Promise<void>}
       */
      async function maybeRefreshDebugEvents(key) {
        if (!peekShowsDebug(key)) return;
        const url = debugEventsUrlFor(key);
        if (!url) return;
        try {
          const resp = await fetch(url);
          if (resp.ok) peekDebugEvents[key] = await resp.json();
        } catch (_) { /* best-effort; keep the prior cache */ }
      }

      // ---- RAL-247: credential env-var redaction (defense-in-depth) ----
      /**
       * Whether an env-var name is a credential whose value must never be
       * displayed. Mirrors `ralphus_core::redact::is_secret_env_key`: explicit
       * known names plus a generic name heuristic, so a new provider's key is
       * caught without editing this list. Over-matching a non-secret name is
       * safe (it just over-redacts); under-matching is the bug.
       * @param {string} key
       * @returns {boolean}
       */
      function isSecretEnvKey(key) {
        const u = key.toUpperCase();
        if (u === "ANTHROPIC_BASE_URL") return true;
        return ["TOKEN", "_KEY", "SECRET", "PASSWORD", "CREDENTIAL", "AUTH"]
          .some((s) => u.indexOf(s) !== -1);
      }
      /**
       * Defense-in-depth (RAL-247): mask the *values* of credential env-var
       * assignments — PowerShell `$env:KEY = '<value>'` (the concrete leak
       * path) and POSIX `KEY='<value>'` — that could appear in pane or
       * transcript text with `[REDACTED]`. The daemon already redacts at every
       * API boundary the board reads, so this is a second line that also
       * protects against an older or otherwise-unpatched daemon. Plain text
       * carrying no assignment marker is returned unchanged. Display-only: it
       * never rewrites what the daemon persists.
       * @param {string} text
       * @returns {string}
       */
      function scrubSecrets(text) {
        const ASSIGN = /\$env:([A-Za-z_][A-Za-z0-9_]*) = '([^'\n]*)'|([A-Za-z_][A-Za-z0-9_]*)='([^'\n]*)'/g;
        if (ASSIGN.test(text) === false) return text;
        ASSIGN.lastIndex = 0;
        return text.replace(ASSIGN, (m, envKey, envVal, plainKey, plainVal) => {
          const key = envKey || plainKey;
          const val = envVal !== undefined ? envVal : plainVal;
          if (!isSecretEnvKey(key)) return m;
          return m.slice(0, m.length - val.length - 1) + "[REDACTED]'";
        });
      }
      // ---- /RAL-247 ----

      /**
       * Toggles a peek box's expanded/collapsed state and re-renders whichever
       * pane (details or review) currently owns it.
       * @param {string} key
       * @returns {void}
       */
      function togglePeek(key) {
        peekOpen[key] = !peekOpen[key];
        if (!peekOpen[key]) {
          delete peekContent[key]; // stale content shouldn't reappear next time this box is opened
          delete peekEnded[key];
          delete peekMissingStrikes[key];
          delete peekLastActivity[key];
        }
        terminalMenuOpen[key] = false; // pressing the primary button should collapse the actions dropdown too
        if (sel.kind) renderDetails();
        if (selectedGuardian) renderReviewDetail();
        // Fetch immediately on open (rather than waiting up to 2s for the next
        // poll tick) and force it to the bottom — a freshly opened box has no
        // prior scroll position to preserve, and the most recent output is what
        // the user opened it to see.
        if (peekOpen[key]) fetchPeek(key, true);
      }
      /**
       * Whether Live View pane `key` currently shows ralphus's own
       * diagnostic/telemetry lines (RAL-232) rather than just agent output --
       * the per-pane override in `peekShowDebug` if this pane's checkbox has
       * been toggled this session, else the config-driven
       * `showDebugMessagesDefault`.
       * @param {string} key
       * @returns {boolean}
       */
      function peekShowsDebug(key) {
        return key in peekShowDebug ? peekShowDebug[key] : showDebugMessagesDefault;
      }
      /**
       * Toggles Live View pane `key`'s "Show Debug Messages" checkbox
       * (RAL-232). Repaints immediately with whatever's cached (instant, no
       * flash of "Loading…"), then — when just turned on — fetches that
       * cell's debug events (RAL-288 Stage 5) and repaints again once they
       * land, since the additive block can't be shown before it's fetched.
       * @param {string} key
       * @param {boolean} checked
       * @returns {void}
       */
      function toggleShowDebugMessages(key, checked) {
        peekShowDebug[key] = checked;
        const preId = `peek-pre-${peekCssKey(key)}`;
        if (peekContent[key] === undefined) return;
        setPeekPreText(preId, currentPeekDisplayText(key));
        if (checked) {
          maybeRefreshDebugEvents(key).then(() => setPeekPreText(preId, currentPeekDisplayText(key)));
        }
      }
      /**
       * Fetches the operator-configured default for the "Show Debug
       * Messages" checkbox (RAL-232, `[live_view]` in `.ralphus.toml`) once
       * at page load. Best-effort: a failed/unreachable fetch just leaves
       * the built-in `false` (unchecked, agent-only) default in place.
       * @returns {Promise<void>}
       */
      async function fetchLiveViewConfigDefault() {
        try {
          const resp = await fetch("/api/config/live-view");
          if (!resp.ok) return;
          const data = await resp.json();
          showDebugMessagesDefault = !!data.show_debug_messages_default;
        } catch (_) {
          // keep the built-in false default
        }
      }
