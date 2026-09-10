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
      /**
       * @typedef {object} PeekPaneResponse - the `/pane` reply, now consulted
       *   only for its liveness `active` flag (RAL-397 Phase 2G-A moved Live
       *   View content onto the transcript tape); its `content` is no longer
       *   rendered.
       * @property {boolean} [active] - Whether the daemon found a live tmux cell for this key (`has-session`).
       * @property {string} [content] - Live pane snapshot (no longer rendered by the Live View).
       * @property {number|null} [last_activity_ms] - Unix-epoch-ms of the last observed fresh output (RAL-170); unused now that liveness ms comes from tape growth.
       */
      // RALPHUS-PEEK-STATE-MACHINE:BEGIN
      /**
       * @typedef {object} PeekLiveSignal - what one poll observed about a peek
       *   box's liveness (RAL-397 Phase 2G-A). The Live View's *content* now
       *   comes from the transcript tape, not `/pane`; `/pane` is still polled
       *   purely as an authoritative liveness probe (its `active` flag, based on
       *   the daemon's `has-session` check — its pane content is ignored).
       * @property {boolean|null} live - Liveness from the `/pane` probe's `active` flag, or null only when that probe itself failed/errored this poll — in which case the tape-growth fallback (`grew`) decides.
       * @property {boolean} grew - Whether the transcript's `total` grew since the previous poll — the tape-growth liveness fallback when `live` is null, and what feeds `lastActivityMs`.
       * @property {number} nowMs - Current wall-clock ms, passed in (never read here) so this stays pure and testable.
       */
      /**
       * @typedef {object} PeekPaneState
       * @property {boolean} ended - Confirmed-ended, not just one transient quiet poll.
       * @property {number} missingStrikes - Consecutive not-live polls under the tape-growth fallback; reset to 0 by any live signal.
       * @property {number|null} lastActivityMs - Wall-clock ms the transcript last grew, or null when there is none / the cell has ended.
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
       * Resolves a peek key to the daemon API URL for its `.raw` transcript
       * tape (RAL-397 Phase 2G-A) — the single content source the Live View
       * now pages through, `offset`/`limit`/`attempt` appended by the caller.
       * Sibling to {@link peekUrlFor} (still used by the linked-output reader),
       * one per peek kind, all four wired in 2F.
       * @param {string} key
       * @returns {string|null}
       */
      function peekTranscriptUrlFor(key) {
        const [kind, ...rest] = key.split("|");
        if (kind === "cell") { const [squadId, ti, si] = rest; return `/api/squads/${squadId}/cells/${ti}/${si}/pane-transcript`; }
        if (kind === "proof") { const [squadId, ti, scope, si, vi] = rest; return `/api/squads/${squadId}/proofs/${ti}/${scope}/${si}/${vi}/pane-transcript`; }
        if (kind === "guardian") { const [gid, branchId] = rest; return `/api/guardians/${gid}/branches/${branchId}/pane-transcript`; }
        if (kind === "guardian-manual") { const [gid] = rest; return `/api/guardians/${gid}/manual-checks/pane-transcript`; }
        return null;
      }
      /**
       * Folds one poll's liveness observation into a peek box's next state
       * (RAL-397 Phase 2G-A — the content itself comes from the transcript
       * tape, no longer from `/pane`, so this decides only ended/liveness).
       *
       * When the board's own node state answers authoritatively (`live` is a
       * boolean), it's trusted directly: `true` keeps the box live, `false`
       * ends it at once — board state has none of `/pane`'s transient hiccups.
       * When it can't (`live === null`, guardian boxes), the tape-growth
       * fallback applies: a growing tape is live, and a non-growing one is only
       * confirmed ended after `missingLimit` quiet polls, so a briefly-idle
       * live resolver doesn't flash "ended" (mirrors the old missing-strike
       * tolerance, PEEK_MISSING_STRIKE_LIMIT).
       *
       * RAL-186: the flip must be reported in *both* directions. A restarted
       * cell/proof brings a new tape up under the same (index-derived) key, and
       * the "Historical record (read-only)" banner, grey dot and tooltip are
       * only produced by a full `peekBox()` render — so reviving must report
       * `headerChanged` just as ending does, or the box stays visually stuck
       * on its stale historical banner until the user navigates away and back.
       * @param {PeekPaneState} prev
       * @param {PeekLiveSignal} signal
       * @param {number} missingLimit
       * @returns {PeekPaneTransition}
       */
      function nextPeekPaneState(prev, signal, missingLimit) {
        if (signal.live === true || (signal.live === null && signal.grew)) {
          return {
            state: {
              ended: false,
              missingStrikes: 0,
              // Fresh only when the tape actually grew this poll; a quiet-but-
              // live cell keeps its prior timestamp so the RAL-170 staleness
              // warning can still fire.
              lastActivityMs: signal.grew ? signal.nowMs : prev.lastActivityMs,
            },
            headerChanged: prev.ended,
          };
        }
        if (signal.live === false) {
          // Authoritatively ended by the board's node state — no strike wait.
          return {
            state: { ended: true, missingStrikes: prev.missingStrikes, lastActivityMs: null },
            headerChanged: !prev.ended,
          };
        }
        // Liveness unknown and the tape didn't grow: tolerate a few quiet polls
        // before declaring the box ended.
        const missingStrikes = prev.missingStrikes + 1;
        if (missingStrikes < missingLimit) {
          return {
            state: { ended: prev.ended, missingStrikes, lastActivityMs: prev.lastActivityMs },
            headerChanged: false,
          };
        }
        return {
          state: { ended: true, missingStrikes, lastActivityMs: null },
          headerChanged: !prev.ended,
        };
      }
      // RALPHUS-PEEK-STATE-MACHINE:END

      // ---- RAL-397 Phase 2G-A: single-tape seamless scroll ----
      // The Live View is driven entirely from the one `.raw` transcript tape
      // (`GET .../pane-transcript`, 2F): the bottom of the file IS the live
      // output (pipe-pane appends continuously, the sink flushes after every
      // read) and scrolling up just reads earlier bytes of the same file, so
      // there is no "live vs. saved" boundary to stitch. Everything between the
      // markers below is a pure, DOM/fetch/module-state-free reducer over a
      // loaded byte window, sliced out and driven standalone by
      // test/board-tape-scroll.mjs (the same pattern as the peek-state region).
      /** Byte window the Live View pages the tape in (matches the server's DEFAULT_TRANSCRIPT_RANGE_LIMIT). */
      const TAPE_CHUNK_BYTES = 64 * 1024;
      /** A deliberately-past-the-end offset for probing the tape's current size on open — the server clamps `start` to `total` and returns empty content. */
      const TAPE_PROBE_OFFSET = Number.MAX_SAFE_INTEGER;
      /** Soft cap (chars) on the retained follow-tail window; older text is dropped off the top while following the tail, still re-loadable by scrolling up. */
      const TAPE_MAX_WINDOW_CHARS = 1024 * 1024;
      /** Scroll distance (px) from the top that triggers loading an older chunk. */
      const TAPE_TOP_TRIGGER_PX = 120;
      // RALPHUS-TAPE-SCROLL:BEGIN
      /**
       * @typedef {object} TapeWindow
       * @property {number} loadedStart - Byte offset of the first loaded byte.
       * @property {number} loadedEnd - Byte offset just past the last loaded byte.
       * @property {number} total - Last known transcript size in bytes.
       * @property {string} text - The decoded bytes for `[loadedStart, loadedEnd)`, verbatim (ANSI included).
       */
      /**
       * @typedef {object} TapeChunk
       * @property {number} start - Byte offset the server says `content` begins at.
       * @property {string} content - Decoded content of the returned range.
       * @property {number} total - Transcript size at read time.
       * @property {number} [requested] - Byte limit the caller requested (append/seed only); `loadedEnd` advances by this, not by the returned string's byte length, so a server-side redaction that shortens `content` can't desync the offset.
       */
      /**
       * A never-fetched, empty tape window.
       * @returns {TapeWindow}
       */
      function emptyTapeWindow() { return { loadedStart: 0, loadedEnd: 0, total: 0, text: "" }; }
      /**
       * UTF-8 byte length of a JS string — the tape's coordinate space is
       * bytes, but a JS string is UTF-16, so `.length` is not it.
       * @param {string} s
       * @returns {number}
       */
      function utf8ByteLength(s) { return new TextEncoder().encode(s).length; }
      /**
       * Folds a follow-tail (or initial-seed) chunk into the window: the chunk
       * covers `[chunk.start, …)` at/after the current `loadedEnd`. An empty or
       * non-contiguous window adopts the chunk wholesale (the initial tail seed
       * goes through here too). `loadedEnd` advances by `min(requested, total −
       * start)`, never by the returned string's byte length, so a redaction
       * that shortens `content` can't desync the byte offset.
       *
       * NOTE — byte-cap freeze (RAL-397): once an attempt exceeds
       * `max_transcript_bytes_per_attempt` (default 256 MiB) the pipe-sink
       * stops appending and `total` freezes while the pane keeps producing, so
       * the tail stops advancing here. Only bites pathologically huge attempts;
       * called out so it isn't a silent surprise (see PSMUX_MEMORY_FIX.local.md
       * 2G "Liveness caveats").
       * @param {TapeWindow} w
       * @param {TapeChunk} chunk
       * @returns {TapeWindow}
       */
      function tapeAppend(w, chunk) {
        const requested = chunk.requested ?? chunk.content.length;
        const contiguous = w.text !== "" && chunk.start === w.loadedEnd;
        if (!contiguous) {
          return { loadedStart: chunk.start, loadedEnd: Math.min(chunk.start + requested, chunk.total), total: chunk.total, text: chunk.content };
        }
        return { loadedStart: w.loadedStart, loadedEnd: Math.min(w.loadedEnd + requested, chunk.total), total: chunk.total, text: w.text + chunk.content };
      }
      /**
       * Folds a load-older chunk into the window: the caller requests exactly
       * `[chunk.start, w.loadedStart)`, so `loadedStart` moves back to
       * `chunk.start` with no byte-length math — redaction-proof by
       * construction. Prepends the text.
       * @param {TapeWindow} w
       * @param {TapeChunk} chunk
       * @returns {TapeWindow}
       */
      function tapePrepend(w, chunk) {
        return { loadedStart: chunk.start, loadedEnd: w.loadedEnd, total: w.total, text: chunk.content + w.text };
      }
      /**
       * Bounds the in-memory window while following the tail: once `text`
       * exceeds `maxChars`, drop the front and advance `loadedStart` by the
       * *true* UTF-8 byte length of what was dropped, so it stays a valid byte
       * offset load-older can still page back from. Only ever applied on the
       * follow-tail path; scrolling up (prepend) deliberately grows past the
       * cap since the user asked to see that history.
       * @param {TapeWindow} w
       * @param {number} maxChars
       * @returns {TapeWindow}
       */
      function tapeTrimFront(w, maxChars) {
        if (w.text.length <= maxChars) return w;
        const dropChars = w.text.length - maxChars;
        const droppedBytes = utf8ByteLength(w.text.slice(0, dropChars));
        return { loadedStart: w.loadedStart + droppedBytes, loadedEnd: w.loadedEnd, total: w.total, text: w.text.slice(dropChars) };
      }
      /**
       * The complete lines currently renderable from the window. A partial
       * leading line (present when `loadedStart > 0`, since a byte range can cut
       * a line's start off) is dropped; a partial trailing line (text after the
       * last newline) is held back unless the cell has ended AND the window
       * reaches the file's true end — so a marker line split at either edge is
       * never half-classified (same carry idea as the Rust `TranscriptTailer`).
       * @param {TapeWindow} w
       * @param {boolean} ended
       * @returns {string[]}
       */
      function tapeCompleteLines(w, ended) {
        let text = w.text;
        if (w.loadedStart > 0) {
          const nl = text.indexOf("\n");
          text = nl === -1 ? "" : text.slice(nl + 1);
        }
        const atEnd = w.loadedEnd >= w.total;
        const parts = text.split("\n");
        const trailing = parts.pop();
        if (trailing !== undefined && trailing !== "" && ended && atEnd) parts.push(trailing);
        return parts;
      }
      // RALPHUS-TAPE-SCROLL:END

      // RALPHUS-TAPE-LINES:BEGIN
      // Pure line pipeline for the single-tape Live View (RAL-397 Phase 2G-A):
      // ANSI-strip each complete line (a JS mirror of
      // daemon/src/terminal_log.rs::strip_ansi_escapes), then classify it. The
      // tape is served raw — `/pane-transcript` does NOT strip ralphus's own
      // marker lines the way `/pane` does — so this is where they're handled.
      // Sliced out and driven standalone by test/board-tape-lines.mjs.
      /** Internal tmux-completion sentinel; always dropped, never shown in any debug state. Matches daemon/src/runner.rs::TMUX_DONE_MARKER. */
      const RALPHUS_TAPE_DONE_PREFIX = "RALPHUS_TMUX_DONE:";
      /** Cartographer event marker, trailing space included. Matches runner/src/cartographer.rs::EVENT_MARKER. */
      const RALPHUS_TAPE_EVENT_PREFIX = "RALPHUS_EVENT: ";
      /**
       * Strip ANSI/VT100 escape sequences from `s` — a faithful JS port of
       * daemon/src/terminal_log.rs::strip_ansi_escapes (RAL-397 Phase 2E):
       * CSI (`ESC [` … final byte `@`–`~`), OSC (`ESC ]` … BEL or `ESC \`), and
       * bare two-char escapes as a catch-all. Not a full ECMA-48 parser — the
       * same scope as the Rust original, sufficient for real pane output.
       * @param {string} s
       * @returns {string}
       */
      function stripAnsiEscapes(s) {
        const ESC = "\x1b", BEL = "\x07";
        let out = "";
        let i = 0;
        while (i < s.length) {
          const c = s[i];
          if (c !== ESC) { out += c; i++; continue; }
          const next = s[i + 1];
          if (next === "[") {
            i += 2;
            while (i < s.length) { const ch = s[i]; i++; if (ch >= "@" && ch <= "~") break; }
          } else if (next === "]") {
            i += 2;
            while (i < s.length) {
              const ch = s[i];
              if (ch === BEL) { i++; break; }
              if (ch === ESC && s[i + 1] === "\\") { i += 2; break; }
              i++;
            }
          } else if (next !== undefined) {
            i += 2; // consume ESC + the single following char
          } else {
            i += 1; // bare trailing ESC
          }
        }
        return out;
      }
      /**
       * Render one `RALPHUS_EVENT` payload (the trailing JSON after the marker)
       * as a concise inline one-liner — Debug-ON only. Mirrors
       * `formatDebugEvent`'s `source: message` tone, with a compact
       * usage/session-id detail when the payload carries one. Malformed JSON
       * falls back to the raw payload rather than throwing.
       * @param {string} payloadJson
       * @returns {string}
       */
      function formatInlineTapeEvent(payloadJson) {
        let ev;
        try { ev = JSON.parse(payloadJson); } catch (_) { return `⟨debug⟩ ${payloadJson}`; }
        const source = typeof ev.source === "string" ? ev.source : "event";
        const message = typeof ev.message === "string" ? ev.message : "";
        const p = (ev && typeof ev.payload === "object" && ev.payload) ? ev.payload : {};
        let detail = "";
        if (typeof p.cost_usd === "number") {
          const tin = typeof p.tokens_in === "number" ? p.tokens_in : 0;
          const tout = typeof p.tokens_out === "number" ? p.tokens_out : 0;
          detail = ` — in ${tin} / out ${tout} tok · $${p.cost_usd.toFixed(4)}`;
        } else if (typeof p.agent_session_id === "string") {
          detail = ` — session ${p.agent_session_id}`;
        }
        return `⟨debug⟩ ${source}: ${message}${detail}`;
      }
      /**
       * Classify + render one already-ANSI-stripped line, or return null to
       * drop it: `RALPHUS_TMUX_DONE` lines are always dropped (internal
       * sentinel); `RALPHUS_EVENT` lines are dropped when Debug is off and
       * rendered inline when on; everything else is agent output, kept verbatim
       * (a trailing `\r` from `\r\n` is trimmed for tidy display).
       * @param {string} line
       * @param {boolean} showDebug
       * @returns {string|null}
       */
      function classifyTapeLine(line, showDebug) {
        const clean = line.replace(/\r$/, "");
        if (clean.startsWith(RALPHUS_TAPE_DONE_PREFIX)) return null;
        if (clean.startsWith(RALPHUS_TAPE_EVENT_PREFIX)) {
          if (!showDebug) return null;
          return formatInlineTapeEvent(clean.slice(RALPHUS_TAPE_EVENT_PREFIX.length));
        }
        return clean;
      }
      /**
       * Run the full pipeline over a tape's complete lines — ANSI-strip,
       * classify, drop nulls, join — to the single text the Live View renders.
       * @param {string[]} lines
       * @param {boolean} showDebug
       * @returns {string}
       */
      function renderTapeLines(lines, showDebug) {
        const out = [];
        for (const raw of lines) {
          const rendered = classifyTapeLine(stripAnsiEscapes(raw), showDebug);
          if (rendered !== null) out.push(rendered);
        }
        return out.join("\n");
      }
      // RALPHUS-TAPE-LINES:END

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
      // RALPHUS-DEBUG-STREAM:BEGIN
      // Pure, DOM/fetch/module-state-free logic for the unified debug/
      // terminal-log stream (RAL-296) -- kept free of `peekContent`/other
      // module-level state so it can be sliced out and evaluated standalone by
      // test/board-debug-stream.mjs, the same pattern test/board-peek-state.mjs
      // and test/board-merge-button.mjs use for their own regions. See
      // librarian/AGENTS.md's "Testing -- Frontend" section before moving these
      // markers. Used by the terminal-log *history* box (`viewHistoryAttempt`)
      // to render a past attempt's merged debug stream; the live Live View box
      // renders debug inline from the tape instead (RAL-397 Phase 2G-A), so it
      // no longer consumes these.
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
          delete peekTape[key]; // drop the loaded transcript-tape window too (RAL-397 Phase 2G-A)
          peekLoadingOlder.delete(key);
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
       * (RAL-232, RAL-397 Phase 2G-A). Debug is now rendered *inline* from the
       * already-loaded transcript-tape window (the `RALPHUS_EVENT` marker lines
       * are shown in place when on, dropped when off), so this just flips the
       * flag and re-renders that window immediately — no fetch, no flash.
       * @param {string} key
       * @param {boolean} checked
       * @returns {void}
       */
      function toggleShowDebugMessages(key, checked) {
        peekShowDebug[key] = checked;
        if (peekTape[key] === undefined) return;
        renderPeekTape(key);
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
