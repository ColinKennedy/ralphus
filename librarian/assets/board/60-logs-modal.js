      // ---------- logs modal ----------
      /** @type {string|null} */
      let logSquadId = null;
      let logsTab = "cells";
      // Shared with the "copy as Markdown" header-tooltip footnotes below —
      // kept as single constants so the HTML tooltip and the Markdown export
      // can't drift from each other.
      const TASK_REASON_TOOLTIP = "A task-level failure reason (RAL-291) — set only when the task itself failed for a reason with no underlying cell/proof error to point to, e.g. the RAL-156 no-commits-since-baseline guard.\nEmpty when the task instead failed because one of its own cells or proof steps failed — that failure is already visible in the cells/proofs tabs.";
      const CELL_CUM_TOOLTIP = "Sum of input/output tokens and $ cost across every completed attempt of this cell (including restarts), read from the Cartographer event log.\nMay undercount if older attempts have aged out of Cartographer retention (see [cartographer] in .ralphus.toml).";
      // Detects the one specific task.error text `daemon/src/worktrees.rs`'s
      // default_branch() produces when a git project's `origin` remote has
      // no refs/remotes/origin/HEAD symref -- must stay in sync with that
      // function's error string (the exact command, quoted the same way).
      const DEFAULT_BRANCH_AUTOFIX_MARKER = "git remote set-head origin --auto";
      /**
       * Renders an inline "Autofix" button beside a task's failure reason
       * when it's the specific `?upstream=<<default>>` failure a missing
       * `origin/HEAD` symref causes (see DEFAULT_BRANCH_AUTOFIX_MARKER) --
       * empty string for every other reason. The button calls
       * `POST /api/projects/{project}/default-branch/autofix`, which runs
       * `git remote set-head origin --auto` in the project's path, then
       * offers to restart the task.
       * @param {string|null|undefined} error
       * @param {string} project
       * @param {string} squadId
       * @param {number} taskIdx
       * @returns {string}
       */
      function taskErrorAutofixBtn(error, project, squadId, taskIdx) {
        if (!error || !error.includes(DEFAULT_BRANCH_AUTOFIX_MARKER)) return "";
        return `<button class="btn" style="padding:1px 6px;font-size:11px;margin-left:6px" data-click="autofixDefaultBranch" data-project="${esc(project)}" data-squad-id="${esc(squadId)}" data-task-idx="${taskIdx}" data-tip="Runs \`git remote set-head origin --auto\` for project \"${esc(project)}\" -- points its local default-branch pointer at whatever the \"origin\" remote reports as its own default branch, then offers to restart this task.\nWho/when: this task failed because its project has no refs/remotes/origin/HEAD symref set locally (common for a repo not created via a plain \`git clone\`), which \"?upstream=&lt;&lt;default&gt;&gt;\" needs to resolve.\nAlways targets the remote named \"origin\" specifically -- never a fallback remote.">⚡ Autofix</button>`;
      }
      /**
       * Runs the origin/HEAD autofix for `project` (see taskErrorAutofixBtn),
       * then offers to restart the task whose failure reason it was attached
       * to.
       * @param {string} project
       * @param {string} squadId
       * @param {number} taskIdx
       * @returns {Promise<void>}
       */
      async function autofixDefaultBranch(project, squadId, taskIdx) {
        if (!confirm(`Run \`git remote set-head origin --auto\` for project "${project}"?\nThis points its local default-branch pointer (origin/HEAD) at whatever branch the "origin" remote reports as its own default.`)) return;
        let resp;
        try {
          resp = await post(`/api/projects/${encodeURIComponent(project)}/default-branch/autofix`);
        } catch (e) {
          alert("Autofix failed: daemon unreachable");
          return;
        }
        if (!resp.ok) {
          let msg = `autofix failed (${resp.status})`;
          try { const body = await resp.json(); if (body && body.error && body.error.message) msg = body.error.message; } catch (_) {}
          alert(`Autofix failed: ${msg}`);
          return;
        }
        const body = await resp.json();
        alert(`Default branch set to "${body.branch}". You can now restart this task.`);
        await restartTask(squadId, taskIdx);
      }
      /**
       * Opens the Logs modal for a squad (defaults to the selected squad).
       * @param {string|null} [id]
       * @returns {void}
       */
      function openLogs(id) { const r = findSquad(id || selectedSquadId); if (!r) return; logSquadId = r.id; renderLogs(); }
      /**
       * Opens the Logs modal for a squad from a squad-row button click.
       * @param {MouseEvent} e
       * @param {string} id
       * @returns {void}
       */
      function openSquadLogs(e, id) { e.stopPropagation(); openLogs(id); }
      /**
       * Renders the Logs modal's active tab for `logSquadId`.
       * @returns {void}
       */
      function renderLogs() {
        const rid = logSquadId; if (!rid) return;
        const squad = findSquad(rid); if (!squad) return;
        const tabs = ["events", "tasks", "cells", "proofs"];
        if (logsTab === "events" && cartoModalCache[`squad:${rid}`] === undefined) {
          loadCartoModalRows(`squad:${rid}`, { squad_id: rid }).then(() => { if (logSquadId && logsTab === "events") renderLogs(); });
        }
        // RAL-160: cumulative token/cost totals across every restart of each
        // cell in this squad — sourced from the "cell completed"
        // Cartographer events already logged on every attempt (record_cell_result
        // overwrites the cell row itself, so it alone can't show restart history).
        if (logsTab === "cells" && cartoModalCache[`cellstotals:${rid}`] === undefined) {
          loadCartoModalRows(`cellstotals:${rid}`, { squad_id: rid, q: "cell completed" }).then(() => { if (logSquadId && logsTab === "cells") renderLogs(); });
        }
        byId("modal-root").innerHTML = `
          <div class="modal-bg" onclick="if(event.target===this)closeModal()"><div class="modal" style="width:880px;max-width:94vw">
            <h2>Logs — ${esc(squad.label || squad.id)}</h2>
            <div class="row" style="margin-bottom:10px;justify-content:space-between;align-items:center">
              <div class="row">${tabs.map((t) => `<span class="chip ${t === logsTab ? "active" : ""}" data-click="setLogsTab" data-tab="${esc(t)}" data-tip="${{events:"The structured event log (RAL-98), filtered to this squad — every state change, scheduler decision, and LLM call.",tasks:"Task-level summary with cell count.",cells:"Per-cell state, token usage, and cost breakdown — plus cumulative totals across restarts.",proofs:"Proof step status and raw output."}[t]||t}">${t}</span>`).join("")}</div>
              ${logsCopyBtnHtml(logsTab)}
            </div>
            <div style="max-height:60vh;overflow:auto">${logsBody(squad)}</div>
            <div class="btn-row">${logsTab === "events" ? `<span style="flex:1;color:var(--muted);font-size:12px;align-self:center" data-tip="See the Logs tab for filtering across every squad/cell/guardian and full pagination.">${(cartoModalCache[`squad:${logSquadId}`] || {}).total || 0} total — open the Logs tab for more filters</span>` : ""}<button class="btn" onclick="closeModal()">Close</button></div>
          </div></div>`;
      }
      // RAL-89: format a feedback-chat message timestamp in the viewer's local
      // time. Short form ("Jul 10, 02:34 PM") for the inline label; the full
      // date+time goes in the tooltip. Returns "" for optimistic (unsent)
      // messages that carry at_ms=0 until the server assigns the real time.
      /**
       * Formats a feedback-chat timestamp in short local form ("Jul 10, 02:34 PM"); "" for unsent (ms=0).
       * @param {number} ms
       * @returns {string}
       */
      function fmtMsgTime(ms) {
        if (!ms || ms <= 0) return "";
        return new Date(ms).toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
      }
      /**
       * Formats a feedback-chat timestamp in full local form for a tooltip.
       * @param {number} ms
       * @returns {string}
       */
      function fmtMsgTimeFull(ms) {
        if (!ms || ms <= 0) return "";
        return new Date(ms).toLocaleString();
      }
      /**
       * Renders one `<tr>` of already-HTML-formatted cell contents.
       * @param {(string|number)[]} cells
       * @returns {string}
       */
      function tblRow(cells) { return "<tr>" + cells.map((c) => `<td style="padding:5px 10px;border-bottom:1px solid var(--border)">${c}</td>`).join("") + "</tr>"; }
      /**
       * Sums each cell's "cell completed" Cartographer events (RAL-160)
       * into a per-cell totals map, keyed by cell id.
       * @param {string} squadId
       * @returns {{[cellId: string]: {tokensIn: number, tokensOut: number, costUsd: number, attempts: number}}}
       */
      function cumulativeCellTotals(squadId) {
        /** @type {{[cellId: string]: {tokensIn: number, tokensOut: number, costUsd: number, attempts: number}}} */
        const totals = {};
        const cache = cartoModalCache[`cellstotals:${squadId}`];
        if (!cache) return totals;
        cache.rows.forEach((row) => {
          const sid = row.cell_id;
          if (!sid) return;
          const cur = totals[sid] || { tokensIn: 0, tokensOut: 0, costUsd: 0, attempts: 0 };
          cur.tokensIn += Number(row.payload.tokens_in) || 0;
          cur.tokensOut += Number(row.payload.tokens_out) || 0;
          cur.costUsd += Number(row.payload.cost_usd) || 0;
          cur.attempts += 1;
          totals[sid] = cur;
        });
        return totals;
      }
      // RALPHUS-LOGS-OUTPUT-BTN:BEGIN
      // The Logs modal's cells/proofs-tab "output" button: pending/queued
      // rows fall back to "—" (nothing has run yet to fetch), every other
      // state renders a button that fetches on demand via
      // `openLinkedOutputPopup` rather than reading a stored `output` field.
      // Deliberately pure -- its only free variable is `esc`, injected as a
      // parameter by test/board-logs-output-btn.mjs -- so the empty-state
      // fallback and key wiring can be exercised under `node --test`.
      /**
       * Renders a Logs-modal "output" button that opens `key`'s raw output
       * (RAL-295) via `openLinkedOutputPopup`, fetched on demand from the
       * pane/terminal-log-attempts endpoints -- or the same "—" fallback
       * used elsewhere once a cell/proof step hasn't run yet.
       * @param {string} key
       * @param {string} state
       * @returns {string}
       */
      function logsOutputBtn(key, state) {
        if (["pending", "queued"].includes(state)) return "—";
        return `<button class="btn" style="padding:1px 6px;font-size:11px" data-click="openLinkedOutputPopup" data-key="${esc(key)}" data-tip="View this row's raw output — the agent's response or command stdout/stderr.\nWho/when: checking what a cell or proof step actually did, without leaving the Logs modal.\nFetched live from its terminal log each time you click, not a stored summary.">▶ output</button>`;
      }
      // RALPHUS-LOGS-OUTPUT-BTN:END
      /**
       * Renders the Logs modal cells-tab rows, each with its current-squad and
       * cumulative-across-restarts (RAL-160) token/cost figures.
       * @param {SquadView} squad
       * @returns {string[]}
       */
      function cumulativeCellRows(squad) {
        const totals = cumulativeCellTotals(squad.id);
        const loading = cartoModalCache[`cellstotals:${squad.id}`] === null;
        /** @type {string[]} */
        const rows = [];
        squad.tasks.forEach((t, ti) => t.cells.forEach((s, si) => {
          const cum = totals[s.id];
          const cumText = cum
            ? `input ${cum.tokensIn} · output ${cum.tokensOut} · ${fmtCostUsd(cum.costUsd)} (${cum.attempts} attempt${cum.attempts === 1 ? "" : "s"})`
            : (loading ? "…" : "—");
          rows.push(tblRow([
            esc(t.name),
            `<span class="mono">${esc(s.id)}</span>`,
            pill(s.state),
            `input ${s.tokens_in} · output ${s.tokens_out}`,
            `${fmtCostUsd(s.cost_usd)}${s.maximum_budget_usd ? ` <span style="color:var(--muted)">/ cap $${s.maximum_budget_usd.toFixed(4)}</span>` : ""}`,
            cumText,
            logsOutputBtn(`cell|${squad.id}|${ti}|${si}`, s.state),
            s.error ? `<span style="color:var(--failed)">${esc(s.error)}</span>` : "",
          ]));
        }));
        return rows;
      }
      /**
       * Renders the Logs modal's active-tab table body for a squad.
       * @param {SquadView} squad
       * @returns {string}
       */
      function logsBody(squad) {
        /**
         * @param {string[]} head
         * @param {string[]} rows
         * @returns {string}
         */
        const T = (head, rows) => `<table style="width:100%;border-collapse:collapse;font-size:13px"><thead><tr>${head.map((h) => `<th style="text-align:left;padding:5px 10px;color:var(--muted);font-weight:500">${h}</th>`).join("")}</tr></thead><tbody>${rows.join("")}</tbody></table>`;
        if (logsTab === "events") {
          const d = cartoModalCache[`squad:${squad.id}`];
          return cartoTableHtml(d ? d.rows : null);
        }
        if (logsTab === "tasks") {
          const reasonHead = `<span data-tip="${TASK_REASON_TOOLTIP}">reason</span>`;
          return T(["task", "state", "cells", reasonHead], squad.tasks.map((t, ti) => tblRow([
            esc(t.name),
            pill(t.state),
            t.cells.length,
            t.error
              ? `<span style="color:var(--failed)">${esc(t.error)}</span>${taskErrorAutofixBtn(t.error, t.project, squad.id, ti)}`
              : "",
          ])));
        }
        if (logsTab === "proofs") {
          /** @type {string[]} */
          const rows = [];
          // RAL-295: the output button links to the pane/terminal-log-attempts
          // endpoints (logsOutputBtn) rather than reading `v.output` directly,
          // for consistency with the cells tab's equivalent button.
          squad.tasks.forEach((t, ti) => {
            (t.proof || []).forEach((v, vi) => rows.push(tblRow([esc(t.name), "task", esc(v.id || "—"), esc(v.kind), pill(v.state), logsOutputBtn(`proof|${squad.id}|${ti}|task|-1|${vi}`, v.state)])));
            (t.cells || []).forEach((s, si) => (s.proof || []).forEach((v, vi) =>
              rows.push(tblRow([esc(t.name), `cell ${esc(s.id)}`, esc(v.id || "—"), esc(v.kind), pill(v.state), logsOutputBtn(`proof|${squad.id}|${ti}|cell|${si}|${vi}`, v.state)]))));
          });
          return rows.length ? T(["task", "scope", "id", "kind", "state", "output"], rows) : `<div class="empty">No proofs.</div>`;
        }
        const cumHead = `<span data-tip="${CELL_CUM_TOOLTIP}">cumulative (all restarts)</span>`;
        return T(["task", "cell", "state", "tokens (this squad)", "cost (this squad)", cumHead, "output", "error"], cumulativeCellRows(squad));
      }
      /**
       * Plain-text form of a Cartographer row's squad/guardian/cell references
       * (RAL-294) — the Markdown-export sibling of cartoRefChip's HTML links.
       * @param {CartographerRow} row
       * @returns {string}
       */
      function cartoRefText(row) {
        /** @type {string[]} */
        const parts = [];
        if (row.squad_id) parts.push(row.squad_id);
        if (row.guardian_id) parts.push(row.guardian_id);
        if (row.cell_id) parts.push(row.cell_id);
        return parts.length ? parts.join(" · ") : "—";
      }
      // ---------- logs modal: copy active tab as Markdown (RAL-294) ----------
      /**
       * @typedef {object} LogsMdTable
       * @property {string[]} head
       * @property {string[][]} rows plain-text cells, already escaped for a pipe table
       * @property {string[]} notes footnote lines (`[1]: ...`) referenced from `head`/`rows`
       * @property {string[]} warnings free-form lines shown above the table (e.g. a capped fetch)
       */
      // RALPHUS-LOGS-MARKDOWN:BEGIN
      /**
       * Escapes a plain-text value for a Markdown pipe-table cell: collapses
       * newlines to spaces and escapes literal `|` so the row can't break.
       * @param {string|number|null|undefined} value
       * @returns {string}
       */
      function mdEscapeCell(value) {
        const s = String(value ?? "").replace(/\|/g, "\\|").replace(/[\r\n]+/g, " ").trim();
        return s || "—";
      }
      /**
       * Renders a table cell that may be long or multi-line: short single-line
       * values are inlined as-is, longer ones are truncated with a `[n]`
       * footnote marker whose full text is pushed onto `notes` for the
       * Notes block below the table.
       * @param {string|null|undefined} value
       * @param {string[]} notes
       * @returns {string}
       */
      function mdCellWithFootnote(value, notes) {
        const s = String(value ?? "").trim();
        if (!s) return "—";
        const maxLen = 120;
        if (s.length <= maxLen && !/[\r\n]/.test(s)) return mdEscapeCell(s);
        const idx = notes.length + 1;
        notes.push(`[${idx}]: ${s}`);
        return `${mdEscapeCell(s.split(/[\r\n]/)[0].slice(0, maxLen))}… [${idx}]`;
      }
      /**
       * Renders a header cell, folding a substantive column tooltip into a
       * `[n]` footnote (in `notes`) rather than dropping it in the text export.
       * @param {string} label
       * @param {string} [tooltip]
       * @param {string[]} [notes]
       * @returns {string}
       */
      function mdHeadCell(label, tooltip, notes) {
        if (!tooltip || !notes) return label;
        const idx = notes.length + 1;
        notes.push(`[${idx}]: ${tooltip}`);
        return `${label} [${idx}]`;
      }
      /**
       * Serializes a `LogsMdTable` into a pipe-delimited Markdown table, with
       * any warnings above it and any footnotes below it.
       * @param {LogsMdTable} t
       * @returns {string}
       */
      function mdTableToText(t) {
        /** @type {string[]} */
        const lines = [];
        if (t.warnings.length) { lines.push(...t.warnings, ""); }
        lines.push("| " + t.head.join(" | ") + " |");
        lines.push("| " + t.head.map(() => "---").join(" | ") + " |");
        t.rows.forEach((r) => lines.push("| " + r.join(" | ") + " |"));
        if (t.notes.length) { lines.push("", "--- Notes ---", ...t.notes); }
        return lines.join("\n");
      }
      /**
       * Plain-text mirror of `cartoRefChip` — the same squad/guardian/entity
       * references, without the HTML links.
       * @param {CartographerRow} row
       * @returns {string}
       */
      function cartoRefText(row) {
        /** @type {string[]} */
        const parts = [];
        if (row.squad_id) parts.push(row.squad_id);
        if (row.guardian_id) parts.push(row.guardian_id);
        const target = cartoResolveEntity(row);
        if (target) parts.push(target.label);
        else if (row.cell_id) parts.push(row.cell_id);
        return parts.length ? parts.join(" · ") : "—";
      }
      // Safety cap on an "all rows" Markdown export: keeps a squad with a
      // huge event/cell history from turning one click into thousands of
      // fetch round-trips. logsCopyBtnHtml below documents this to the user.
      const LOGS_COPY_ALL_ROWS_CAP = 2000;
      /**
       * Fetches every Cartographer row matching `filter` across as many pages
       * as needed (up to `cap`), for the "all rows" copy-as-Markdown scope.
       * @param {Partial<CartoFilter>} filter
       * @param {number} cap
       * @returns {Promise<{rows: CartographerRow[], total: number}>}
       */
      async function fetchAllCartoRows(filter, cap) {
        /** @type {CartographerRow[]} */
        const rows = [];
        let offset = 0, total = Infinity;
        const limit = 200;
        while (rows.length < total && rows.length < cap) {
          const page = await cartoFetch(/** @type {CartoFilter} */ ({ ...filter, limit, offset, sortCol: "time", dir: "desc" }));
          total = page.total || 0;
          if (!page.rows.length) break;
          rows.push(...page.rows);
          offset += limit;
          if (page.rows.length < limit) break;
        }
        return { rows, total };
      }
      /**
       * Sums a set of "cell completed" Cartographer rows into the same
       * per-cell totals shape as `cumulativeCellTotals`, for rows fetched
       * outside the modal cache (the "all rows" copy scope).
       * @param {CartographerRow[]} rows
       * @returns {{[cellId: string]: {tokensIn: number, tokensOut: number, costUsd: number, attempts: number}}}
       */
      function cellTotalsFromRows(rows) {
        /** @type {{[cellId: string]: {tokensIn: number, tokensOut: number, costUsd: number, attempts: number}}} */
        const totals = {};
        rows.forEach((row) => {
          const sid = row.cell_id;
          if (!sid) return;
          const cur = totals[sid] || { tokensIn: 0, tokensOut: 0, costUsd: 0, attempts: 0 };
          cur.tokensIn += Number(row.payload.tokens_in) || 0;
          cur.tokensOut += Number(row.payload.tokens_out) || 0;
          cur.costUsd += Number(row.payload.cost_usd) || 0;
          cur.attempts += 1;
          totals[sid] = cur;
        });
        return totals;
      }
      /**
       * Builds the events tab's Markdown table.
       * @param {string} squadId
       * @param {"current"|"all"} scope
       * @returns {Promise<LogsMdTable>}
       */
      async function eventsMdTable(squadId, scope) {
        /** @type {string[]} */
        const notes = [];
        /** @type {string[]} */
        const warnings = [];
        const cached = cartoModalCache[`squad:${squadId}`];
        const fetched = scope === "all"
          ? await fetchAllCartoRows({ squad_id: squadId }, LOGS_COPY_ALL_ROWS_CAP)
          : { rows: cached ? cached.rows : [], total: cached ? cached.total : 0 };
        const rows = fetched.rows, total = fetched.total;
        if (rows.length < total) warnings.push(`Note: showing ${rows.length} of ${total} events — ${scope === "all" ? "capped to avoid a very slow export; narrow the filters in the Logs tab for the rest." : "choose \"All rows\" or use the Logs tab for the rest."}`);
        const head = ["time", "level", "source", "scope", "task", "refs", "message"];
        const mdRows = rows.map((r) => {
          const payload = r.payload && Object.keys(r.payload).length ? JSON.stringify(r.payload) : "";
          return [
            mdEscapeCell(new Date(r.at_ms).toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit", second: "2-digit" })),
            mdEscapeCell(r.level),
            mdEscapeCell(r.source),
            mdEscapeCell(r.scope || "—"),
            mdEscapeCell(r.task || "—"),
            mdEscapeCell(cartoRefText(r)),
            payload ? mdCellWithFootnote(`${r.message}\npayload: ${payload}`, notes) : mdEscapeCell(r.message),
          ];
        });
        return { head, rows: mdRows, notes, warnings };
      }
      /**
       * Builds the tasks tab's Markdown table.
       * @param {SquadView} squad
       * @returns {LogsMdTable}
       */
      function tasksMdTable(squad) {
        /** @type {string[]} */
        const notes = [];
        const head = ["task", "state", "cells", mdHeadCell("reason", TASK_REASON_TOOLTIP, notes)];
        const rows = squad.tasks.map((t) => [
          mdEscapeCell(t.name),
          mdEscapeCell(t.state),
          mdEscapeCell(t.cells.length),
          t.error ? mdCellWithFootnote(t.error, notes) : "—",
        ]);
        return { head, rows, notes, warnings: [] };
      }
      /**
       * Builds the cells tab's Markdown table, including cumulative
       * (across-restarts) totals for the chosen scope.
       * @param {SquadView} squad
       * @param {"current"|"all"} scope
       * @returns {Promise<LogsMdTable>}
       */
      async function cellsMdTable(squad, scope) {
        /** @type {string[]} */
        const notes = [];
        /** @type {string[]} */
        const warnings = [];
        /** @type {{[cellId: string]: {tokensIn: number, tokensOut: number, costUsd: number, attempts: number}}} */
        let totals;
        if (scope === "all") {
          const fetched = await fetchAllCartoRows({ squad_id: squad.id, q: "cell completed" }, LOGS_COPY_ALL_ROWS_CAP);
          if (fetched.rows.length < fetched.total) warnings.push(`Note: cumulative totals use ${fetched.rows.length} of ${fetched.total} "cell completed" events — capped to avoid a very slow export.`);
          totals = cellTotalsFromRows(fetched.rows);
        } else {
          const cache = cartoModalCache[`cellstotals:${squad.id}`];
          totals = cellTotalsFromRows(cache ? cache.rows : []);
          if (cache && cache.rows.length < cache.total) warnings.push(`Note: cumulative totals reflect the currently loaded ${cache.rows.length} of ${cache.total} "cell completed" events — choose "All rows" for the complete history.`);
        }
        const head = ["task", "cell", "state", "tokens (this squad)", "cost (this squad)", mdHeadCell("cumulative (all restarts)", CELL_CUM_TOOLTIP, notes), "error"];
        /** @type {string[][]} */
        const rows = [];
        squad.tasks.forEach((t) => t.cells.forEach((s) => {
          const cum = totals[s.id];
          const cumText = cum
            ? `input ${cum.tokensIn} · output ${cum.tokensOut} · ${fmtCostUsd(cum.costUsd)} (${cum.attempts} attempt${cum.attempts === 1 ? "" : "s"})`
            : "—";
          rows.push([
            mdEscapeCell(t.name),
            mdEscapeCell(s.id),
            mdEscapeCell(s.state),
            mdEscapeCell(`input ${s.tokens_in} · output ${s.tokens_out}`),
            mdEscapeCell(`${fmtCostUsd(s.cost_usd)}${s.maximum_budget_usd ? ` / cap $${s.maximum_budget_usd.toFixed(4)}` : ""}`),
            mdEscapeCell(cumText),
            s.error ? mdCellWithFootnote(s.error, notes) : "—",
          ]);
        }));
        return { head, rows, notes, warnings };
      }
      /**
       * Builds one proofs-tab Markdown row for a single proof step.
       * @param {string} taskName
       * @param {string} scope
       * @param {ProofView} v
       * @param {string[]} notes
       * @returns {string[]}
       */
      function proofMdRow(taskName, scope, v, notes) {
        return [
          mdEscapeCell(taskName),
          mdEscapeCell(scope),
          mdEscapeCell(v.id || "—"),
          mdEscapeCell(v.kind),
          mdEscapeCell(v.state),
          v.output ? mdCellWithFootnote(v.output, notes) : "—",
        ];
      }
      /**
       * Builds the proofs tab's Markdown table.
       * @param {SquadView} squad
       * @returns {LogsMdTable}
       */
      function proofsMdTable(squad) {
        /** @type {string[]} */
        const notes = [];
        const head = ["task", "scope", "id", "kind", "state", "output"];
        /** @type {string[][]} */
        const rows = [];
        squad.tasks.forEach((t) => {
          (t.proof || []).forEach((v) => rows.push(proofMdRow(t.name, "task", v, notes)));
          (t.cells || []).forEach((s) => (s.proof || []).forEach((v) => rows.push(proofMdRow(t.name, `cell ${s.id}`, v, notes))));
        });
        return { head, rows, notes, warnings: [] };
      }
      /**
       * Builds the Markdown table for the Logs modal's currently active tab.
       * @param {string} tab
       * @param {SquadView} squad
       * @param {"current"|"all"} scope
       * @returns {Promise<LogsMdTable>}
       */
      async function buildLogsMdTable(tab, squad, scope) {
        if (tab === "events") return eventsMdTable(squad.id, scope);
        if (tab === "cells") return cellsMdTable(squad, scope);
        if (tab === "proofs") return proofsMdTable(squad);
        return tasksMdTable(squad);
      }
      // RALPHUS-LOGS-MARKDOWN:END
      /**
       * Renders the Logs modal's "copy active tab as Markdown" trigger button,
       * disabled while that tab's underlying data is still loading.
       * @param {string} tab
       * @returns {string}
       */
      function logsCopyBtnHtml(tab) {
        const busy = (tab === "events" && !cartoModalCache[`squad:${logSquadId}`])
          || (tab === "cells" && !cartoModalCache[`cellstotals:${logSquadId}`]);
        const tip = "Copy this tab's table as a Markdown table you can paste into a PR description, ticket, or chat message.\n\"Current view\" copies what's shown here; \"All rows\" fetches the complete underlying history first (up to " + LOGS_COPY_ALL_ROWS_CAP + " rows) — that can be slow for a squad with a lot of event/cell/proof history.";
        return `<button id="logs-copy-btn" class="copy-btn" data-tip="${esc(tip)}" ${busy ? "disabled" : ""} data-click="showLogsCopyMenu" data-tab="${esc(tab)}">⧉ Copy as Markdown</button>`;
      }
      /**
       * Opens the "current view" / "all rows" scope menu for the Logs modal's copy-as-Markdown button.
       * @param {MouseEvent} e
       * @param {string} tab
       * @returns {void}
       */
      function showLogsCopyMenu(e, tab) {
        e.stopPropagation();
        const existing = document.getElementById("logs-copy-menu");
        if (existing) { existing.remove(); return; }
        const rect = /** @type {HTMLElement} */ (e.currentTarget).getBoundingClientRect();
        const menu = document.createElement("div");
        menu.id = "logs-copy-menu";
        menu.className = "copy-menu";
        menu.style.top = (rect.bottom + 4) + "px";
        menu.style.left = rect.left + "px";
        menu.innerHTML = `<div class="copy-menu-item" data-click="copyLogsAs" data-tab="${esc(tab)}" data-scope="current" data-tip="Copy just what's currently loaded in this tab.">Current view</div><div class="copy-menu-item" data-click="copyLogsAs" data-tab="${esc(tab)}" data-scope="all" data-tip="Fetch and copy the complete history for this tab first — can be slow for a squad with a lot of event/cell/proof history.">All rows</div>`;
        document.body.appendChild(menu);
        setTimeout(() => document.addEventListener("click", () => { const m = document.getElementById("logs-copy-menu"); if (m) m.remove(); }, { once: true }), 0);
      }
      /**
       * Copies the Logs modal's active tab to the clipboard as Markdown, at the chosen scope.
       * @param {MouseEvent} e
       * @param {string} tab
       * @param {string} scope
       * @returns {Promise<void>}
       */
      async function copyLogsAs(e, tab, scope) {
        e.stopPropagation();
        const menu = document.getElementById("logs-copy-menu");
        if (menu) menu.remove();
        const squad = logSquadId ? findSquad(logSquadId) : null;
        const btn = document.getElementById("logs-copy-btn");
        if (!squad) return;
        if (btn) { btn.textContent = "…"; /** @type {HTMLButtonElement} */ (btn).disabled = true; }
        try {
          const table = await buildLogsMdTable(tab, squad, scope === "all" ? "all" : "current");
          await writeClipboardText(mdTableToText(table));
          if (btn) { btn.textContent = "✓ Copied"; btn.classList.add("copied"); }
        } catch (_) {
          if (btn) btn.textContent = "✗ Copy failed";
        } finally {
          if (btn) setTimeout(() => { btn.textContent = "⧉ Copy as Markdown"; btn.classList.remove("copied"); /** @type {HTMLButtonElement} */ (btn).disabled = false; }, 1200);
        }
      }

      // ---------- uber-log-viewer timeline modal (RAL-155) ----------
      /**
       * @typedef {object} SquadTimelineMeta
       * @property {string} squad_id
       * @property {number} generated_at_ms
       * @property {number|null} start_ms
       * @property {number|null} end_ms
       * @property {number} event_count
       * @property {number} terminal_log_count
       * @property {number} task_count
       * @property {number} cell_count
       * @property {boolean} truncated
       * @property {boolean} gaps_possible
       */
      /**
       * @typedef {object} SquadTimeline
       * @property {SquadTimelineMeta} meta
       * @property {object[]} entries
       * @property {string} text
       * @property {string} file_path
       */
      /**
       * Opens the Timeline modal for a squad from a button click, generating a
       * fresh merged view on every open (the daemon regenerates its temp file
       * each call too — see `squadTimelineBtn`'s tooltip).
       * @param {MouseEvent} e
       * @param {string} id
       * @returns {Promise<void>}
       */
      async function openSquadTimeline(e, id) {
        e.stopPropagation();
        const squad = findSquad(id);
        if (!squad) return;
        byId("modal-root").innerHTML = `
          <div class="modal-bg"><div class="modal" style="width:880px;max-width:94vw">
            <h2>Timeline — ${esc(squad.label || squad.id)}</h2>
            <p style="font-size:13px;color:var(--muted)">Generating…</p>
          </div></div>`;
        /** @type {SquadTimeline} */
        let timeline;
        try {
          const resp = await fetch(`/api/squads/${id}/timeline`);
          if (!resp.ok) { closeModal(); alert("Failed to generate the timeline."); return; }
          timeline = await resp.json();
        } catch (_) { closeModal(); alert("Failed to generate the timeline: network error."); return; }
        renderSquadTimeline(squad, timeline);
      }
      /**
       * Renders a generated `SquadTimeline` into the Timeline modal.
       * @param {SquadView} squad
       * @param {SquadTimeline} timeline
       * @returns {void}
       */
      function renderSquadTimeline(squad, timeline) {
        const m = timeline.meta;
        const gapsNote = m.gaps_possible
          ? `<p style="font-size:12px;color:var(--incomplete)" data-tip="Cartographer's retention pruning (retention_days/max_rows) has already removed some of this squad's earliest history — this timeline is best-effort, not guaranteed complete.">⚠ earliest history may have been pruned — this view is best-effort</p>`
          : "";
        const truncatedNote = m.truncated
          ? `<p style="font-size:12px;color:var(--incomplete)" data-tip="This squad generated more events than the conservative per-generation cap — only the earliest events up to the cap are shown.">⚠ truncated — this squad has more events than fit in one generation</p>`
          : "";
        byId("modal-root").innerHTML = `
          <div class="modal-bg" onclick="if(event.target===this)closeModal()"><div class="modal" style="width:880px;max-width:94vw">
            <h2>Timeline — ${esc(squad.label || squad.id)}</h2>
            <p style="font-size:12px;color:var(--muted)" data-tip="Every state transition, Cartographer event, and terminal-log excerpt for this squad, merged in chronological order.">${m.event_count} events · ${m.terminal_log_count} terminal-log refs · ${m.task_count} tasks · ${m.cell_count} cells</p>
            <p style="font-size:12px;color:var(--muted)" data-tip="Where the daemon (best-effort) wrote this generation's rendered text as a temp file — not a persistent export.">file: <span class="mono">${esc(timeline.file_path)}</span>${copyBtn(timeline.file_path)}</p>
            ${gapsNote}${truncatedNote}
            <pre style="max-height:55vh;overflow:auto;background:var(--bg);border:1px solid var(--border);border-radius:8px;padding:10px;font-size:12px;white-space:pre-wrap;word-break:break-word">${esc(timeline.text)}</pre>
            <div class="btn-row">${copyBtn(timeline.text)}<button class="btn" onclick="closeModal()" data-tip="Close this popup.">Close</button></div>
          </div></div>`;
      }
      // Files tab: validate + queue every loaded file as its own squad, named
      // after the file. Files that fail validation or submission stay in the
      // batch (with their error shown inline) so the user can fix and retry
      // without re-loading the ones that already succeeded.
      /**
       * Validates and submits every loaded Files-tab file, each as its own squad.
       * @returns {Promise<void>}
       */
      async function submitTaskFiles() {
        const errEl = byId("nt-err");
        if (!ntFiles.length) { errEl.innerHTML = "Nothing to submit — drop or select at least one .toml file first."; return; }
        /** @type {NtFile[]} */
        const succeeded = [];
        for (const f of ntFiles) {
          f.err = null;
          const v = await ntValidateOne(f.content);
          if (!v.valid) {
            f.err = (v.errors || []).map((e) => `line ${e.line ?? "?"}: ${esc(e.message)}`).join("<br>") || "invalid TOML";
            continue;
          }
          const label = f.name.replace(/\.toml$/i, "");
          const resp = await fetch("/api/squads", { method: "POST", headers: traceHeaders(), body: JSON.stringify({ toml: f.content, label }) });
          if (!resp.ok) {
            const b = await resp.json().catch(() => ({}));
            f.err = (b.error && b.error.message) || "submit failed";
            continue;
          }
          succeeded.push(f);
        }
        if (succeeded.length < ntFiles.length) {
          ntFiles = ntFiles.filter((f) => !succeeded.includes(f));
          renderNewTaskModal();
          byId("nt-err").innerHTML = `${succeeded.length}/${succeeded.length + ntFiles.length} queued — ${ntFiles.length} failed, see errors above.`;
          return;
        }
        closeModal(); tick();
      }
      /**
       * Validates and submits the New Task modal (dispatches to the active tab).
       * @returns {Promise<void>}
       */
      async function submitTask() {
        if (ntTab === "simple") { await submitTaskSimple(); return; }
        if (ntTab === "files") { await submitTaskFiles(); return; }
        const toml = /** @type {HTMLTextAreaElement} */ (document.getElementById("nt-toml")).value;
        const label = /** @type {HTMLInputElement} */ (document.getElementById("nt-label")).value;
        const errEl = byId("nt-err");
        if (!toml.trim()) { errEl.innerHTML = "Nothing to submit — paste TOML first."; return; }
        const v = await ntValidate();
        if (!v || !v.valid) return; // errors are already rendered by ntValidate
        const resp = await fetch("/api/squads", { method: "POST", headers: traceHeaders(), body: JSON.stringify({ toml, label: label || null }) });
        if (!resp.ok) { const b = await resp.json().catch(() => ({})); errEl.textContent = (b.error && b.error.message) || "submit failed"; return; }
        closeModal(); tick();
      }

      // ---------- worktree → cell linking (RAL-71) ----------
      // Normalise a path for comparison: forward slashes, lowercase, no trailing slash.
      /**
       * Normalizes a filesystem path for comparison (forward slashes, lowercase, no trailing slash).
       * @param {string} p
       * @returns {string}
       */
      function normPath(p) {
        if (!p) return "";
        return p.replace(/\\/g, "/").toLowerCase().replace(/\/+$/, "");
      }
      // A cell cwd and a review worktree path are "linked" when one is a path-boundary
      // prefix of the other (i.e. one directory is at or inside the other).
      /**
       * Checks whether a cell's cwd and a review worktree path are the same or nested.
       * @param {string} cellCwd
       * @param {string} worktreePath
       * @returns {boolean}
       */
      function pathLinked(cellCwd, worktreePath) {
        const a = normPath(cellCwd), b = normPath(worktreePath);
        if (!a || !b) return false;
        const longer = a.length >= b.length ? a : b;
        const shorter = a.length < b.length ? a : b;
        if (!longer.startsWith(shorter)) return false;
        const next = longer[shorter.length];
        return next === "/" || next === undefined;
      }
      /**
       * @typedef {object} LinkedCell
       * @property {string} squadId
       * @property {number} taskIdx
       * @property {number} cellIdx
       * @property {string} name
       * @property {string} taskName
       * @property {string} state
       */
      // Collect all cells (across all loaded squads) whose cwd is linked to the given worktree.
      /**
       * Finds every cell (across all loaded squads) whose cwd is linked to the given worktree path.
       * @param {string} worktreePath
       * @returns {LinkedCell[]}
       */
      function findLinkedCells(worktreePath) {
        /** @type {LinkedCell[]} */
        const results = [];
        for (const squad of squads) {
          squad.tasks.forEach((t, ti) => {
            t.cells.forEach((s, si) => {
              if (s.cwd && pathLinked(s.cwd, worktreePath)) {
                results.push({ squadId: squad.id, taskIdx: ti, cellIdx: si,
                  name: s.name ?? s.id, taskName: t.name, state: s.state });
              }
            });
          });
        }
        return results;
      }
      // Cell state priority for the summary dot (worst-first).
      const CELL_STATE_RANK = ["running", "failed", "pending", "queued", "cancelled", "done"];
      /**
       * Toggles a worktree's linked-cells dropdown.
       * @param {string} key
       * @returns {void}
       */
      function toggleWorktreeMenu(key) {
        worktreeMenuOpen[key] = !worktreeMenuOpen[key];
        renderReviewDetail();
      }
      // Render resolver terminal buttons for a review branch (RAL-102).
      // Every resolver agent (not just claude-code) now runs inside tmux, so
      // the dropdown is available whenever a review worktree exists — "not
      // currently live" degrades gracefully at click/peek time instead of
      // pre-disabling. "Open Worktree Shell" (a plain shell, unrelated to
      // tmux) remains available too, since it's useful even between
      // resolver passes.
      /**
       * Renders a review branch's resolver terminal button(s), varying by cell/live/agent state.
       * @param {GuardianView} g
       * @param {GuardianBranch} b
       * @returns {string}
       */
      function resolverTerminalBtns(g, b) {
        // RAL-149/<new>: proof_pending and actioning also have a live
        // resolver tmux cell (the dedicated final-proof call, or the
        // feedback-revision agent pass), so they count as "live" alongside
        // in_progress (the fix pass / rebase itself).
        const isLive = g.status === "merging" && (b.merge_status === "in_progress" || b.merge_status === "proof_pending" || b.merge_status === "actioning");
        if (!b.worktree) {
          const tip = "No worktree yet — terminal access becomes available once this branch starts being processed.";
          return `<span data-tip="${tip}"><button class="btn primary" disabled style="pointer-events:none">Show Live View</button></span>`;
        }
        const key = `guardian|${g.id}|${b.id}`;
        const previewLabel = peekOpen[key] ? "Hide Live View" : (isLive ? "Watch Live" : "Show Live View");
        const openLabel = isLive ? "Open Live Terminal Log" : "Open Terminal Log";
        const previewTip = "Peek at the conflict resolver's live tmux pane — auto-refreshing, read-only.\nNothing you do here is ever sent to the agent.\nShows 'terminal session has ended' once the resolver isn't running.";
        const openTip = "Attach an interactive terminal to the conflict resolver's live tmux cell.\nShows the runner's own log/event stream, not the agent's actual conversation.\nOnly available while the resolver is actively running — fails gracefully otherwise.";
        const wtTip = isLive
          ? "Open a shell in the review worktree while the resolver is working.\nRun 'git diff' or 'git status' to watch files change in real time."
          : "Open a shell in the review worktree to manually inspect or fix merge conflicts.";
        const previewBtn = `<button class="btn primary" data-click="togglePeek" data-key="${esc(key)}" style="border-radius:6px 0 0 6px" data-tip="${previewTip}">${previewLabel}</button>`;
        const items = terminalMenuItem(key, openLabel, "openGuardianBranchTerminalMenuItem", { gid: g.id, bid: b.id, mode: "open" }, openTip)
          + openAgentMenuItem(key, "openGuardianBranchTerminalMenuItem", { gid: g.id, bid: b.id, mode: "agent" },
              isLive, b.resolver_agent_session_id, resolverOf(g))
          + terminalMenuItem(key, "Open Worktree Shell", "openGuardianBranchTerminalMenuItem", { gid: g.id, bid: b.id, mode: "worktree" }, wtTip)
          + terminalMenuItem(key, "View Attempt History",
              "toggleHistoryMenuItem", {},
              "List every durably-persisted terminal-log attempt for this resolver, including past reattaches.\nWho/when: the pane died or reattached and you need to see what happened right before, after the live view is gone.\nEach attempt's log survives pane death and daemon restarts.");
        return `${previewBtn}${terminalMenuHtml(key, items)}`;
      }
      // Sibling to resolverTerminalBtns — rendered outside the flex btn-row
      // (see the call site) so the box lays out on its own line, not squeezed
      // in as a flex item next to the buttons.
      /**
       * Renders a review branch's resolver live-pane peek box plus its
       * attempt-history box, if its worktree exists.
       * @param {GuardianView} g
       * @param {GuardianBranch} b
       * @returns {string}
       */
      function resolverPeekBox(g, b) {
        if (!b.worktree) return "";
        const key = `guardian|${g.id}|${b.id}`;
        return `${peekBox(key, b.started_at_ms ?? null)}${historyBox(key)}`;
      }

      // Render manual-checks-generation terminal buttons for a review (RAL-88
      // follow-up). Generation is a single guardian-level pass (not per-branch),
      // gated on `checks_state` rather than a worktree: "waiting" means
      // generation hasn't started yet (no tmux cell could exist), so the
      // button stays disabled until it reaches "generating" or "ready".
      /**
       * Renders a review's manual-checks-generation terminal button(s), varying by live/agent state.
       * @param {GuardianView} g
       * @returns {string}
       */
      function manualChecksTerminalBtns(g) {
        const cmds = g.manual_commands || [];
        const state = g.checks_state || (cmds.length ? "ready" : "waiting");
        if (state === "waiting") {
          const tip = "No terminal yet — becomes available once every enabled branch finishes rebasing and manual-checks generation starts.";
          return `<span data-tip="${tip}"><button class="btn" disabled style="pointer-events:none;font-size:11px;padding:1px 7px">Show Live View</button></span>`;
        }
        const isLive = state === "generating";
        const key = `guardian-manual|${g.id}`;
        const previewLabel = peekOpen[key] ? "Hide Live View" : (isLive ? "Watch Live" : "Show Live View");
        const openLabel = isLive ? "Open Live Terminal Log" : "Open Terminal Log";
        const previewTip = "Peek at the manual-checks generation's live tmux pane — auto-refreshing, read-only.\nNothing you do here is ever sent to the agent.\nShows 'terminal session has ended' once generation isn't running.";
        const openTip = "Attach an interactive terminal to the manual-checks generation's live tmux cell.\nShows the runner's own log/event stream, not the agent's actual conversation.\nOnly available while generation is actively running — fails gracefully otherwise.";
        const previewBtn = `<button class="btn" data-click="togglePeek" data-key="${esc(key)}" style="border-radius:6px 0 0 6px;font-size:11px;padding:1px 7px" data-tip="${previewTip}">${previewLabel}</button>`;
        const items = terminalMenuItem(key, openLabel, "openGuardianManualChecksTerminalMenuItem", { gid: g.id, mode: "open" }, openTip)
          + openAgentMenuItem(key, "openGuardianManualChecksTerminalMenuItem", { gid: g.id, mode: "agent" },
              isLive, g.manual_commands_agent_session_id, g.manual_commands_agent || resolverOf(g))
          + terminalMenuItem(key, "View Attempt History",
              "toggleHistoryMenuItem", {},
              "List every durably-persisted terminal-log attempt for this generation pass, including past reattaches.\nWho/when: the pane died or reattached and you need to see what happened right before, after the live view is gone.\nEach attempt's log survives pane death and daemon restarts.");
        return `${previewBtn}${terminalMenuHtml(key, items)}`;
      }
      // Sibling to manualChecksTerminalBtns — rendered outside the flex
      // btn-row so the box lays out on its own line.
      /**
       * Renders a review's manual-checks-generation live-pane peek box plus
       * its attempt-history box, if generation has started.
       * @param {GuardianView} g
       * @returns {string}
       */
      function manualChecksPeekBox(g) {
        const cmds = g.manual_commands || [];
        const state = g.checks_state || (cmds.length ? "ready" : "waiting");
        if (state === "waiting") return "";
        const key = `guardian-manual|${g.id}`;
        return `${peekBox(key, g.manual_checks_started_at_ms ?? null)}${historyBox(key)}`;
      }

