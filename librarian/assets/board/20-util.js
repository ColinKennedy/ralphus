      // ---------- helpers ----------
      /**
       * Gets an element by id, asserting it is present. Use only where the
       * element is guaranteed to exist by the page's static structure (or by
       * a render call earlier in the same function) — same non-null
       * assumption the original unguarded `document.getElementById(...)`
       * call sites already made; this just satisfies strictNullChecks
       * without changing behavior (still throws if the id is ever missing).
       * @param {string} id
       * @returns {HTMLElement}
       */
      const byId = (id) => /** @type {HTMLElement} */ (document.getElementById(id));
      /**
       * HTML-escapes a value for safe interpolation into a template string.
       * @param {*} s
       * @returns {string}
       */
      const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) => (/** @type {Record<string, string>} */ ({ "&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;","'":"&#39;" }))[c]);
      /**
       * Reads a CSS custom property off `:root`, falling back to `var(--muted)`.
       * @param {string} n
       * @returns {string}
       */
      const cvar = (n) => getComputedStyle(document.documentElement).getPropertyValue(n) || "var(--muted)";
      /**
       * Stamps the shared freshness indicator with the current time. Call from a
       * poller's success path, once that poller's own data has landed.
       * @returns {void}
       */
      const markUpdated = () => { byId("updated").textContent = "updated " + new Date().toLocaleTimeString(); };
      /**
       * Marks the board as unable to reach the daemon: clears the connection dot
       * and replaces the freshness stamp, so a stale time is never left standing
       * next to data that failed to load.
       * @returns {void}
       */
      const markUnreachable = () => { byId("conn").className = "dot off"; byId("updated").textContent = "daemon unreachable"; };

      // ---- ralphus URI scheme (RAL-188) ----
      // One self-describing way to address any squad/task/cell/proof or
      // review, readable cold by a human or an AI agent:
      //
      //   ralphus:/SQUAD[my squad]/TASK[ral-178]/CELL[work]?id=squad-000000000151
      //   ralphus:/SQUAD[my squad]/TASK[ral-178]/PROOF[~0]?id=squad-000000000151
      //   ralphus:/REVIEW[RAL-174/175 batch]?id=guardian-000000000003
      //
      // This is the third implementation of the same grammar, alongside
      // `cli/src/ralphus/uri.py` and `core/src/uri.rs` — all three must stay in
      // lockstep. It is hand-written rather than imported because board.html
      // has no build step (RAL-188 §C.8).
      //
      // Every character the grammar gives meaning to is named below as a
      // `URI_*_TOKEN` constant, and every parser here compares against those
      // names rather than a bare literal — so "is this `/` a separator or part
      // of a label?" is answerable by reading the code, and the Python/Rust
      // twins can be diffed against this one by name.
      //
      // Rules that matter here (§C):
      //  · `[` `]` and `/` are percent-encoded *inside* a label; a balanced
      //    `[...]` group is extracted before the path is split on `/`, so a
      //    review literally named "RAL-174/175 batch" still parses.
      //  · A `~N` token is a position; a bare token is always a name, so
      //    `PROOF[~0]` and `PROOF[0]` address different things. The sigil is
      //    `~` — an RFC 3986 *unreserved* character — precisely because this
      //    file puts whole URIs in `location.hash`, where a `#` would be read
      //    as a second fragment delimiter.
      //  · `?id=` is authoritative and the board always emits it, so a link
      //    survives the label being renamed.
      // RALPHUS-URI-CODEC:BEGIN
      /** Opens a segment's bracketed value: the `[` of `TASK[ral-178]`. */
      const URI_VALUE_OPEN_TOKEN = "[";
      /** Closes a segment's bracketed value: the `]` of `TASK[ral-178]`. */
      const URI_VALUE_CLOSE_TOKEN = "]";
      /** Separates one path segment from the next: the `/` of `SQUAD[r]/TASK[t]`. */
      const URI_SEGMENT_SEPARATOR_TOKEN = "/";
      /** Starts the query string: the `?` of `SQUAD[r]?id=squad-1`. */
      const URI_QUERY_OPEN_TOKEN = "?";
      /** Separates one query pair from the next: the `&` of `?id=g-1&combined`. */
      const URI_QUERY_PAIR_SEPARATOR_TOKEN = "&";
      /** Separates a query key from its value: the `=` of `?id=squad-1`. */
      const URI_QUERY_ASSIGN_TOKEN = "=";
      /** Introduces a positional index rather than a name: the `~` of `PROOF[~0]`. */
      const URI_INDEX_SIGIL_TOKEN = "~";
      /** Introduces a two-hex-digit percent-escape inside a label or query value. */
      const URI_PERCENT_TOKEN = "%";
      /** RFC 3986's fragment delimiter. Not a grammar token — escaped only so an embedded URI survives a real URL parser. */
      const URI_FRAGMENT_TOKEN = "#";
      /** Not a grammar token either: form-encoded query parsers read a raw `+` as a space. */
      const URI_PLUS_TOKEN = "+";
      /** Matches a bracket label or query value that is exactly a positional token (`~2`). */
      const URI_POSITIONAL_RE = new RegExp(`^${URI_INDEX_SIGIL_TOKEN}\\d+$`);
      /** Characters that must never appear raw inside a label: the grammar's own tokens plus what a URL parser would eat. */
      const URI_MUST_ENCODE = [
        URI_PERCENT_TOKEN,
        URI_VALUE_OPEN_TOKEN,
        URI_VALUE_CLOSE_TOKEN,
        URI_SEGMENT_SEPARATOR_TOKEN,
        URI_QUERY_OPEN_TOKEN,
        URI_QUERY_PAIR_SEPARATOR_TOKEN,
        URI_QUERY_ASSIGN_TOKEN,
        URI_INDEX_SIGIL_TOKEN,
        URI_FRAGMENT_TOKEN,
        URI_PLUS_TOKEN,
      ].join("");
      /**
       * Percent-encodes a label so it is safe as bracket contents. Spaces are
       * deliberately left raw — legibility is the point, and a balanced
       * `[...]` group cannot be broken by one.
       * @param {string} raw
       * @returns {string}
       */
      function uriEncodeLabel(raw) {
        let out = "";
        for (const ch of String(raw ?? "")) {
          const code = ch.codePointAt(0) ?? 0;
          if (URI_MUST_ENCODE.includes(ch) || code < 0x20 || code > 0x7e) {
            // encodeURIComponent escapes most of URI_MUST_ENCODE (and
            // everything non-ASCII) as uppercase UTF-8 %XX bytes, exactly the
            // encoding the Python/Rust twins produce — but it deliberately
            // leaves the RFC 3986 *unreserved* set alone, which now includes
            // our own `~` sigil. Escape by hand whenever it declines, or a
            // label literally named `~2` would come back as a position.
            const escaped = encodeURIComponent(ch);
            out += escaped === ch ? URI_PERCENT_TOKEN + code.toString(16).toUpperCase().padStart(2, "0") : escaped;
          } else {
            out += ch;
          }
        }
        return out;
      }
      /**
       * Percent-encodes a query value, leaving a well-formed positional token
       * (`~2`) raw. `uriEncodeLabel` escapes the sigil everywhere, which is
       * what stops a label beginning with `~` from reading as a position — but
       * `?worktree=` carries positions too, and `?worktree=%7E2` would be
       * correct yet unreadable.
       * @param {string} raw
       * @returns {string}
       */
      function uriEncodeQueryValue(raw) {
        const text = String(raw ?? "");
        return URI_POSITIONAL_RE.test(text) ? text : uriEncodeLabel(text);
      }
      /**
       * Percent-decodes bracket contents or a query value. Returns null on a
       * malformed escape, so a mis-encoded label fails visibly rather than
       * silently addressing something else.
       * @param {string} raw
       * @returns {string|null}
       */
      function uriDecodeLabel(raw) {
        try {
          return decodeURIComponent(raw);
        } catch {
          return null;
        }
      }
      /**
       * @typedef {object} UriSegment
       * @property {string} kind one of SQUAD/TASK/CELL/PROOF/REVIEW
       * @property {string|null} name the decoded label, when addressed by name
       * @property {number|null} index the position, when addressed as `~N`
       */
      /**
       * @typedef {object} ParsedUri
       * @property {UriSegment[]} segments
       * @property {{[key: string]: string|null}} query `null` marks a bare flag such as `?combined`
       */
      /**
       * Whether a string should be read as a ralphus URI rather than a legacy
       * `kind:ti:si` selector. Requires a `[` so the unrelated
       * `ralphus:new-review/<key>` TOML link syntax isn't captured.
       * @param {string|null|undefined} raw
       * @returns {boolean}
       */
      function looksLikeUri(raw) {
        if (!raw || !raw.includes(URI_VALUE_OPEN_TOKEN)) return false;
        if (raw.startsWith("ralphus:")) return true;
        const head = raw.split(URI_VALUE_OPEN_TOKEN)[0];
        return head.length > 0 && /^[A-Z]+$/.test(head);
      }
      /**
       * Parses a ralphus URI. Returns null (never throws) if it is malformed —
       * every caller falls back to the legacy hash form on null.
       * @param {string} raw
       * @returns {ParsedUri|null}
       */
      function parseRalphusUri(raw) {
        let text = String(raw ?? "").trim();
        if (text.startsWith("ralphus:")) text = text.slice("ralphus:".length);
        text = text.replace(/^\/+/, "");
        if (!text) return null;
        // Split at the first bracket-depth-zero `?`, so a `?` inside a label
        // stays part of the label.
        let depth = 0, cut = -1;
        for (let i = 0; i < text.length && cut < 0; i++) {
          if (text[i] === URI_VALUE_OPEN_TOKEN) depth++;
          else if (text[i] === URI_VALUE_CLOSE_TOKEN) depth = Math.max(0, depth - 1);
          else if (text[i] === URI_QUERY_OPEN_TOKEN && depth === 0) cut = i;
        }
        const path = cut < 0 ? text : text.slice(0, cut);
        const queryText = cut < 0 ? "" : text.slice(cut + 1);
        /** @type {UriSegment[]} */
        const segments = [];
        let i = 0;
        while (i < path.length) {
          const open = path.indexOf(URI_VALUE_OPEN_TOKEN, i);
          if (open < 0) return null;
          const kind = path.slice(i, open);
          let d = 1, j = open + 1;
          for (; j < path.length && d > 0; j++) {
            if (path[j] === URI_VALUE_OPEN_TOKEN) d++;
            else if (path[j] === URI_VALUE_CLOSE_TOKEN) d--;
          }
          if (d > 0) return null;
          const label = path.slice(open + 1, j - 1);
          if (!label) return null;
          if (URI_POSITIONAL_RE.test(label)) {
            segments.push({ kind, name: null, index: Number(label.slice(URI_INDEX_SIGIL_TOKEN.length)) });
          } else {
            const name = uriDecodeLabel(label);
            if (name === null) return null;
            segments.push({ kind, name, index: null });
          }
          if (j >= path.length) break;
          if (path[j] !== URI_SEGMENT_SEPARATOR_TOKEN) return null;
          i = j + 1;
          if (i >= path.length) return null;
        }
        if (!segments.length) return null;
        /** @type {{[key: string]: string|null}} */
        const query = {};
        for (const part of queryText.split(URI_QUERY_PAIR_SEPARATOR_TOKEN)) {
          if (!part) continue;
          const eq = part.indexOf(URI_QUERY_ASSIGN_TOKEN);
          if (eq < 0) { query[part] = null; continue; }
          const value = uriDecodeLabel(part.slice(eq + 1));
          if (value === null) return null;
          query[part.slice(0, eq)] = value;
        }
        return { segments, query };
      }
      /**
       * Renders one `TYPE[label]` segment from a name, or from a position when
       * the entity has no name of its own (an anonymous proof step).
       * @param {string} kind
       * @param {string|null|undefined} name
       * @param {number} index
       * @returns {string}
       */
      function uriSegment(kind, name, index) {
        const value = name ? uriEncodeLabel(name) : `${URI_INDEX_SIGIL_TOKEN}${index}`;
        return `${kind}${URI_VALUE_OPEN_TOKEN}${value}${URI_VALUE_CLOSE_TOKEN}`;
      }
      // RALPHUS-URI-CODEC:END
      /**
       * Builds the canonical URI for the current tasks-tab selection inside `squad`.
       * Always carries the `?id=` sidecar, so the link survives a rename (§C.3).
       * @param {SquadView} squad
       * @param {SelStateTasks} s
       * @returns {string}
       */
      function squadSelectionUri(squad, s) {
        let path = uriSegment("SQUAD", squad.label || squad.id, 0);
        if (s.kind && s.kind !== "squad") {
          const task = (squad.tasks || [])[s.taskIdx];
          path += URI_SEGMENT_SEPARATOR_TOKEN + uriSegment("TASK", task && task.name, s.taskIdx);
          // A task selection stops there. `cellIdx === -1` is the board's
          // own marker for a *task-scope* proof step — it has no owning
          // cell, so it gets no CELL segment either.
          const underCell = s.kind === "cell" || (s.kind === "proof" && s.cellIdx !== -1);
          /** @type {ProofView[]} */
          let steps = (task && task.proof) || [];
          if (underCell) {
            const cell = task && (task.cells || [])[s.cellIdx];
            path += URI_SEGMENT_SEPARATOR_TOKEN + uriSegment("CELL", cell && (cell.name || cell.id), s.cellIdx);
            steps = (cell && cell.proof) || [];
          }
          if (s.kind === "proof") {
            const vi = s.proofIdx ?? 0;
            path += URI_SEGMENT_SEPARATOR_TOKEN + uriSegment("PROOF", steps[vi] && steps[vi].id, vi);
          }
        }
        return `ralphus:${URI_SEGMENT_SEPARATOR_TOKEN}${path}${URI_QUERY_OPEN_TOKEN}id${URI_QUERY_ASSIGN_TOKEN}${uriEncodeQueryValue(squad.id)}`;
      }
      /**
       * Resolves a parsed squad-family URI against a loaded squad view, turning its
       * names back into the positional indices the board renders with.
       * Returns null if any segment names something the squad doesn't have.
       * @param {ParsedUri} uri
       * @param {SquadView} squad
       * @returns {SelStateTasks|null}
       */
      function selFromUri(uri, squad) {
        /**
         * @param {UriSegment|undefined} segment
         * @param {(string|null|undefined)[]} names
         * @returns {number}
         */
        const idx = (segment, names) => {
          if (!segment) return -1;
          if (segment.index !== null) return segment.index < names.length ? segment.index : -1;
          // A nameless entity contributes an empty candidate and is reachable
          // only as `~N`, so it must never win a name lookup.
          const hits = names.map((n, i) => (n && n === segment.name ? i : -1)).filter((i) => i >= 0);
          return hits.length === 1 ? hits[0] : -1;
        };
        const bySegment = /** @type {{[kind: string]: UriSegment}} */ ({});
        for (const segment of uri.segments) bySegment[segment.kind] = segment;
        if (!bySegment.TASK) return { kind: "squad", taskIdx: 0, cellIdx: 0, proofIdx: -1 };
        const tasks = squad.tasks || [];
        const taskIdx = idx(bySegment.TASK, tasks.map((t) => t.name));
        if (taskIdx < 0) return null;
        const task = tasks[taskIdx];
        const proofSeg = bySegment.PROOF;
        if (!bySegment.CELL) {
          if (!proofSeg) return { kind: "task", taskIdx, cellIdx: -1, proofIdx: -1 };
          const proofIdx = idx(proofSeg, (task.proof || []).map((v) => v.id));
          if (proofIdx < 0) return null;
          return { kind: "proof", taskIdx, cellIdx: -1, proofIdx, proofScope: "task" };
        }
        const cells = task.cells || [];
        const cellIdx = idx(bySegment.CELL, cells.map((s) => s.name || s.id));
        if (cellIdx < 0) return null;
        if (!proofSeg) return { kind: "cell", taskIdx, cellIdx, proofIdx: -1 };
        const proofIdx = idx(proofSeg, (cells[cellIdx].proof || []).map((v) => v.id));
        if (proofIdx < 0) return null;
        return { kind: "proof", taskIdx, cellIdx, proofIdx, proofScope: "cell" };
      }
      /**
       * Builds the canonical URI for the Tasks tab's own selection scheme
       * (RAL-362 §7) -- deliberately disjoint from `squadSelectionUri`'s: it
       * starts with a TASK segment (never SQUAD), carrying the squad only as
       * a `?sq=` sidecar, so `isLegacySquadsTasksHash` can tell old squad-viewer
       * links apart from links into this tab without any extra state.
       * @param {string} squadId
       * @param {TaskView} task
       * @param {TaskTabSel} s
       * @returns {string}
       */
      function taskTabSelectionUri(squadId, task, s) {
        let path = uriSegment("TASK", task.name, s.taskIdx);
        if (s.kind === "cell") {
          const cell = (task.cells || [])[s.cellIdx];
          path += URI_SEGMENT_SEPARATOR_TOKEN + uriSegment("CELL", cell && (cell.name || cell.id), s.cellIdx);
        }
        return `ralphus:${URI_SEGMENT_SEPARATOR_TOKEN}${path}${URI_QUERY_OPEN_TOKEN}sq${URI_QUERY_ASSIGN_TOKEN}${uriEncodeQueryValue(squadId)}`;
      }
      /**
       * Resolves a Tasks-tab selection URI against the loaded squads list,
       * turning its `?sq=`/TASK/CELL names back into positional indices.
       * Returns null if the squad, task or cell it names can't be found.
       * @param {ParsedUri} uri
       * @returns {TaskTabSel|null}
       */
      function taskTabSelFromUri(uri) {
        const squadId = uri.query.sq;
        const squad = squadId ? findSquad(squadId) : null;
        if (!squad) return null;
        const bySegment = /** @type {{[kind: string]: UriSegment}} */ ({});
        for (const segment of uri.segments) bySegment[segment.kind] = segment;
        if (!bySegment.TASK) return null;
        const tasks = squad.tasks || [];
        const taskIdx = bySegment.TASK.index !== null ? bySegment.TASK.index : tasks.findIndex((t) => t.name === bySegment.TASK.name);
        if (taskIdx < 0 || taskIdx >= tasks.length) return null;
        if (!bySegment.CELL) return { kind: "task", squadId: squad.id, taskIdx, cellIdx: -1 };
        const cells = tasks[taskIdx].cells || [];
        const cellIdx = bySegment.CELL.index !== null
          ? bySegment.CELL.index
          : cells.findIndex((c) => (c.name || c.id) === bySegment.CELL.name);
        if (cellIdx < 0 || cellIdx >= cells.length) return null;
        return { kind: "cell", squadId: squad.id, taskIdx, cellIdx };
      }
      /**
       * Whitelists a state/status string to identifier-safe characters
       * before it reaches markup (RAL-231) -- structural fix so
       * pill()/sdot()/gdot() can never emit HTML-breaking content,
       * regardless of what a future caller passes in. Every real state
       * string (a Rust enum variant serialized as JSON) is already exactly
       * this shape, so this is a no-op for every value used today.
       * @param {string} s
       * @returns {string}
       */
      const safeState = (s) => /^[A-Za-z0-9_]*$/.test(s) ? s : "unknown";
      /**
       * Renders a colored status dot for a state name.
       * @param {string} s
       * @returns {string}
       */
      const sdot = (s) => { const safe = safeState(s); return `<span class="dot" style="background:${cvar("--"+safe)}"></span>`; };
      /**
       * Renders a status pill for a state name.
       * @param {string} s
       * @returns {string}
       */
      const pill = (s) => { const safe = safeState(s); return `<span class="pill p-${safe}">${safe}</span>`; };
      /**
       * Renders a "soloed" badge for a task (RAL-157), or an empty string when
       * the task isn't soloed.
       * @param {TaskView} t
       * @returns {string}
       */
      const soloBadge = (t) => t.soloed
        ? `<span class="pill p-solo" data-tip="This task is soloed — every other task in the squad is paused (their not-yet-started cells won't launch) until un-soloed.\nRight-click this task and choose \"Un-solo task\" to resume the others.">★ solo</span>`
        : "";
      /**
       * Renders a cosmetic "out of date" badge (RAL-271) for a task/cell/proof
       * step whose own environment-variable overrides were edited after it
       * last ran/retried or had its status explicitly set, or an empty string
       * when the flag isn't set. Purely informational — nothing is blocked
       * and no proof result is invalidated.
       * @param {boolean} [flag]
       * @returns {string}
       */
      const outOfDateBadge = (flag) => flag
        ? ` <span class="pill p-out-of-date" data-tip="This item's environment-variable overrides changed since it last ran/retried — a purely cosmetic signal, nothing is blocked or invalidated.\nWho/when: notice this before trusting an old run's results as still reflecting the current overrides.\nClears automatically the next time this item runs/retries, or when its status is set explicitly (e.g. via Set Status).">⚠ env out of date</span>`
        : "";
      /**
       * Renders a "detached" badge (RAL-288) for a cell that reads `running`
       * but has actually been cleanly stopped for a real interactive agent
       * session to take over — not stuck, not stalled, and not Done/Failed/
       * Cancelled either. `detachedAtMs` is the cell's own `detached_at_ms`
       * field (present only while genuinely detached).
       * @param {number|null|undefined} detachedAtMs
       * @returns {string}
       */
      const detachedBadge = (detachedAtMs) => detachedAtMs
        ? ` <span class="pill p-detached" data-tip="This cell cleanly stopped so a real interactive agent session could take over — it is not stuck, and its Live View below now shows the historical record of that stopped process, not a hang.\nWho/when: you (or someone else) clicked 'Open Agent' and the live conversation continues in a separate terminal (tmux session), outside this board.\nUse 'Open Agent' again to reattach a terminal to that live session, or 'Resume Automation' once you are done with it to continue unattended from where it left off.">⏸ detached — open agent to reattach</span>`
        : "";
      /**
       * Compact "detached" badge (RAL-288) for the graph tree's cell node,
       * where space is tight — same signal and tooltip as {@link detachedBadge},
       * just the bare word instead of the full explanatory label.
       * @param {number|null|undefined} detachedAtMs
       * @returns {string}
       */
      const detachedGraphBadge = (detachedAtMs) => detachedAtMs
        ? ` <span class="pill p-detached" data-tip="This cell cleanly stopped so a real interactive agent session could take over — it is not stuck.\nWho/when: 'Open Agent' was clicked and the live conversation continues in a separate terminal (tmux session), outside this board.\nUse 'Open Agent' again to reattach, or 'Resume Automation' to continue unattended from where it left off.">detached</span>`
        : "";
      /** @type {{[key: string]: ("task"|"cell"|"proof")[]}} */
      const GRAPH_NODE_ACTION_COMPAT = {
        restart: ["task", "cell", "proof"],
        stop: ["task", "cell", "proof"],
        status: ["task", "cell", "proof"],
        solo: ["task"],
        unsolo: ["task"],
      };
      /**
       * Looks up a squad by id from the in-memory `squads` list.
       * @param {string|null|undefined} id
       * @returns {SquadView|undefined}
       */
      const findSquad = (id) => squads.find((r) => r.id === id);
      /**
       * Looks up a review by id from the in-memory `guardians` list.
       * @param {string|null|undefined} id
       * @returns {GuardianView|undefined}
       */
      const findGuardian = (id) => guardians.find((g) => g.id === id);
      // RAL-122: a squad that's Pending purely because a configured scheduler
      // down-time window is active gets a distinct "waiting" label/color
      // instead of looking like an ordinary Pending squad stuck for no visible
      // reason. This is a display-only relabeling — the underlying state is
      // still "pending" (filters, menus, and the API all use the real state).
      const WAITING_TIP = "Waiting for the configured scheduler down-time window to end before this squad can be picked up.\nShown instead of \"pending\" when the daemon is currently inside a down-time window (see [daemon] in .ralphus.toml).\nIn-progress squads are never paused — this only affects new automatic pickups.";
      const DEFAULT_AGENT = "claude";
      // RAL-160: token/cost figures shown in the cell detail pane reflect
      // only this cell's current squad — a restart overwrites them rather
      // than accumulating, so they are never a running total across retries.
      const TOKENS_COST_TIP = "Input tokens: uncached text sent to the model (prompt, system prompt, tool results).\nOutput tokens: text the model generated in response.\ncache write / cache read: prompt-cache tokens (RAL-326), billed at their own rates and deliberately kept out of the input figure — for a long agentic cell they are usually the bulk of what was actually billed, which is why an input count on its own can look implausibly small.\ncompaction input: tokens spent on Claude Code's own auto-compaction summarization requests (RAL-373) — also kept out of the input figure. See the \"compaction\" row below for what that costs.\nShown only when the backend reports them: claude-code, codex and pi do; the native pydantic backend and external harnesses do not.\n$ cost is for this squad only — it is not cumulative across restarts of this cell.\nN/A means this cell's agent backend reported no dollar figure at all (Codex and the native pydantic backend never do) — it does not mean the cell was free.\nWhile a cell is running these update as each agent turn completes, so they read 0 until the first turn finishes.";
      // RAL-160: restarting a cell overwrites its tokens/cost in place, so
      // the running total across every attempt only survives in Cartographer's
      // "cell completed" event history — load it on demand rather than on
      // every render, since it's an extra API call a routine refresh should not repeat.
      // RAL-326: that history is queried by `cell_id` *and* `squad_id`. A cell
      // with no explicit `id` in its TOML gets a purely positional default sid
      // (`cell-0`, `cell-1`, ...), so every squad's Nth cell shares one --
      // unscoped, the sum silently swept in unrelated cells from unrelated
      // squads across the daemon's entire history.
      const LIFETIME_COST_TIP = "Sums tokens and cost across every completed attempt of this cell within this squad — every restart and retry of it, not just the attempt shown above.\nScoped to this squad on purpose: a cell with no explicit id in its TOML gets a positional one (cell-0, cell-1, ...) that every squad reuses, so a cross-squad total would be summing unrelated cells from unrelated squads.\nWho/when: check this to see total spend on a cell that has been restarted several times inside this squad.\nLoaded on demand from the Cartographer event log — click to fetch; older attempts may be missing if Cartographer retention has pruned them.";
      // RAL-304: read-only — set via `maximum_context`/`auto_compact_threshold`
      // in the submitted TOML, not editable here. Only shown when at least one
      // is set, since most cells set neither.
      const CONTEXT_LIMITS_TIP = "Context-window controls delivered to the backend at cell start, resolved from the cell (or its owning task) at submit time.\nmax = the context-window ceiling itself (maximum_context) — only codex and pi backends have a real lever for this.\ncompact @ = the token count at which the backend auto-compacts its own history (auto_compact_threshold) — also supported by claude-code, via its CLAUDE_CODE_AUTO_COMPACT_WINDOW env var.\nWho/when: check this on a long-running cell to see whether/where it's set up to compact instead of just running until it hits the wall and fails.\nRead-only here — edit the TOML and resubmit to change it.";
      // RAL-333: read-only — set via `maximum_tool_output_tokens` in the
      // submitted TOML, not editable here. Always shown (RAL-356), as "—"
      // when unset since most cells/proof steps leave it unset.
      const MAXIMUM_TOOL_OUTPUT_TOKENS_TIP = "Caps how many tokens a single tool-call result (e.g. a large file read) can inject into the agent's context, delivered via each backend's own mechanism: claude-code's CLAUDE_CODE_FILE_READ_MAX_OUTPUT_TOKENS env var, codex's tool_output_token_limit config, or pi's models.json maxTokens.\nWho/when: check this to see whether a cell/proof step is guarded against one huge tool output blowing out its context and forcing an auto-compact.\nRead-only here — edit the TOML and resubmit to change it.";
      // Same idea as TOKENS_COST_TIP/LIFETIME_COST_TIP above, for proof steps
      // (RAL-172): a proof step's own token/cost usage is now tracked
      // per-attempt server-side, and its lifetime total is loaded the same
      // on-demand way, scoped to this exact step (task + scope + position).
      const PROOF_TOKENS_COST_TIP = "Input tokens: uncached text sent to the model (prompt, system prompt, tool results) for this proof step.\nOutput tokens: text the model generated in response.\ncache write / cache read: prompt-cache tokens (RAL-326), billed at their own rates and kept out of the input figure; shown only when the backend reports them.\ncompaction input: tokens spent on Claude Code's own auto-compaction summarization requests (RAL-373); see the \"compaction\" row below for what that costs.\n$ cost is for this squad only — it is not cumulative across restarts of this proof step.\nN/A means this step's agent backend reported no dollar figure at all (Codex and the native pydantic backend never do) — it does not mean the step was free.";
      const PROOF_LIFETIME_COST_TIP = "Sums tokens and cost across every completed attempt of this proof step (same task + scope + step position) within this squad — every restart and retry of it, not just the attempt shown above.\nScoped to this squad for the same reason the cell-level total is: the step key is positional, so every squad running the same task file reuses it.\nWho/when: check this to see total spend on a proof step that has been restarted several times inside this squad.\nLoaded on demand from the Cartographer event log — click to fetch; older attempts may be missing if Cartographer retention has pruned them.";
      // RAL-373: Claude Code's own auto-compaction summarization requests are
      // billed at the uncached input rate and already folded into `cost_usd`
      // by the backend's authoritative total — this row is a *derived slice*
      // of that total, not additional spend, and must never be added on top
      // of it. Shown unconditionally (unlike the cache write/read segments
      // in usageSummary(), which are omitted at zero) because the count
      // alone — including zero — is exactly the signal `auto_compact_threshold`
      // gets sized from.
      const COMPACTION_TIP = "How many times Claude Code auto-compacted this run's own conversation history, and the USD cost attributable to those compactions (compaction input tokens × the model's uncached input rate).\nThis cost is already included in the $ figure above — it's a breakdown of part of that total, not extra spend on top of it.\nWho/when: check this when deciding whether to raise or lower auto_compact_threshold for this cell/proof step's agent — frequent compaction usually means the threshold is too low for the work being done.\n\"size not reported\" means compactions happened but this Claude Code version didn't report how many tokens each one summarized away — treat it as a nonzero, unknown cost, not a free compaction.";
      // RAL-187: not every agent backend reports a dollar cost. Codex's CLI has
      // no cost field anywhere in its output, and the native pydantic backend
      // returns 0.0 unconditionally, so both land here as a bare 0. Rendering
      // that as "$0.0000 USD" reads as "this squad was free" — the opposite of
      // the truth for a Codex cell that just burned millions of tokens. A
      // zero/absent figure therefore renders as "N/A": no price table, no
      // estimated number, and deliberately no per-backend branching, since
      // "nothing was reported" is the honest answer wherever it comes from.
      // The helpers below are kept pure and DOM-free so `test/
      // board-usage-summary.mjs` can slice them out of this file and run the
      // real shipped source under `node --test`. Keep the markers around them.
      // RALPHUS-USAGE-SUMMARY:BEGIN
      /**
       * Formats a reported USD cost, or "N/A" when no cost figure was reported.
       * @param {number|null|undefined} cost
       * @returns {string}
       */
      function fmtCostUsd(cost) {
        return cost ? `$${cost.toFixed(4)} USD` : "N/A";
      }
      // RAL-326: prompt-cache tokens are billed at their own rates, so they are
      // shown as their own segments rather than folded into "input" — folding
      // would silently change what every existing input reading means. The two
      // segments are omitted entirely when both are zero, which is both the
      // pre-RAL-326 row and the backend-reports-no-cache-breakdown case; there
      // is nothing to tell apart and nothing worth the row width either way.
      // RAL-373: compaction-input tokens are billed the same way (own bucket,
      // omitted at zero for the same reason) — see compactionSummary() below
      // for the derived-cost breakdown of what they add up to.
      /**
       * Renders one run's token/cost figures as a single line.
       * @param {{tokens_in?: number, tokens_out?: number, cache_creation_tokens?: number, cache_read_tokens?: number, compaction_input_tokens?: number, cost_usd?: number}} u
       * @returns {string}
       */
      function usageSummary(u) {
        const parts = [`input ${u.tokens_in ?? 0}`, `output ${u.tokens_out ?? 0}`];
        const cc = u.cache_creation_tokens ?? 0;
        const cr = u.cache_read_tokens ?? 0;
        if (cc || cr) parts.push(`cache write ${cc}`, `cache read ${cr}`);
        const ci = u.compaction_input_tokens ?? 0;
        if (ci) parts.push(`compaction input ${ci}`);
        parts.push(fmtCostUsd(u.cost_usd));
        return parts.join(" · ");
      }
      // RAL-373: mirrors `estimate_cost_usd`'s `rate_in` selection in
      // runner/src/claude_code_backend.rs so this display and the runner's
      // own conservative kill-switch estimate agree on what a given model's
      // uncached input costs per million tokens.
      /**
       * Approximate uncached-input rate (USD per million tokens) by
       * model-name substring.
       * @param {string|null|undefined} model
       * @returns {number}
       */
      function uncachedInputRateUsd(model) {
        const m = (model || "").toLowerCase();
        if (m.includes("haiku")) return 1.0;
        if (m.includes("sonnet") || m.includes("fable") || m.includes("mythos")) return 3.0;
        return 15.0;
      }
      /**
       * Derives the USD cost attributable to Claude Code's own auto-compaction
       * summarization requests: `compaction_input_tokens` billed at the
       * model's uncached input rate. This is a *slice* of `cost_usd` (which,
       * on this code path, already bills the compaction requests as part of
       * the backend's authoritative total) — never add it on top of `cost_usd`.
       * @param {string|null|undefined} model
       * @param {number} compactionInputTokens
       * @returns {number}
       */
      function compactionCostUsd(model, compactionInputTokens) {
        return (compactionInputTokens * uncachedInputRateUsd(model)) / 1_000_000;
      }
      /**
       * Renders one run's compaction count and derived cost as a single line
       * — e.g. "7 - $2.8200 USD", or "7 - size not reported" when
       * `compaction_count` is nonzero but `compaction_input_tokens` is still
       * 0 (an older Claude Code version that didn't report compaction size).
       * Always renders, including "0", unlike the cache/compaction *token*
       * segments in usageSummary() — the count alone is the signal
       * `auto_compact_threshold` gets sized from.
       * @param {{compaction_count?: number, compaction_input_tokens?: number}} u
       * @param {string|null|undefined} model
       * @returns {string}
       */
      function compactionSummary(u, model) {
        const count = u.compaction_count ?? 0;
        if (!count) return `${count}`;
        const tokens = u.compaction_input_tokens ?? 0;
        const costText = tokens ? fmtCostUsd(compactionCostUsd(model, tokens)) : "size not reported";
        return `${count} - ${costText}`;
      }
      const ESTIMATED_COST_TIP = "These figures are the last live mid-run snapshot, not the backend's own final accounting (RAL-326).\nThe process was lost, cancelled, timed out, or killed for exceeding its cost cap before a terminal usage event arrived, so what got recorded stops at whatever agent turn died and is priced by the runner's approximate per-model table rather than the backend's own total.\nTreat it as a floor on what was really spent, not a settled bill.";
      /**
       * Badge marking a usage row whose figures are a live snapshot rather
       * than a final accounting.
       * @param {boolean|undefined} isEstimated
       * @returns {string}
       */
      function estimatedBadge(isEstimated) {
        return isEstimated
          ? ` <span class="ro-badge" style="color:var(--muted)" data-tip="${ESTIMATED_COST_TIP}">≈ estimated</span>`
          : "";
      }
      // RALPHUS-USAGE-SUMMARY:END
      const SYSTEM_PROMPT_TIP = "Read-only — the effective system prompt actually appended to this agent invocation.\nIncludes ralphus-added hidden instructions (for unattended execution, proof mode, ghost handoff, etc.) plus any stored cell-level system prompt/addendum that applies.\nWho/when: inspect this when a cell or proof step behaved unexpectedly and you need to see the hidden instructions it received.";
      /**
       * True when a squad is Pending only because the scheduler down-time window is active.
       * @param {SquadView} r
       * @returns {boolean}
       */
      const isDowntimeWaiting = (r) => r.state === "pending" && !!(/** @type {any} */ (window)._daemonStatus && /** @type {any} */ (window)._daemonStatus.downtime_active);
      /**
       * Display state for a squad, relabeling downtime-waiting squads as "waiting".
       * @param {SquadView} r
       * @returns {string}
       */
      const squadDisplayState = (r) => isDowntimeWaiting(r) ? "waiting" : r.state;
      // RAL-96: mint a fresh W3C `traceparent` (root span) for every button
      // click that triggers a backend request, so the daemon/librarian/runner
      // spans it kicks off all land on the same trace. No opentelemetry-js
      // SDK here (board.html is plain HTML + inline JS, no build step) — this
      // is just the 32-hex-trace-id/16-hex-span-id wire format itself, per
      // https://www.w3.org/TR/trace-context/.
      /**
       * Mints a fresh W3C `traceparent` header value (root span, sampled).
       * @returns {string}
       */
      const newTraceparent = () => {
        /**
         * @param {number} n
         * @returns {string}
         */
        const hex = (n) => { const a = new Uint8Array(n); crypto.getRandomValues(a); return Array.from(a, (b) => b.toString(16).padStart(2, "0")).join(""); };
        return `00-${hex(16)}-${hex(8)}-01`;
      };
      /**
       * @returns {{traceparent: string}}
       */
      const traceHeaders = () => ({ traceparent: newTraceparent() });
      /**
       * POSTs JSON to the daemon API with a fresh trace header.
       * @param {string} path
       * @param {*} [body]
       * @returns {Promise<Response>}
       */
      const post = (path, body) => fetch(path, { method: "POST", headers: traceHeaders(), body: body ? JSON.stringify(body) : undefined });
      /**
       * Sends a DELETE to the daemon API with a fresh trace header.
       * @param {string} path
       * @returns {Promise<Response>}
       */
      const del = (path) => fetch(path, { method: "DELETE", headers: traceHeaders() });
      // copy-to-clipboard: a small button carrying its payload in data-copy.
      /**
       * Renders a copy-to-clipboard button carrying its payload in `data-copy`.
       * @param {string} text
       * @returns {string}
       */
      const copyBtn = (text) => `<button class="copy-btn" data-tip="Copy to clipboard." data-copy="${esc(text)}" onclick="copyText(event)">⧉</button>`;
      // Squad-level Logs button (opens the logs modal) — sits next to a squad's state.
      /**
       * Renders a squad-level "Logs" button that opens the squad logs modal.
       * @param {string} id
       * @returns {string}
       */
      const squadLogsBtn = (id) => `<button class="logs-btn" data-tip="View squad logs — events, task states, cell timings, and proof output." data-click="openSquadLogs" data-id="${esc(id)}">📄 Logs</button>`;
      // Squad-level Timeline button (RAL-155) — generates and opens the merged,
      // chronological uber-log-viewer for the whole squad.
      /**
       * Renders a squad-level "Timeline" button that generates and opens the
       * merged uber-log-viewer for the whole squad.
       * @param {string} id
       * @returns {string}
       */
      const squadTimelineBtn = (id) => `<button class="logs-btn" data-tip="Generate the merged, chronological timeline for this whole squad (RAL-155) — every state transition, Cartographer event, and terminal-log excerpt from every task/cell, in one time-ordered document.\nWho/when: use this for incident review or debugging instead of cross-referencing the Logs modal, per-cell terminal logs, and task/squad state by hand.\nRegenerates a temp file on the daemon on every click (not a persistent export)." data-click="openSquadTimeline" data-id="${esc(id)}">⏱ Timeline</button>`;
      // Per-cell failure-log button (opens the traceback popup) — next to a failed badge.
      /**
       * Renders a per-cell failure-log button that opens the traceback popup.
       * @param {string} err
       * @returns {string}
       */
      const failLogBtn = (err) => `<button class="logs-btn fail" data-tip="View the failure log — the full error traceback for this cell." data-full="${esc(err)}" onclick="event.stopPropagation();openErrPopup(event)">📄 Failure log</button>`;
      /**
       * Writes text to the clipboard, falling back to a hidden textarea +
       * `execCommand` in insecure contexts where `navigator.clipboard` is unavailable.
       * @param {string} text
       * @returns {Promise<void>}
       */
      async function writeClipboardText(text) {
        if (navigator.clipboard && window.isSecureContext) { await navigator.clipboard.writeText(text); return; }
        const ta = document.createElement("textarea");
        ta.value = text; ta.style.cssText = "position:fixed;opacity:0";
        document.body.appendChild(ta); ta.select(); document.execCommand("copy"); ta.remove();
      }
      /**
       * Copies a copy-button's `data-copy` payload to the clipboard and flashes a checkmark.
       * @param {MouseEvent} e
       * @returns {Promise<void>}
       */
      async function copyText(e) {
        e.stopPropagation();
        const btn = /** @type {HTMLElement} */ (e.currentTarget), text = btn.dataset.copy || "";
        try {
          await writeClipboardText(text);
          btn.textContent = "✓"; btn.classList.add("copied");
          setTimeout(() => { btn.textContent = "⧉"; btn.classList.remove("copied"); }, 1200);
        } catch (_) { btn.textContent = "✗"; setTimeout(() => { btn.textContent = "⧉"; }, 1200); }
      }
      /**
       * Copies a live-terminal peek box's current content to the clipboard and
       * flashes a checkmark. Unlike copyText/copyBtn (a static payload baked in
       * at render time), this reads peekContent[key] live at click time, since
       * pollOpenPeeks() patches the pane text on its own timer (RAL-167)
       * without re-rendering the header this button lives in — a baked-in
       * payload would go stale. `peekContent[key]` already holds the tape run
       * through the ANSI-strip/classify pipeline honoring this pane's "Show
       * Debug Messages" checkbox (RAL-232/RAL-397 Phase 2G-A), so copying it
       * copies exactly what's visible.
       * @param {MouseEvent} e
       * @param {string} key
       * @returns {Promise<void>}
       */
      async function copyPeekText(e, key) {
        e.stopPropagation();
        const btn = /** @type {HTMLElement} */ (e.currentTarget);
        // RAL-232/RAL-397: copy whatever is currently visible, same as this box's own text.
        const text = peekContent[key] || "";
        try {
          if (navigator.clipboard && window.isSecureContext) await navigator.clipboard.writeText(text);
          else { const ta = document.createElement("textarea"); ta.value = text; ta.style.cssText = "position:fixed;opacity:0"; document.body.appendChild(ta); ta.select(); document.execCommand("copy"); ta.remove(); }
          btn.textContent = "✓"; btn.classList.add("copied");
          setTimeout(() => { btn.textContent = "⧉"; btn.classList.remove("copied"); }, 1200);
        } catch (_) { btn.textContent = "✗"; setTimeout(() => { btn.textContent = "⧉"; }, 1200); }
      }

