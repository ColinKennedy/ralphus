      // ---------- widget-based agent log (RAL-603, experimental) ----------
      // Each backend emits a structured `RALPHUS_TOOL: {json}` record per tool
      // call and per result (runner/src/tool_record.rs): exact tool name, the
      // provider's tool-use id, the full input and the full output. Those
      // records are the source. A transcript written before the records existed
      // has none, so it falls back to rebuilding the widgets from the
      // bracket-tagged text tape (`[tool.Bash.git] Bash(...)`, then `[result]
      // ...` / `[error] ...`), which is lossy: args and results are truncated,
      // results pair to calls by order, and Codex output is absent.
      // RALPHUS-AGENT-LOG:BEGIN
      /**
       * @typedef {object} AgentLogEntry
       * @property {number} index - Position of the call in the log; stable as the log grows.
       * @property {string} id - The provider's tool-use id, or "" for a text-tape entry.
       * @property {string} tool - The tool name (`Bash`, `exec`, `mcp`, ...).
       * @property {string} sub - The sub-command or sub-tool (`git`, `server.tool`), or "".
       * @property {string} tag - The full bracketed type, e.g. `tool.Bash.git`.
       * @property {string} input - The call's arguments as printed.
       * @property {AgentLogArg[]} args - `input` split into named arguments, sorted by name; empty when it is not `name=value` shaped.
       * @property {string} output - The result text, or "" while none has arrived.
       * @property {"pending"|"ok"|"error"} status - `pending` until a result line arrives.
       */
      /**
       * @typedef {object} AgentLogArg
       * @property {string} key - The argument name.
       * @property {string} value - The argument value, quotes removed.
       */
      /** Tools whose `command` argument is a shell command line. */
      const AGENT_LOG_SHELL_TOOLS = ["Bash", "bash", "PowerShell", "powershell", "exec"];
      /** Argument names tried in order for a non-shell call's collapsed row; the first argument is the last resort. */
      const AGENT_LOG_PREVIEW_KEYS = ["file_path", "path", "pattern", "command", "url", "query"];
      /** How many of the newest entries are drawn before "show earlier" is pressed. */
      const AGENT_LOG_PAGE = 150;
      /** Marker of a structured tool record; matches runner/src/tool_record.rs::TOOL_MARKER. */
      const AGENT_LOG_RECORD_PREFIX = "RALPHUS_TOOL: ";
      /**
       * Splits the printed arguments (`key="v", key2=v2`, values possibly
       * quoted and spanning lines) into named arguments sorted by name. A quote
       * inside a quoted value only closes it when a `, key=` or the end follows.
       * Input that is not `name=value` shaped yields no arguments.
       * @param {string} input
       * @returns {AgentLogArg[]}
       */
      function splitAgentLogArgs(input) {
        const trimmed = input.trim();
        if (trimmed.startsWith("{")) {
          try {
            const obj = JSON.parse(trimmed);
            if (obj !== null && typeof obj === "object" && !Array.isArray(obj)) {
              return Object.keys(obj).sort().map((key) => ({ key, value: typeof obj[key] === "string" ? obj[key] : JSON.stringify(obj[key]) }));
            }
          } catch (_) { /* not JSON: fall through to name=value */ }
        }
        const next = /^\s*(?:,\s*[A-Za-z_][\w.-]*=|$)/;
        const keyRe = /^\s*,?\s*([A-Za-z_][\w.-]*)=/;
        /** @type {AgentLogArg[]} */
        const args = [];
        let pos = 0;
        while (pos < input.length) {
          const km = keyRe.exec(input.slice(pos));
          if (km === null) return [];
          pos += km[0].length;
          const q = input[pos];
          let value = "";
          if (q === '"' || q === "'") {
            let i = pos + 1;
            let closed = false;
            for (; i < input.length; i++) {
              if (input[i] === "\\" && i + 1 < input.length) { value += input[i + 1] === q ? q : input[i] + input[i + 1]; i++; continue; }
              if (input[i] === q && next.test(input.slice(i + 1))) { closed = true; break; }
              value += input[i];
            }
            pos = closed ? i + 1 : input.length;
          } else {
            const m = /,\s*[A-Za-z_][\w.-]*=/.exec(input.slice(pos));
            const end = m === null ? input.length : pos + m.index;
            value = input.slice(pos, end);
            pos = end;
          }
          args.push({ key: km[1], value });
        }
        return args.sort((x, y) => (x.key < y.key ? -1 : x.key > y.key ? 1 : 0));
      }
      /**
       * The one-line text shown on a collapsed row. A shell command's leading
       * `cd <dir> &&` / `cd <dir>;` prefixes (and PowerShell's `Set-Location`
       * / `sl`, with an optional `-Path` / `-LiteralPath`, case-insensitive)
       * are skipped so the command that matters shows; a call that is only a
       * directory change keeps showing it. The `sl` alias is only stripped
       * when a path and a separator follow it.
       * @param {AgentLogEntry} e
       * @returns {string}
       */
      function agentLogPreview(e) {
        let text = e.input;
        if (AGENT_LOG_SHELL_TOOLS.includes(e.tool)) {
          const cmd = e.args.find((a) => a.key === "command" || a.key === "cmd");
          if (cmd !== undefined) text = cmd.value;
          const cd =
            /^\s*(?:cd|set-location|sl)\s+(?:-(?:literalpath|path)\s+)?(?:"[^"]*"|'[^']*'|[^\s&;|]+)\s*(?:&&|;|\n)\s*/i;
          let rest = text;
          while (cd.test(rest)) rest = rest.replace(cd, "");
          if (rest.trim() !== "") text = rest;
        }
        else if (e.args.length > 0) {
          const picked = AGENT_LOG_PREVIEW_KEYS.map((k) => e.args.find((a) => a.key === k)).find((a) => a !== undefined && a.value.trim() !== "");
          text = (picked !== undefined ? picked : e.args[0]).value;
        }
        return text.trimStart().split("\n")[0].slice(0, 120);
      }
      /**
       * Masks the values of an entry's named arguments. A credential-named
       * argument is masked whole; every other value goes through `redact`.
       * @param {AgentLogEntry} e
       * @param {(s: string) => string} redact
       * @returns {void}
       */
      function agentLogRedactArgs(e, redact) {
        const secretKey = /token|secret|passw(?:or)?d|api[_-]?key|credential|authorization/i;
        for (const a of e.args) a.value = secretKey.test(a.key) ? "[REDACTED]" : redact(a.value);
      }
      /**
       * Pretty-prints a JSON object/array input; anything else is returned as is.
       * @param {string} text
       * @returns {string}
       */
      function agentLogPrettyInput(text) {
        if (!text.startsWith("{") && !text.startsWith("[")) return text;
        try {
          return JSON.stringify(JSON.parse(text), null, 2);
        } catch (_e) {
          return text;
        }
      }
      /**
       * Folds structured tool records into one entry per tool-use id. A call
       * half opens the entry, a result half (same id) fills its output; a
       * record carrying both fills both. Unparseable lines are skipped.
       * @param {string[]} lines - Logical log lines, any of which may be a record.
       * @returns {AgentLogEntry[]}
       */
      function parseAgentLogRecords(lines) {
        /** @type {AgentLogEntry[]} */
        const entries = [];
        /** @type {Map<string, AgentLogEntry>} */
        const byId = new Map();
        for (const line of lines) {
          const t = line.trimStart();
          if (!t.startsWith(AGENT_LOG_RECORD_PREFIX)) continue;
          /** @type {{id?: string, tool?: string, sub?: string, tag?: string, input?: string, output?: string, is_error?: boolean}} */
          let rec;
          try {
            rec = JSON.parse(t.slice(AGENT_LOG_RECORD_PREFIX.length));
          } catch (_e) {
            continue;
          }
          if (rec === null || typeof rec !== "object" || Array.isArray(rec)) continue;
          const id = typeof rec.id === "string" ? rec.id : "";
          let cur = id === "" ? undefined : byId.get(id);
          if (cur === undefined) {
            const tool = typeof rec.tool === "string" ? rec.tool : "";
            if (tool === "" && typeof rec.output === "string") {
              cur = [...entries].reverse().find((e) => e.id === "" && e.status === "pending");
            }
            if (cur === undefined) {
              cur = { index: entries.length, id, tool: tool || "tool", sub: "", tag: "", input: "", args: [], output: "", status: "pending" };
              entries.push(cur);
              if (id !== "") byId.set(id, cur);
            }
          }
          if (typeof rec.tool === "string" && rec.tool !== "") {
            cur.tool = rec.tool;
            cur.sub = typeof rec.sub === "string" ? rec.sub : "";
            cur.tag = typeof rec.tag === "string" ? rec.tag : `tool.${rec.tool}`;
          }
          if (typeof rec.input === "string") {
            cur.args = splitAgentLogArgs(rec.input);
            cur.input = agentLogPrettyInput(rec.input);
          }
          if (typeof rec.output === "string") {
            cur.output = rec.output;
            cur.status = rec.is_error ? "error" : "ok";
          }
        }
        return entries;
      }
      /**
       * Builds one entry per tool call: from the structured records when the
       * log has any, otherwise from the tag-flattened text.
       * @param {string} text - The scrubbed transcript text.
       * @returns {AgentLogEntry[]}
       */
      function parseAgentLog(text) {
        const lines = text.split("\n");
        if (lines.some((l) => l.trimStart().startsWith(AGENT_LOG_RECORD_PREFIX))) return parseAgentLogRecords(lines);
        return parseAgentLogText(lines);
      }
      /**
       * Parses a tag-flattened log into one entry per tool call.
       * @param {string[]} lines - The scrubbed transcript lines.
       * @returns {AgentLogEntry[]}
       */
      function parseAgentLogText(lines) {
        /** @type {AgentLogEntry[]} */
        const entries = [];
        /** @type {"input"|"output"|null} */
        let section = null;
        /** @type {AgentLogEntry|null} */
        let cur = null;
        /** @type {AgentLogEntry|null} */
        let awaiting = null;
        for (const line of lines) {
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
            cur = { index: entries.length, id: "", tool: segs[1], sub: segs.slice(2).join("."), tag, input, args: [], output: "", status: "pending" };
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
        for (const e of entries) e.args = splitAgentLogArgs(e.input);
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

      // Optional tree-sitter parse of a shell call (RAL-644). The daemon splits
      // the command into rows and tags byte ranges; this region only draws that
      // result. Display-only: the raw text is always one click away.
      // RALPHUS-SHELL-SYNTAX:BEGIN
      /**
       * @typedef {object} ShellSyntaxCommand
       * @property {string} text - The command's text.
       * @property {number} start - Start byte (UTF-8) of the command in the input.
       * @property {number} end - End byte (UTF-8, exclusive).
       * @property {string} sep - Operator joining it to the previous command (`;`, `&&`, `||`, `&`), or "".
       */
      /**
       * @typedef {object} ShellSyntaxSpan
       * @property {string} kind - `string`, `flag`, `operator`, `command_name`, `variable`, `comment` or `heredoc`.
       * @property {number} start - Start byte (UTF-8).
       * @property {number} end - End byte (UTF-8, exclusive).
       */
      /**
       * @typedef {object} ShellSyntaxParse
       * @property {string} status - `ok`, `unparsed`, `unsupported`, `too_long`, `timeout` or `unavailable`.
       * @property {ShellSyntaxCommand[]} commands - Top-level commands in order; empty unless `ok`.
       * @property {ShellSyntaxSpan[]} spans - Non-overlapping highlighted ranges; empty unless `ok`.
       */
      /** @type {{[kind: string]: string}} Span kind to CSS class suffix. */
      const SHELL_SYNTAX_CLASS = { string: "string", flag: "flag", operator: "op", command_name: "cmd", variable: "var", comment: "comment" };
      /**
       * Whether a tool name is a shell tool whose `command` can be parsed.
       * @param {string} tool
       * @returns {boolean}
       */
      function shellSyntaxTool(tool) {
        return ["bash", "powershell", "exec"].includes(tool.toLowerCase());
      }
      /**
       * The shell command line among a call's arguments.
       * @param {{key: string, value: string}[]} args
       * @returns {string|null} null when the call has no command argument.
       */
      function shellCommandText(args) {
        const a = args.find((x) => x.key === "command" || x.key === "cmd");
        return a === undefined ? null : a.value;
      }
      /**
       * Whether a cached parse result can be drawn (a clean parse with rows).
       * @param {ShellSyntaxParse} p
       * @returns {boolean}
       */
      function shellSyntaxUsable(p) {
        return p.status === "ok" && p.commands.length > 0;
      }
      /**
       * How many rows follow the one a collapsed call shows. Leading
       * `cd <dir>` rows are skipped, matching `agentLogPreview`.
       * @param {ShellSyntaxParse} p
       * @returns {number}
       */
      function shellSyntaxMoreCount(p) {
        let i = 0;
        while (i < p.commands.length - 1 && /^cd\s/.test(p.commands[i].text)) i++;
        return Math.max(0, p.commands.length - 1 - i);
      }
      /**
       * Draws a parsed command as one row per command, spans colored, each
       * heredoc body a collapsed block under its row.
       * @param {string} text - The exact command text that was parsed.
       * @param {ShellSyntaxParse} p
       * @param {(s: string) => string} escFn - HTML escaper.
       * @returns {string}
       */
      function shellSyntaxRowsHtml(text, p, escFn) {
        const bytes = new TextEncoder().encode(text);
        const dec = new TextDecoder();
        /**
         * @param {number} a
         * @param {number} b
         * @returns {string}
         */
        const slice = (a, b) => dec.decode(bytes.subarray(a, b));
        return p.commands.map((c) => {
          let pos = c.start;
          let html = "";
          /** @type {string[]} */
          const docs = [];
          for (const sp of p.spans) {
            if (sp.start < pos || sp.end > c.end) continue;
            html += escFn(slice(pos, sp.start));
            const body = slice(sp.start, sp.end);
            if (sp.kind === "heredoc") {
              docs.push(body);
            } else {
              const cls = SHELL_SYNTAX_CLASS[sp.kind];
              html += cls ? `<span class="alog-sh-${cls}">${escFn(body)}</span>` : escFn(body);
            }
            pos = sp.end;
          }
          html += escFn(slice(pos, c.end));
          const sep = c.sep === "" ? "" : `<span class="alog-sh-sep" data-tip="Joins this command to the previous one.">${escFn(c.sep)}</span>`;
          const blocks = docs.map((d) => `<details class="alog-sh-heredoc"><summary data-tip="The heredoc / here-string body, kept collapsed and unhighlighted.">heredoc · ${d.split("\n").length} lines</summary><pre>${escFn(d)}</pre></details>`).join("");
          return `<div class="alog-sh-row">${sep}${html}${blocks}</div>`;
        }).join("");
      }
      // RALPHUS-SHELL-SYNTAX:END

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
      /** @type {{[key: string]: {[index: number]: ShellSyntaxParse|"loading"|"failed"}}} Shell-syntax parse per expanded call; absent until first expanded. */
      const agentLogShellSyntax = {};
      /** @type {{[key: string]: {[index: number]: boolean}}} Calls whose raw text the reader chose over the parsed view. */
      const agentLogShellRaw = {};
      /** Set once the daemon says shell-syntax is not built, so the board stops asking for the session. */
      let agentLogShellSyntaxOff = false;

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
          // Wrapped rows are rejoined so a long record stays one line.
          /** @type {string[]} */
          const logical = [];
          let carry = "";
          for (const raw of tapeCompleteLines(w, !!peekEnded[key])) {
            const clean = renderPaneLine(raw).replace(/\r$/, "");
            if (tapeLineWrapped(raw)) {
              carry += clean;
              continue;
            }
            logical.push(carry + clean);
            carry = "";
          }
          if (carry !== "") logical.push(carry);
          return logical.join("\n");
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
        const entries = parseAgentLog(src);
        for (const e of entries) {
          e.input = redactAgentLog(e.input);
          e.output = redactAgentLog(e.output);
          agentLogRedactArgs(e, redactAgentLog);
        }
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
        const preview = agentLogPreview(e);
        const parse = agentLogShellParse(key, e);
        const raw = !!(agentLogShellRaw[key] && agentLogShellRaw[key][e.index]);
        const more = parse !== null ? shellSyntaxMoreCount(parse) : 0;
        const moreHtml = more > 0
          ? `<span class="alog-sh-more" data-tip="This call runs ${more} more command${more === 1 ? "" : "s"} after the one shown.\nExpand it to see every row.">+${more} more</span>`
          : "";
        const cmdText = shellCommandText(e.args);
        const inputHtml = e.args.length === 0
          ? `<pre class="alog-pre">${esc(e.input) || "(none)"}</pre>`
          : e.args.map((a) => {
            const body = parse !== null && !raw && a.value === cmdText && (a.key === "command" || a.key === "cmd")
              ? `<div class="alog-sh" data-tip="Syntax-highlighted view of the command. Display-only and optional: it never changes what ran, and the raw text is one click away.">${shellSyntaxRowsHtml(a.value, parse, esc)}</div>`
              : `<pre class="alog-pre">${esc(a.value)}</pre>`;
            return `<div class="alog-kv"><span class="alog-k" data-tip="The &quot;${esc(a.key)}&quot; argument of this call.">${esc(a.key)}</span>${body}</div>`;
          }).join("");
        const toggle = parse !== null
          ? `<button class="btn alog-sh-raw" data-click="agentLogToggleRaw" data-key="${esc(key)}" data-idx="${e.index}"
              data-tip="${raw ? "Show the highlighted, row-per-command view." : "Show the command exactly as logged."}\nThe highlighted view is display-only and optional.">${raw ? "parsed" : "raw"}</button>`
          : "";
        return `<details class="alog-w ${e.status}" data-idx="${e.index}" data-sig="${agentLogSig(key, e)}" ${open ? "open" : ""}
            ontoggle="agentLogToggled(this,'${esc(key).replace(/'/g, "&#39;")}')">
          <summary data-tip="Click to expand or collapse this call's input and output.\nValues come from the runner's structured tool records.">
            <span class="alog-tool" style="color:${color}">${esc(e.tool)}</span>${e.sub ? `<span class="alog-sub">${esc(e.sub)}</span>` : ""}
            <span class="alog-prev">${esc(preview)}</span>${moreHtml}
            <span class="alog-st ${e.status}" data-tip="${esc(statusTip)}">${e.status === "pending" ? "…" : (e.status === "error" ? "error" : "ok")}</span>
          </summary>
          <div class="alog-lbl">Input</div>${toggle}${inputHtml}
          <div class="alog-lbl">Output</div><pre class="alog-pre">${e.output === "" ? (e.status === "pending" ? "(waiting for result)" : "(not logged)") : esc(e.output)}</pre>
        </details>`;
      }
      /**
       * The cached usable parse for a call, or null (never fetches).
       * @param {string} key
       * @param {AgentLogEntry} e
       * @returns {ShellSyntaxParse|null}
       */
      function agentLogShellParse(key, e) {
        const p = agentLogShellSyntax[key] && agentLogShellSyntax[key][e.index];
        return p !== undefined && typeof p === "object" && shellSyntaxUsable(p) ? p : null;
      }
      /**
       * What `refreshAgentLog` compares to decide a widget needs redrawing:
       * the call's status and sizes plus its parse state and raw/parsed choice.
       * @param {string} key
       * @param {AgentLogEntry} e
       * @returns {string}
       */
      function agentLogSig(key, e) {
        const p = agentLogShellSyntax[key] && agentLogShellSyntax[key][e.index];
        const ps = p === undefined ? "-" : (typeof p === "string" ? p : p.status);
        const raw = agentLogShellRaw[key] && agentLogShellRaw[key][e.index] ? "r" : "p";
        return `${e.status}|${e.input.length}|${e.output.length}|${ps}${raw}`;
      }
      /**
       * Requests the shell-syntax parse of one call, once, when it is first
       * expanded. A collapsed call never asks; a cached or in-flight call is
       * not asked again; after an `unavailable` answer nothing asks again.
       * @param {string} key
       * @param {AgentLogEntry} e
       * @returns {Promise<void>}
       */
      async function ensureAgentLogShellSyntax(key, e) {
        if (agentLogShellSyntaxOff || !shellSyntaxTool(e.tool)) return;
        const command = shellCommandText(e.args);
        if (command === null || command.trim() === "") return;
        const cache = agentLogShellSyntax[key] || (agentLogShellSyntax[key] = {});
        if (cache[e.index] !== undefined) return;
        cache[e.index] = "loading";
        try {
          const resp = await fetch("/api/shell-syntax/parse", { method: "POST", headers: traceHeaders(), body: JSON.stringify({ tool: e.tool, command }) });
          const data = resp.ok ? /** @type {ShellSyntaxParse|null} */ (await resp.json()) : null;
          if (data === null || typeof data.status !== "string") {
            cache[e.index] = "failed";
          } else {
            if (data.status === "unavailable") agentLogShellSyntaxOff = true;
            cache[e.index] = data;
          }
        } catch (_) {
          cache[e.index] = "failed";
        }
        refreshAgentLog(key, false);
      }
      /**
       * Flips one call between the parsed view and the raw text.
       * @param {string} key
       * @param {number} index
       * @returns {void}
       */
      function agentLogToggleRaw(key, index) {
        const m = agentLogShellRaw[key] || (agentLogShellRaw[key] = {});
        if (m[index]) delete m[index]; else m[index] = true;
        refreshAgentLog(key, false);
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
        if (entries.length === 0) return `<div class="alog-empty">No tool calls in this run's log yet.</div>`;
        if (hits.length === 0) return `<div class="alog-empty">No tool calls match the filter.</div>`;
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
          <div class="hint">Experimental — read from the runner's structured tool records (exact ids, full inputs and outputs).
          A transcript from before those records existed is rebuilt from the terminal text instead, with truncated values.
          Secrets are masked.</div>`;
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
        if (el.open) {
          const memo = agentLogParsed[key];
          const entry = memo === undefined ? undefined : memo.entries.find((x) => x.index === idx);
          if (entry !== undefined) void ensureAgentLogShellSyntax(key, entry);
        }
      }
      /**
       * Remembers the list's scroll offset.
       * @param {HTMLElement} el
       * @returns {void}
       */
      function agentLogScrolled(el) {
        agentLogScroll[el.dataset.key || ""] = el.scrollTop;
      }
      /** @type {Set<string>} Agent Log keys whose refresh was skipped because the user was selecting text. */
      const agentLogDeferred = new Set();
      /**
       * Records the live scroll offset of every mounted Agent Log list, so a
       * repaint of an enclosing pane can put them back.
       * @returns {void}
       */
      function snapshotAgentLogScroll() {
        document.querySelectorAll(".alog[data-key]").forEach((el0) => {
          agentLogScrolled(/** @type {HTMLElement} */ (el0));
        });
      }
      /**
       * Runs the Agent Log refreshes that were deferred while a selection was
       * active, once that selection is gone.
       * @returns {void}
       */
      function flushDeferredAgentLogs() {
        if (agentLogDeferred.size === 0) return;
        for (const key of [...agentLogDeferred]) refreshAgentLog(key);
      }
      document.addEventListener("selectionchange", flushDeferredAgentLogs);
      /**
       * Re-applies remembered scroll offsets after a live-view widget repainted.
       * @returns {void}
       */
      function restoreAgentLogScroll() {
        document.querySelectorAll(".alog[data-key]").forEach((el0) => {
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
        // A repaint would destroy the user's drag-selection; defer it until the selection clears.
        if (selectionWithin(host)) { agentLogDeferred.add(key); return; }
        agentLogDeferred.delete(key);
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
          } else if (/** @type {HTMLElement} */ (n).dataset.sig !== agentLogSig(key, e)) {
            n.outerHTML = html;
          }
        }
        if (stick) host.scrollTop = host.scrollHeight;
      }
