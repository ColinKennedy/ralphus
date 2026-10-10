      // ---------- widget-based agent log (RAL-603, experimental) ----------
      // The runner flattens every tool call to a bracket-tagged text line
      // (`[tool.Bash.git] Bash(...)`, then `[result] ...` / `[error] ...`), and
      // that text tape is the only record there is, so the widgets are rebuilt
      // from it. Results carry no tool-use id: a result belongs to the most
      // recent call that has not had one yet. Args and results are already
      // truncated by the runner, and Codex does not print command output at all.
      // RALPHUS-AGENT-LOG:BEGIN
      /**
       * @typedef {object} AgentLogEntry
       * @property {number} index - Position of the call in the log; stable as the log grows.
       * @property {string} tool - The tool name (`Bash`, `exec`, `mcp`, ...).
       * @property {string} sub - The sub-command or sub-tool (`git`, `server.tool`), or "".
       * @property {string} tag - The full bracketed type, e.g. `tool.Bash.git`.
       * @property {string} input - The call's arguments as printed.
       * @property {string} output - The result text, or "" while none has arrived.
       * @property {"pending"|"ok"|"error"} status - `pending` until a result line arrives.
       */
      /** How many of the newest entries are drawn before "show earlier" is pressed. */
      const AGENT_LOG_PAGE = 150;
      /**
       * Parses a tag-flattened log into one entry per tool call.
       * @param {string} text - The scrubbed transcript text.
       * @returns {AgentLogEntry[]}
       */
      function parseAgentLog(text) {
        /** @type {AgentLogEntry[]} */
        const entries = [];
        /** @type {"input"|"output"|null} */
        let section = null;
        /** @type {AgentLogEntry|null} */
        let cur = null;
        /** @type {AgentLogEntry|null} */
        let awaiting = null;
        for (const line of text.split("\n")) {
          const m = line.trimStart().match(/^\[([^\]]+)\]\s?(.*)$/);
          if (m === null) {
            if (cur !== null && section !== null) cur[section] += (cur[section] === "" ? "" : "\n") + line;
            continue;
          }
          const tag = m[1];
          const rest = m[2];
          if (tag.startsWith("tool.")) {
            const segs = tag.split(".");
            let input = rest;
            const head = `${segs[1]}(`;
            if (input.startsWith(head) && input.endsWith(")")) input = input.slice(head.length, -1);
            cur = { index: entries.length, tool: segs[1], sub: segs.slice(2).join("."), tag, input, output: "", status: "pending" };
            entries.push(cur);
            awaiting = cur;
            section = "input";
          } else if ((tag === "result" || tag === "error") && awaiting !== null) {
            awaiting.status = tag === "error" ? "error" : "ok";
            awaiting.output = rest;
            cur = awaiting;
            awaiting = null;
            section = "output";
          } else {
            section = null;
          }
        }
        return entries;
      }
      /**
       * Whether an entry passes the filter. The query matches tool, sub-command,
       * input and output, case-insensitively.
       * @param {AgentLogEntry} e
       * @param {{tool: string, status: string, q: string}} f
       * @returns {boolean}
       */
      function agentLogEntryVisible(e, f) {
        if (f.tool !== "" && e.tool !== f.tool) return false;
        if (f.status !== "" && e.status !== f.status) return false;
        if (f.q === "") return true;
        const q = f.q.toLowerCase();
        return `${e.tool}\n${e.sub}\n${e.input}\n${e.output}`.toLowerCase().includes(q);
      }
      // RALPHUS-AGENT-LOG:END

      /** @type {{[key: string]: {tool: string, status: string, q: string}}} Per-run filter state; survives repaints. */
      const agentLogFilter = {};
      /** @type {{[key: string]: {[index: number]: boolean}}} Which entries the reader expanded, by call index. */
      const agentLogOpen = {};
      /** @type {{[key: string]: boolean}} Runs whose older entries were revealed. */
      const agentLogAll = {};
      /** @type {{[key: string]: number}} Scroll offset of each run's widget list. */
      const agentLogScroll = {};
      /** @type {{[key: string]: {src: string, entries: AgentLogEntry[]}}} Parse memo: the log is re-parsed only when its text changed. */
      const agentLogParsed = {};

      /**
       * Extra masking on top of the daemon's and `scrubSecrets`: bearer tokens,
       * well-known key prefixes and `name=value` / `"name": "value"` pairs whose
       * name looks like a credential. Over-matching is fine; under-matching leaks.
       * @param {string} text
       * @returns {string}
       */
      function redactAgentLog(text) {
        return scrubSecrets(text)
          .replace(/\b(Bearer|Basic)\s+[A-Za-z0-9._~+/=-]{8,}/gi, "$1 [REDACTED]")
          .replace(/\b(sk-[A-Za-z0-9_-]{16,}|gh[pousr]_[A-Za-z0-9]{20,}|glpat-[A-Za-z0-9_-]{16,}|AKIA[0-9A-Z]{16}|xox[baprs]-[A-Za-z0-9-]{10,})/g, "[REDACTED]")
          .replace(/(["']?[A-Za-z0-9_.-]*(?:token|secret|passw(?:or)?d|api[_-]?key|credential|authorization)[A-Za-z0-9_.-]*["']?\s*[:=]\s*)(?:"[^"\n]*"|'[^'\n]*'|[^\s,;)}\]]+)/gi, "$1[REDACTED]");
      }
      /**
       * The unfiltered, scrubbed transcript text for a run, from data the
       * Terminal view already loaded -- opening this tab fetches nothing extra.
       * @param {string} key - The peek key.
       * @returns {string|null} null while the transcript has not loaded.
       */
      function agentLogSource(key) {
        const w = peekTape[key];
        if (w) {
          return renderTapeLines(tapeCompleteLines(w, !!peekEnded[key]), false, false, parseLiveViewTypeFilter(""));
        }
        const shown = peekContent[key];
        return shown === undefined ? null : shown;
      }
      /**
       * The parsed entries for a run, reusing the last parse when the text is unchanged.
       * @param {string} key
       * @returns {AgentLogEntry[]|null}
       */
      function agentLogEntries(key) {
        const src = agentLogSource(key);
        if (src === null) return null;
        const memo = agentLogParsed[key];
        if (memo && memo.src === src) return memo.entries;
        const entries = parseAgentLog(redactAgentLog(src));
        agentLogParsed[key] = { src, entries };
        return entries;
      }
      /**
       * One widget: a collapsible card with the tool, sub-command, input and output.
       * @param {string} key
       * @param {AgentLogEntry} e
       * @returns {string}
       */
      function agentLogWidget(key, e) {
        const open = !!(agentLogOpen[key] && agentLogOpen[key][e.index]);
        const color = e.status === "error" ? "var(--turn-error)" : typeColor(e.tag, document.documentElement.dataset.theme === "light" ? "light" : "dark");
        const statusTip = e.status === "pending" ? "No result has been logged for this call yet." : (e.status === "error" ? "The tool reported an error." : "The tool returned a result.");
        const preview = e.input.split("\n")[0].slice(0, 120);
        return `<details class="alog-w ${e.status}" data-idx="${e.index}" data-sig="${e.status}|${e.input.length}|${e.output.length}" ${open ? "open" : ""}
            ontoggle="agentLogToggled(this,'${esc(key).replace(/'/g, "&#39;")}')">
          <summary data-tip="Click to expand or collapse this call's input and output.\nThe log is reconstructed from the terminal text, so long values may be truncated.">
            <span class="alog-tool" style="color:${color}">${esc(e.tool)}</span>${e.sub ? `<span class="alog-sub">${esc(e.sub)}</span>` : ""}
            <span class="alog-prev">${esc(preview)}</span>
            <span class="alog-st ${e.status}" data-tip="${esc(statusTip)}">${e.status === "pending" ? "…" : (e.status === "error" ? "error" : "ok")}</span>
          </summary>
          <div class="alog-lbl">Input</div><pre class="alog-pre">${esc(e.input) || "(none)"}</pre>
          <div class="alog-lbl">Output</div><pre class="alog-pre">${e.output === "" ? (e.status === "pending" ? "(waiting for result)" : "(not logged)") : esc(e.output)}</pre>
        </details>`;
      }
      /**
       * The widget list's inner HTML for the current filter.
       * @param {string} key
       * @param {AgentLogEntry[]} entries
       * @returns {string}
       */
      function agentLogListHtml(key, entries) {
        const f = agentLogFilter[key] || { tool: "", status: "", q: "" };
        const hits = entries.filter((e) => agentLogEntryVisible(e, f));
        if (entries.length === 0) return `<div class="empty">No tool calls in this run's log yet.</div>`;
        if (hits.length === 0) return `<div class="empty">No tool calls match the filter.</div>`;
        const cut = agentLogAll[key] ? 0 : Math.max(0, hits.length - AGENT_LOG_PAGE);
        const more = cut > 0
          ? `<button class="btn" style="padding:3px 8px;font-size:11px" data-click="agentLogShowEarlier" data-key="${esc(key)}"
              data-tip="Draw the ${cut} older matching calls above. Nothing is refetched.">Show ${cut} earlier</button>`
          : "";
        return more + hits.slice(cut).map((e) => agentLogWidget(key, e)).join("");
      }
      /**
       * The Live tab's Agent Log view (experimental).
       * @param {string} key - The peek key of the shown run.
       * @returns {string}
       */
      function liveAgentLogView(key) {
        const entries = agentLogEntries(key);
        if (entries === null) return `<div class="empty">Loading…</div>`;
        const f = agentLogFilter[key] || { tool: "", status: "", q: "" };
        const tools = [...new Set(entries.map((e) => e.tool))].sort();
        const ek = esc(key);
        const bar = `<div class="peek-ctl">
            <select class="alog-sel" onchange="setAgentLogFilter('${ek}','tool',this.value)" aria-label="Filter by tool"
              data-tip="Show only calls to one tool.\nChanging this re-filters what is already loaded.">
              <option value="">All tools</option>${tools.map((t) => `<option value="${esc(t)}" ${f.tool === t ? "selected" : ""}>${esc(t)}</option>`).join("")}</select>
            <select class="alog-sel" onchange="setAgentLogFilter('${ek}','status',this.value)" aria-label="Filter by status"
              data-tip="Show only calls that succeeded, failed, or are still waiting for a result.">
              ${[["", "Any status"], ["ok", "ok"], ["error", "error"], ["pending", "pending"]].map((o) => `<option value="${o[0]}" ${f.status === o[0] ? "selected" : ""}>${o[1]}</option>`).join("")}</select>
            <input class="typefilter" placeholder="Search tool, input, output" value="${esc(f.q)}"
              oninput="setAgentLogFilter('${ek}','q',this.value)" aria-label="Search the agent log"
              data-tip="Case-insensitive text search across every call's tool, sub-command, input and output.">
          </div>`;
        return `${bar}<div class="alog" id="alog-${peekCssKey(key)}" data-key="${ek}" onscroll="agentLogScrolled(this)"
            data-tip="Scroll through the agent's tool calls.\nExpansion and scroll position survive live updates.">${agentLogListHtml(key, entries)}</div>
          <div class="hint">Experimental — rebuilt from the terminal text, so arguments and results are truncated as the
          runner printed them, results are paired to calls by order, and Codex command output is not logged. Secrets are masked.</div>`;
      }
      /**
       * Remembers whether a widget is expanded.
       * @param {HTMLDetailsElement} el
       * @param {string} key
       * @returns {void}
       */
      function agentLogToggled(el, key) {
        const idx = Number(el.dataset.idx);
        const open = agentLogOpen[key] || (agentLogOpen[key] = {});
        if (el.open) open[idx] = true; else delete open[idx];
      }
      /**
       * Remembers the list's scroll offset.
       * @param {HTMLElement} el
       * @returns {void}
       */
      function agentLogScrolled(el) {
        agentLogScroll[el.dataset.key || ""] = el.scrollTop;
      }
      /**
       * Re-applies remembered scroll offsets after the inspector repainted.
       * @returns {void}
       */
      function restoreAgentLogScroll() {
        document.querySelectorAll("#review-inspector .alog[data-key]").forEach((el0) => {
          const el = /** @type {HTMLElement} */ (el0);
          const top = agentLogScroll[el.dataset.key || ""];
          if (top) el.scrollTop = top;
        });
      }
      /**
       * Applies one filter change and redraws just the list.
       * @param {string} key
       * @param {"tool"|"status"|"q"} field
       * @param {string} value
       * @returns {void}
       */
      function setAgentLogFilter(key, field, value) {
        const f = agentLogFilter[key] || (agentLogFilter[key] = { tool: "", status: "", q: "" });
        f[field] = value;
        refreshAgentLog(key, true);
      }
      /**
       * Reveals the entries hidden behind "show earlier".
       * @param {string} key
       * @returns {void}
       */
      function agentLogShowEarlier(key) {
        agentLogAll[key] = true;
        refreshAgentLog(key, true);
      }
      /**
       * Brings an open Agent Log list up to date after the transcript changed.
       * Without `full`, only widgets that are new or whose status/output moved
       * are touched, so expanded cards and the scroll position are left alone.
       * @param {string} key
       * @param {boolean} [full] - Redraw the whole list (the filter changed).
       * @returns {void}
       */
      function refreshAgentLog(key, full) {
        const host = document.getElementById(`alog-${peekCssKey(key)}`);
        if (!host) return;
        const entries = agentLogEntries(key);
        if (entries === null) return;
        const f = agentLogFilter[key] || { tool: "", status: "", q: "" };
        const redraw = full || f.q !== "" || f.tool !== "" || f.status !== "" || (!agentLogAll[key] && entries.length > AGENT_LOG_PAGE);
        if (redraw) {
          const top = host.scrollTop;
          host.innerHTML = agentLogListHtml(key, entries);
          host.scrollTop = top;
          return;
        }
        const nodes = new Map();
        host.querySelectorAll("details.alog-w").forEach((n) => nodes.set(Number(/** @type {HTMLElement} */ (n).dataset.idx), n));
        if (nodes.size === 0) { host.innerHTML = agentLogListHtml(key, entries); return; }
        const stick = host.scrollTop + host.clientHeight >= host.scrollHeight - 24;
        for (const e of entries) {
          const html = agentLogWidget(key, e);
          const n = nodes.get(e.index);
          if (!n) {
            host.insertAdjacentHTML("beforeend", html);
          } else if (/** @type {HTMLElement} */ (n).dataset.sig !== `${e.status}|${e.input.length}|${e.output.length}`) {
            n.outerHTML = html;
          }
        }
        if (stick) host.scrollTop = host.scrollHeight;
      }
