      // ---------- Triage tab (RAL-318) ----------
      // Flow view: a project rail, the pools (grouped project -> subproject),
      // the cells in flight, and the Arbiter reviews pools have drained into,
      // with a details pane for the selected pool. Configuration view: the
      // triage-type registry and every cron drain schedule.

      // RALPHUS-TRIAGE-FLOW-LOGIC:BEGIN
      /**
       * Splits a pool key into its project and optional subproject (RAL-346):
       * `"proj::core"` -> `{proj: "proj", sub: "core"}`, `"proj"` -> `{proj: "proj", sub: null}`.
       * @param {string} key
       * @returns {{proj: string, sub: string|null}}
       */
      function triageSplitKey(key) {
        const i = key.indexOf("::");
        return i < 0 ? { proj: key, sub: null } : { proj: key.slice(0, i), sub: key.slice(i + 2) };
      }
      /**
       * Everything the pool card, the rail, and the summary derive from one
       * pool. A count threshold drains in threshold-sized batches, so `ready`
       * is the cells sitting in full batches (each batch becomes its own
       * review) and `remainder` is what stays pooled.
       * @param {TriagePoolView} pool
       * @param {number} scheduleCount - cron schedules on this exact pool key
       * @returns {TriagePoolMath}
       */
      function triagePoolMath(pool, scheduleCount) {
        const t = pool.threshold ?? null;
        const fullBatches = t ? Math.floor(pool.count / t) : 0;
        const ready = t ? fullBatches * t : 0;
        return {
          threshold: t,
          fullBatches,
          ready,
          remainder: pool.count - ready,
          isReady: t !== null && pool.count >= t,
          stalled: t === null && scheduleCount === 0,
        };
      }
      /**
       * Parses one field of a `cron` crate expression into the set of values
       * it allows: `*`/`?`, a value, `a-b`, `*` or `a` or `a-b` with `/step`,
       * and comma lists. `names` maps 3-letter names (uppercase) to values.
       * @param {string} raw
       * @param {number} lo
       * @param {number} hi
       * @param {{[name: string]: number}} names
       * @returns {Set<number>|null} null when the field is malformed
       */
      function triageParseCronField(raw, lo, hi, names) {
        /** @type {Set<number>} */
        const out = new Set();
        /**
         * @param {string} s
         * @returns {number}
         */
        const val = (s) => {
          const up = s.toUpperCase();
          if (up in names) return names[up];
          return /^\d+$/.test(s) ? Number(s) : NaN;
        };
        for (const part of raw.split(",")) {
          if (!part) return null;
          const [range, stepRaw, extra] = part.split("/");
          if (extra !== undefined) return null;
          const step = stepRaw === undefined ? 1 : Number(stepRaw);
          if (!Number.isInteger(step) || step < 1) return null;
          let a = lo, b = hi;
          if (range !== "*" && range !== "?") {
            const bits = range.split("-");
            if (bits.length > 2) return null;
            a = val(bits[0]);
            b = bits.length === 2 ? val(bits[1]) : (stepRaw === undefined ? a : hi);
          }
          if (!Number.isInteger(a) || !Number.isInteger(b) || a < lo || b > hi || a > b) return null;
          for (let v = a; v <= b; v += step) out.add(v);
        }
        return out;
      }
      /**
       * Parses a schedule's cron expression the way the daemon's `cron` crate
       * does: 6 fields (sec min hour day-of-month month day-of-week) plus an
       * optional year; day-of-week runs 1-7 from Sunday.
       * @param {string} expr
       * @returns {TriageCronSpec|null}
       */
      function triageParseCron(expr) {
        const f = expr.trim().split(/\s+/).filter(Boolean);
        if (f.length !== 6 && f.length !== 7) return null;
        const months = { JAN: 1, FEB: 2, MAR: 3, APR: 4, MAY: 5, JUN: 6, JUL: 7, AUG: 8, SEP: 9, OCT: 10, NOV: 11, DEC: 12 };
        const days = { SUN: 1, MON: 2, TUE: 3, WED: 4, THU: 5, FRI: 6, SAT: 7 };
        const sec = triageParseCronField(f[0], 0, 59, {});
        const min = triageParseCronField(f[1], 0, 59, {});
        const hour = triageParseCronField(f[2], 0, 23, {});
        const dom = triageParseCronField(f[3], 1, 31, {});
        const mon = triageParseCronField(f[4], 1, 12, months);
        const dow = triageParseCronField(f[5], 1, 7, days);
        const year = f.length === 7 ? triageParseCronField(f[6], 1970, 2100, {}) : null;
        if (!sec || !min || !hour || !dom || !mon || !dow || (f.length === 7 && !year)) return null;
        return { sec, min, hour, dom, mon, dow, year };
      }
      /**
       * Human-readable reason a cron expression would be rejected, or "" when it parses.
       * @param {string} expr
       * @returns {string}
       */
      function triageCronError(expr) {
        const f = expr.trim().split(/\s+/).filter(Boolean);
        if (!f.length) return "Enter a cron expression, or remove this schedule.";
        if (f.length !== 6 && f.length !== 7) {
          return `Needs 6 fields — sec min hour day month weekday (plus an optional year) — got ${f.length}.`;
        }
        return triageParseCron(expr) ? "" : "One of the fields isn't valid (weekday runs 1-7 from Sunday, or SUN-SAT).";
      }
      /**
       * The first time strictly after `afterMs` (UTC) that `spec` matches, or
       * null when nothing matches within the next ten years.
       * @param {TriageCronSpec} spec
       * @param {number} afterMs
       * @returns {number|null}
       */
      function triageCronNextAfter(spec, afterMs) {
        /**
         * @param {Set<number>} s
         * @returns {number[]}
         */
        const sorted = (s) => [...s].sort((x, y) => x - y);
        const hours = sorted(spec.hour), mins = sorted(spec.min), secs = sorted(spec.sec);
        const start = new Date(afterMs);
        let day = Date.UTC(start.getUTCFullYear(), start.getUTCMonth(), start.getUTCDate());
        for (let i = 0; i < 3660; i++, day += 86400000) {
          const d = new Date(day);
          if (!spec.mon.has(d.getUTCMonth() + 1) || !spec.dom.has(d.getUTCDate()) || !spec.dow.has(d.getUTCDay() + 1)) continue;
          if (spec.year && !spec.year.has(d.getUTCFullYear())) continue;
          if (day + 86400000 <= afterMs) continue;
          for (const h of hours) {
            if (day + (h + 1) * 3600000 <= afterMs) continue;
            for (const m of mins) {
              if (day + h * 3600000 + (m + 1) * 60000 <= afterMs) continue;
              for (const s of secs) {
                const t = day + h * 3600000 + m * 60000 + s * 1000;
                if (t > afterMs) return t;
              }
            }
          }
        }
        return null;
      }
      /**
       * When a schedule will next actually drain its pool: its next cron
       * occurrence after `nowMs` that is also a qualifying every-Nth one,
       * counting occurrences from the daemon's cursor exactly like
       * `triage::advance_schedule` / `schedule_occurrence_fires`.
       * @param {TriageScheduleView} s
       * @param {number} nowMs
       * @returns {number|null}
       */
      function triageScheduleNextFire(s, nowMs) {
        const spec = triageParseCron(s.cron_expr);
        if (!spec) return null;
        let cursor = s.last_checked_ms ?? s.anchor_date_ms;
        let count = s.occurrence_count;
        for (let i = 0; i < 5000; i++) {
          const t = triageCronNextAfter(spec, cursor);
          if (t === null) return null;
          count++;
          cursor = t;
          if (t > nowMs && (s.every_n <= 1 || count % s.every_n === 0)) return t;
        }
        return null;
      }
      /**
       * "Mon 6 Oct 09:00 UTC" — schedules are evaluated in UTC, so they are shown in UTC.
       * @param {number} ms
       * @returns {string}
       */
      function triageFormatNext(ms) {
        const d = new Date(ms);
        const wd = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][d.getUTCDay()];
        const mo = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"][d.getUTCMonth()];
        const hh = String(d.getUTCHours()).padStart(2, "0"), mm = String(d.getUTCMinutes()).padStart(2, "0");
        return `${wd} ${d.getUTCDate()} ${mo} ${hh}:${mm} UTC`;
      }
      /**
       * Compact time-until: "in 2d 4h", "in 3h 5m", "in 4m", "now".
       * @param {number} ms
       * @param {number} nowMs
       * @returns {string}
       */
      function triageFormatIn(ms, nowMs) {
        const m = Math.round((ms - nowMs) / 60000);
        if (m < 1) return "now";
        if (m < 60) return `in ${m}m`;
        const h = Math.floor(m / 60);
        if (h < 24) return `in ${h}h${m % 60 ? ` ${m % 60}m` : ""}`;
        const d = Math.floor(h / 24);
        return `in ${d}d${h % 24 ? ` ${h % 24}h` : ""}`;
      }
      /**
       * What a draft threshold would mean for a pool. Saving a threshold only
       * stores it — a pool at or over it drains when one of its cells next
       * finishes (or by Drain now) — so the text says that, not "drains now".
       * @param {number} count - cells currently pooled
       * @param {string} raw - the threshold field's text
       * @param {number} scheduleCount - cron schedules the draft keeps
       * @returns {{ok: boolean, text: string}}
       */
      function triageThresholdEffect(count, raw, scheduleCount) {
        const s = raw.trim();
        if (s === "") {
          return {
            ok: true,
            text: scheduleCount
              ? "No count trigger — only a cron schedule (or Drain now) drains this pool."
              : "No count trigger and no schedule — nothing will drain this pool automatically.",
          };
        }
        if (!/^\d+$/.test(s) || Number(s) < 1) return { ok: false, text: "Must be a whole number of 1 or more, or blank." };
        const t = Number(s);
        const batches = Math.floor(count / t);
        return {
          ok: true,
          text: batches
            ? `${count} pooled is already ${batches} full ${batches === 1 ? "batch" : "batches"} — ${batches === 1 ? "it drains" : "they drain"} as ${batches === 1 ? "a review" : "reviews"} when a cell in this pool next finishes, or use Drain now.`
            : `Drains once ${t} cells are pooled (${count} now).`,
        };
      }
      /**
       * The triage type an Arbiter review was drained from, read off its
       * name (`triage-<type>` or `triage-<type>-<subproject>`); the longest
       * registered type that fits wins, so a type containing "-" still resolves.
       * @param {string} name
       * @param {string[]} typeNames
       * @returns {string|null}
       */
      function triageDrainedType(name, typeNames) {
        if (!name.startsWith("triage-")) return null;
        const rest = name.slice("triage-".length);
        const fits = typeNames.filter((t) => rest === t || rest.startsWith(t + "-"));
        return fits.sort((a, b) => b.length - a.length)[0] ?? null;
      }
      // RALPHUS-TRIAGE-FLOW-LOGIC:END

      /**
       * Polls the Triage endpoints (types, pools, schedules, candidates) and
       * re-renders the tab. The drained-review list is only re-fetched when
       * its section is open.
       * @returns {Promise<void>}
       */
      async function pollTriage() {
        try {
          const [typesResp, poolsResp, schedulesResp, candidatesResp] = await Promise.all([
            fetch("/api/triage/types"),
            fetch("/api/triage/pools"),
            fetch("/api/triage/schedules"),
            fetch("/api/triage/candidates"),
          ]);
          const [typesData, poolsData, schedulesData, candidatesData] = await Promise.all([
            typesResp.json(),
            poolsResp.json(),
            schedulesResp.json(),
            candidatesResp.json(),
          ]);
          byId("conn").className = "dot on";
          markUpdated();
          triageTypes = (typesData.types || []).slice().sort(
            (/** @type {TriageTypeView} */ a, /** @type {TriageTypeView} */ b) => a.name.localeCompare(b.name));
          triagePools = poolsData.pools || [];
          triageSchedules = schedulesData.schedules || [];
          triageCandidates = candidatesData.candidates || [];
          if (triageDrainedOpen) await loadTriageDrained();
          if (!triageSelKey || !triagePools.some((p) => triagePreviewKey(p.project, p.triage_type) === triageSelKey)) {
            const first = triageVisiblePools()[0];
            triageSelKey = first ? triagePreviewKey(first.project, first.triage_type) : null;
          }
          renderTriage();
        } catch (e) { markUnreachable(); }
      }
      /**
       * Fetches the Arbiter-created reviews for the Drained section. Lazy: only
       * called once that collapsed-by-default section is opened, and on each
       * poll while it stays open.
       * @returns {Promise<void>}
       */
      async function loadTriageDrained() {
        try {
          const r = await fetch("/api/guardian-index");
          const all = /** @type {GuardianIndexEntry[]} */ (r.ok ? await r.json() : []);
          triageDrained = all.filter((g) => g.origin === "arbiter");
        } catch (e) { triageDrained = []; }
      }

      /**
       * The cron schedules on one exact pool key.
       * @param {TriagePoolView} p
       * @returns {TriageScheduleView[]}
       */
      function triageSchedulesFor(p) {
        return triageSchedules.filter((s) => s.project === p.project && s.triage_type === p.triage_type);
      }
      /**
       * Pool math for one pool, with its own schedule count.
       * @param {TriagePoolView} p
       * @returns {TriagePoolMath}
       */
      function triageMath(p) { return triagePoolMath(p, triageSchedulesFor(p).length); }
      /**
       * Opted-in cells of one pool key with the given candidate status.
       * @param {TriagePoolView} p
       * @param {"scheduled"|"queued"|"failed"} status
       * @returns {TriageCandidateView[]}
       */
      function triageCandidatesFor(p, status) {
        return triageCandidates.filter((c) => c.project === p.project && c.triage_types.includes(p.triage_type) && c.status === status);
      }
      /**
       * Whether a candidate matches the toolbar's name search.
       * @param {TriageCandidateView} c
       * @returns {boolean}
       */
      function triageMatchesQuery(c) {
        const q = triageQuery.trim().toLowerCase();
        if (!q) return true;
        return `${c.task_name} ${c.cell_id} ${c.cell_name || ""} ${c.squad_label || ""}`.toLowerCase().includes(q);
      }
      /**
       * Whether a pool key falls inside the rail's current project/subproject focus.
       * @param {string} key
       * @returns {boolean}
       */
      function triageInFocus(key) {
        if (triageFocus.proj === null) return true;
        const { proj, sub } = triageSplitKey(key);
        if (proj !== triageFocus.proj) return false;
        return triageFocus.sub === null || (sub ?? "") === triageFocus.sub;
      }
      /**
       * Pools passing the type filter and name search, ignoring the project
       * focus — the rail keeps listing every project.
       * @returns {TriagePoolView[]}
       */
      function triageFilteredPools() {
        return triagePools.filter((p) => !triageHiddenTypes.has(p.triage_type)
          && (!triageQuery.trim() || triageCandidatesFor(p, "queued").some(triageMatchesQuery)));
      }
      /**
       * Pools the flow actually shows: filtered, then narrowed to the focus.
       * @returns {TriagePoolView[]}
       */
      function triageVisiblePools() { return triageFilteredPools().filter((p) => triageInFocus(p.project)); }
      /**
       * Groups pools project -> subproject ("" for the project's own pool).
       * @param {TriagePoolView[]} pools
       * @returns {Map<string, Map<string, TriagePoolView[]>>}
       */
      function triageTree(pools) {
        /** @type {Map<string, Map<string, TriagePoolView[]>>} */
        const tree = new Map();
        for (const p of pools) {
          const { proj, sub } = triageSplitKey(p.project);
          if (!tree.has(proj)) tree.set(proj, new Map());
          const subs = /** @type {Map<string, TriagePoolView[]>} */ (tree.get(proj));
          const sk = sub ?? "";
          if (!subs.has(sk)) subs.set(sk, []);
          /** @type {TriagePoolView[]} */ (subs.get(sk)).push(p);
        }
        return tree;
      }
      /**
       * Whether a project's pools use subproject keying at all.
       * @param {Map<string, TriagePoolView[]>} subs
       * @returns {boolean}
       */
      function triageHasSubs(subs) { return subs.size > 1 || (subs.size === 1 && !subs.has("")); }
      /**
       * Totals across a set of pools for the rail and the project headers.
       * @param {TriagePoolView[]} pools
       * @returns {{cells: number, pools: number, ready: number, stalled: number}}
       */
      function triageRollup(pools) {
        let cells = 0, ready = 0, stalled = 0;
        for (const p of pools) {
          const m = triageMath(p);
          cells += p.count;
          if (m.isReady) ready++;
          if (m.stalled) stalled++;
        }
        return { cells, pools: pools.length, ready, stalled };
      }

      /**
       * Renders the whole Triage tab: toolbar state, summary line, and the
       * Flow or Configuration body. Edit-mode drafts are read back first so a
       * periodic re-render never loses what is being typed.
       * @returns {void}
       */
      function renderTriage() {
        readTriagePaneDraft();
        const flow = triageView === "flow";
        byId("triage-seg-flow").classList.toggle("on", flow);
        byId("triage-seg-cfg").classList.toggle("on", !flow);
        document.querySelectorAll("#triage-page .tri-flow-only").forEach((n) => {
          /** @type {HTMLElement} */ (n).style.display = flow ? "" : "none";
        });
        renderStatusDropdown("triage-type-filter", {
          id: "triage-type",
          label: "Type",
          mode: "multi",
          itemNoun: "triage type",
          options: triageTypes.map((t) => ({ value: t.name, label: t.label || t.name })),
          selected: new Set(triageTypes.map((t) => t.name).filter((n) => !triageHiddenTypes.has(n))),
          onToggle: triageToggleType,
          onAll: () => triageAllTypes(true),
          onNone: () => triageAllTypes(false),
          optionTip: (v) => `Show or hide the ${v} pools, and the cells and reviews of that type.`,
        });
        byId("triage-summary").innerHTML = flow ? triageSummaryHtml() : "";
        byId("triage-summary").style.display = flow ? "" : "none";
        const el = byId("triage");
        preserveUserState(el, () => {
          el.innerHTML = flow ? triageFlowPageHtml() : triageCfgHtml();
        });
        if (triagePaneEdit) triagePaneEditHints();
      }
      /**
       * The one muted summary line under the toolbar; every count follows the rail focus.
       * @returns {string}
       */
      function triageSummaryHtml() {
        const scoped = triagePools.filter((p) => triageInFocus(p.project) && !triageHiddenTypes.has(p.triage_type));
        const ready = scoped.filter((p) => triageMath(p).isReady);
        const reviews = ready.reduce((n, p) => n + triageMath(p).fullBatches, 0);
        const stalled = scoped.filter((p) => triageMath(p).stalled);
        const pooled = scoped.reduce((n, p) => n + p.count, 0);
        const now = Date.now();
        let next = null;
        for (const s of triageSchedules) {
          if (!triageInFocus(s.project) || triageHiddenTypes.has(s.triage_type)) continue;
          const t = triageScheduleNextFire(s, now);
          if (t !== null && (next === null || t < next)) next = t;
        }
        const scope = triageFocus.proj === null ? ""
          : triageFocus.proj + (triageFocus.sub ? ` :: ${triageFocus.sub}` : triageFocus.sub === "" ? " (no subproject)" : "");
        const sep = `<span class="tri-sep">·</span>`;
        return (scope
          ? `<span data-tip="The whole tab is narrowed to this project. Every count here describes only it."><b>${esc(scope)}</b></span>${sep}`
          : `<span><b>${triageTypes.length}</b> types</span>${sep}`)
          + `<span><b>${scoped.length}</b> pools</span>${sep}`
          + `<span><b>${pooled}</b> pooled ${pooled === 1 ? "cell" : "cells"}</span>${sep}`
          + `<span data-tip="Pools at or over their count threshold. They drain when one of their cells next finishes, or by Drain now."><b style="color:var(--done)">${ready.length}</b> at threshold${reviews ? ` (${reviews} ${reviews === 1 ? "review" : "reviews"})` : ""}</span>`
          + (stalled.length ? `${sep}<span data-tip="Pools with neither a count threshold nor a cron schedule. Nothing will drain these automatically."><b style="color:var(--ignored)">${stalled.length}</b> with no trigger</span>` : "")
          + sep + (next !== null
            ? `<span data-tip="The soonest cron drain in scope (UTC).">next scheduled drain ${esc(triageFormatNext(next))} (${esc(triageFormatIn(next, now))})</span>`
            : `<span data-tip="No cron schedule covers anything in scope.">no scheduled drain</span>`);
      }
      /**
       * Flow view: rail | flow | details pane.
       * @returns {string}
       */
      function triageFlowPageHtml() {
        const err = triageError ? `<div class="tri-err">${esc(triageError)}</div>` : "";
        return `<div class="tri-page">
            <aside class="tri-rail">${triageRailHtml()}</aside>
            <div class="tri-flow" id="triage-flow-scroll">${err}${triageFlowHtml()}</div>
            <aside class="tri-pane" id="triage-pane-scroll">${triagePaneHtml()}</aside>
          </div>`;
      }

      /**
       * The small state dot on rail rows: ready pools first, then pools with no trigger.
       * @param {{ready: number, stalled: number}} r
       * @returns {string}
       */
      function triageRailDot(r) {
        const cls = r.ready ? " ready" : r.stalled ? " stalled" : "";
        const tip = r.ready ? `${r.ready} pool(s) at threshold.` : r.stalled ? `${r.stalled} pool(s) with no trigger at all.` : "Nothing needs attention here.";
        return `<span class="tri-sdot tri-rail-dot${cls}" data-tip="${esc(tip)}"></span>`;
      }
      /**
       * The project rail: every project (and subproject) with pools, with counts.
       * @returns {string}
       */
      function triageRailHtml() {
        const pools = triageFilteredPools();
        const tree = triageTree(pools);
        const all = triageRollup(pools);
        let h = `<div class="tri-rail-label">Projects</div>`
          + `<div class="tri-rail-item all${triageFocus.proj === null ? " sel" : ""}" data-click="setTriageFocus" data-tip="Show every project's pools together.">`
          + `<span class="tri-ri-name">All projects</span><span class="tri-ri-n">${all.cells}</span>${triageRailDot(all)}</div>`;
        for (const proj of [...tree.keys()].sort()) {
          const subs = /** @type {Map<string, TriagePoolView[]>} */ (tree.get(proj));
          const r = triageRollup([...subs.values()].flat());
          const on = triageFocus.proj === proj;
          h += `<div class="tri-rail-item proj${on && triageFocus.sub === null ? " sel" : ""}" data-click="setTriageFocus" data-proj="${esc(proj)}" data-tip="Narrow the whole tab to this project — its pools, its cells in flight, and the summary counts.">`
            + `<span class="tri-ri-name">${esc(proj)}</span><span class="tri-ri-n">${r.cells}</span>${triageRailDot(r)}</div>`;
          if (!triageHasSubs(subs)) continue;
          for (const sk of [...subs.keys()].sort()) {
            const sr = triageRollup(/** @type {TriagePoolView[]} */ (subs.get(sk)));
            h += `<div class="tri-rail-item sub${sk === "" ? " root" : ""}${on && triageFocus.sub === sk ? " sel" : ""}" data-click="setTriageFocus" data-proj="${esc(proj)}" data-sub="${esc(sk)}" data-tip="${esc(sk === "" ? "Pools keyed to this project with no subproject." : `Pools keyed to the ${sk} subproject. A drain never crosses a subproject boundary.`)}">`
              + `<span class="tri-ri-name">${sk === "" ? "(no subproject)" : esc(sk)}</span><span class="tri-ri-n">${sr.cells}</span>${triageRailDot(sr)}</div>`;
          }
        }
        return h;
      }

      /**
       * A stage heading. A collapsible one (Drained, Excluded) is a toggle.
       * @param {string} title
       * @param {number|string} count
       * @param {string} note
       * @param {string} [toggle] - the stage name for `toggleTriageStage`, when collapsible
       * @param {boolean} [open]
       * @param {string} [tip]
       * @returns {string}
       */
      function triageStageHead(title, count, note, toggle, open, tip) {
        const attrs = toggle
          ? ` data-click="toggleTriageStage" data-stage="${toggle}" data-tip="${esc(`${tip || ""}\nClick to ${open ? "collapse" : "expand"}.`)}"`
          : (tip ? ` data-tip="${esc(tip)}"` : "");
        return `<div class="tri-stage-head${toggle ? " toggle" : ""}"${attrs}>`
          + (toggle ? `<span class="tri-chev">${open ? "▾" : "▸"}</span>` : "")
          + `<h2>${esc(title)}</h2><span class="tri-stage-count">${count}</span>`
          + (note ? `<span class="tri-stage-note">${esc(note)}</span>` : "")
          + `</div>`;
      }
      /**
       * The flow column: Pooled first (what needs a decision), then In flight,
       * then the collapsed-by-default Drained history, then Excluded.
       * @returns {string}
       */
      function triageFlowHtml() {
        const pools = triageVisiblePools();
        /**
         * @param {TriageCandidateView} c
         * @returns {boolean}
         */
        const shown = (c) => triageMatchesQuery(c) && triageInFocus(c.project) && c.triage_types.some((t) => !triageHiddenTypes.has(t));
        const inflight = triageCandidates.filter((c) => c.status === "scheduled" && shown(c));
        const excluded = triageCandidates.filter((c) => c.status === "failed" && shown(c));

        let h = `<section class="tri-stage s-pooled">`
          + triageStageHead("Pooled", pools.reduce((n, p) => n + p.count, 0), "Done and viable, waiting on a count threshold or a cron schedule — first to fire wins.")
          + (pools.length ? triagePooledHtml(pools) : `<div class="tri-empty">${triagePools.length ? "No pools match these filters." : "Nothing is pooled and no thresholds are configured yet."}</div>`)
          + `</section>`;

        h += `<section class="tri-stage s-inflight">`
          + triageStageHead("In flight", inflight.length, "Classified, but the cell hasn't finished — not counting toward any threshold yet.")
          + (inflight.length ? `<div class="tri-rows">${inflight.map((c) => triageCandRowHtml(c, false)).join("")}</div>` : `<div class="tri-empty">Nothing in flight.</div>`)
          + `</section>`;

        const typeNames = triageTypes.map((t) => t.name);
        const drained = (triageDrained || []).filter((g) => {
          const t = triageDrainedType(g.name, typeNames);
          return !t || !triageHiddenTypes.has(t);
        });
        h += `<section class="tri-stage s-drained">`
          + triageStageHead("Drained", triageDrained === null ? "" : drained.length, "", "drained", triageDrainedOpen,
            "Reviews the Arbiter created by draining a pool. Collapsed by default — this list only grows. Listed across every project.")
          + (triageDrainedOpen
            ? (triageDrained === null ? `<div class="tri-empty">Loading…</div>`
              : drained.length ? `<div class="tri-rows compact">${drained.map(triageDrainedRowHtml).join("")}</div>`
              : `<div class="tri-empty">Nothing drained yet.</div>`)
            : "")
          + `</section>`;

        if (triageShowExcluded) {
          h += `<section class="tri-stage s-excluded">`
            + triageStageHead("Excluded", excluded.length, "", "excluded", triageExcludedOpen,
              "Proof failed — permanently out of every pool, kept as a record.")
            + (triageExcludedOpen
              ? (excluded.length ? `<div class="tri-rows compact">${excluded.map((c) => triageCandRowHtml(c, true)).join("")}</div>` : `<div class="tri-empty">Nothing excluded.</div>`)
              : "")
            + `</section>`;
        }
        return h;
      }
      /**
       * Pooled stage body: project sections holding subproject sections of pool cards.
       * @param {TriagePoolView[]} pools
       * @returns {string}
       */
      function triagePooledHtml(pools) {
        const tree = triageTree(pools);
        /**
         * @param {string} sk
         * @param {TriagePoolView[]} list
         * @param {boolean} header
         * @returns {string}
         */
        const subSection = (sk, list, header) => {
          const r = triageRollup(list);
          return `<div class="tri-sub">`
            + (header ? `<div class="tri-sub-head"><span class="tri-sub-name${sk === "" ? " root" : ""}" data-tip="${esc(sk === "" ? "Pools keyed to this project with no subproject." : `The ${sk} subproject. A drain never crosses this boundary.`)}">${sk === "" ? "(no subproject)" : esc(sk)}</span>`
              + `<span class="tri-sub-roll">${r.cells} ${r.cells === 1 ? "cell" : "cells"} · ${r.pools} ${r.pools === 1 ? "pool" : "pools"}${r.ready ? ` · ${r.ready} ready` : ""}</span></div>` : "")
            + `<div class="tri-pools">${list.map(triagePoolCardHtml).join("")}</div></div>`;
        };
        if (triageFocus.proj !== null) {
          const subs = tree.get(triageFocus.proj);
          if (!subs) return "";
          const header = triageHasSubs(subs) && triageFocus.sub === null;
          return [...subs.keys()].sort().map((sk) => subSection(sk, /** @type {TriagePoolView[]} */ (subs.get(sk)), header)).join("");
        }
        return [...tree.keys()].sort().map((proj) => {
          const subs = /** @type {Map<string, TriagePoolView[]>} */ (tree.get(proj));
          const r = triageRollup([...subs.values()].flat());
          const collapsed = triageCollapsedProjects.has(proj);
          const hasSubs = triageHasSubs(subs);
          return `<section class="tri-proj">`
            + `<div class="tri-proj-head" data-click="toggleTriageProject" data-proj="${esc(proj)}" data-tip="Every pool keyed to this project. Click to collapse it; pick it in the rail to narrow the whole tab to it.">`
            + `<span class="tri-chev">${collapsed ? "▸" : "▾"}</span><span class="tri-proj-name">${esc(proj)}</span>`
            + (hasSubs ? `<span class="badge" data-tip="This project uses subproject keying (RAL-346), so its pools are split further.">${subs.size} subprojects</span>` : "")
            + `<span class="tri-proj-roll"><span><b>${r.cells}</b> ${r.cells === 1 ? "cell" : "cells"}</span><span><b>${r.pools}</b> ${r.pools === 1 ? "pool" : "pools"}</span>`
            + (r.ready ? `<span style="color:var(--done)">${r.ready} ready</span>` : "")
            + (r.stalled ? `<span style="color:var(--ignored)">${r.stalled} no trigger</span>` : "")
            + `</span></div>`
            + (collapsed ? "" : `<div class="tri-proj-body">${[...subs.keys()].sort().map((sk) => subSection(sk, /** @type {TriagePoolView[]} */ (subs.get(sk)), hasSubs)).join("")}</div>`)
            + `</section>`;
        }).join("");
      }
      /**
       * "4 pooled · 6 ready" — a zero part is left out. Ready cells fill
       * complete threshold batches; pooled cells are the remainder.
       * @param {TriagePoolView} p
       * @returns {string}
       */
      function triageCountWordsHtml(p) {
        const m = triageMath(p);
        const parts = [];
        if (m.remainder || !m.ready) parts.push(`<b>${m.remainder}</b> pooled`);
        if (m.ready) parts.push(`<b>${m.ready}</b> ready`);
        return `<span data-tip="Pooled cells are waiting for a trigger. Ready cells fill complete batches of the count threshold; each batch becomes its own review when the pool drains.">${parts.join(`<span class="tri-sep">·</span>`)}</span>`;
      }
      /**
       * One pool card: state dot, type, count in words, and its cron line(s).
       * Click selects it; right-click opens its menu (edit triggers, drain).
       * @param {TriagePoolView} p
       * @returns {string}
       */
      function triagePoolCardHtml(p) {
        const m = triageMath(p);
        const key = triagePreviewKey(p.project, p.triage_type);
        const dot = m.isReady
          ? `<span class="tri-sdot ready" data-tip="At or over its count threshold — drains when one of its cells next finishes, or by Drain now."></span>`
          : m.stalled
            ? `<span class="tri-sdot stalled" data-tip="No trigger — no count threshold and no cron schedule, so nothing will drain this pool automatically. Right-click to edit its triggers."></span>`
            : `<span class="tri-sdot filling" data-tip="Filling toward its next trigger."></span>`;
        return `<div class="tri-pool${key === triageSelKey ? " sel" : ""}" data-click="selectTriagePool" data-project="${esc(p.project)}" data-triage-type="${esc(p.triage_type)}" oncontextmenu="openTriagePoolMenu(event, this)" data-tip="${esc(`Pool (${p.project}, ${p.triage_type}).\nClick to open it in the details pane; right-click to edit its triggers or drain it.`)}">`
          + `<div class="tri-pool-head">${dot}<span class="tri-type">${esc(p.triage_type)}</span><span class="tri-pool-count">${triageCountWordsHtml(p)}</span></div>`
          + triageTriggersHtml(p, false)
          + `</div>`;
      }
      /**
       * Read-only trigger rows. A card shows only its cron line(s) — a count
       * line would restate the card's corner; the pane lists count and cron,
       * each cron as just its next firing time.
       * @param {TriagePoolView} p
       * @param {boolean} pane
       * @returns {string}
       */
      function triageTriggersHtml(p, pane) {
        const m = triageMath(p);
        const scheds = triageSchedulesFor(p);
        const now = Date.now();
        const out = [];
        if (pane) {
          let cls = "", val = "", tip = "";
          if (m.threshold === null) {
            cls = " off"; val = "not set";
            tip = "No count threshold is set for this pool. Only a cron schedule, or Drain now, can empty it.";
          } else if (m.isReady) {
            cls = " firing"; val = `every ${m.threshold} · ${m.fullBatches} ${m.fullBatches === 1 ? "review" : "reviews"} ready`;
            tip = `At or over its threshold. When a cell in this pool next finishes, it drains in batches of ${m.threshold} — each full batch becomes its own review, and any remainder stays pooled.`;
          } else {
            val = `every ${m.threshold} · ${m.threshold - p.count} more`;
            tip = `Drains once this pool holds ${m.threshold} viable cells, in batches of ${m.threshold}. A cell whose proof failed never counts toward this.`;
          }
          out.push(`<div class="tri-trig${cls}" data-tip="${esc(tip)}"><span class="tri-tk">count</span><span class="tri-tv">${esc(val)}</span></div>`);
        }
        if (!scheds.length) {
          out.push(`<div class="tri-trig ${m.threshold === null ? "missing" : "off"}" data-tip="${esc(`No cron schedule is registered for this exact pool key.${m.threshold === null ? " With no threshold either, nothing will ever drain this pool automatically." : " The count threshold is this pool's only automatic trigger."}`)}"><span class="tri-tk">cron</span><span class="tri-tv">none</span></div>`);
        }
        for (const s of scheds) {
          const next = triageScheduleNextFire(s, now);
          const nextText = next === null ? "—" : (pane ? `next ${triageFormatNext(next)}` : triageFormatIn(next, now));
          const val = pane ? esc(nextText) : `<span class="mono">${esc(s.cron_expr)}</span>${s.every_n > 1 ? ` ×${s.every_n}` : ""} · ${esc(nextText)}`;
          const tip = `${s.cron_expr} (UTC)${s.every_n > 1 ? `, every ${s.every_n}th occurrence counted from ${new Date(s.anchor_date_ms).toISOString().slice(0, 10)}` : ""}.`
            + (next === null ? "" : `\nNext firing: ${triageFormatNext(next)} (${triageFormatIn(next, now)}).`)
            + "\nA cron firing drains this key's whole pool into one review. First trigger to fire wins.";
          out.push(`<div class="tri-trig" data-tip="${esc(tip)}"><span class="tri-tk">cron</span><span class="tri-tv">${val}</span></div>`);
        }
        return `<div class="tri-triggers">${out.join("")}</div>`;
      }
      /**
       * One in-flight or excluded cell row in the flow; click jumps to it on the Squads tab.
       * @param {TriageCandidateView} c
       * @param {boolean} failed
       * @returns {string}
       */
      function triageCandRowHtml(c, failed) {
        const cell = c.cell_name ? `${c.cell_id} · ${c.cell_name}` : c.cell_id;
        const badge = failed
          ? `<span class="badge" style="color:var(--failed);border-color:var(--failed)" data-tip="This cell's proof failed, so it never counts toward a threshold and is never swept into a review. It stays listed as a record.">✗ excluded</span>`
          : `<span class="badge" data-tip="Classified, but the cell hasn't finished, so it isn't counting toward its pool's threshold yet.">⏳ in flight</span>`;
        return `<div class="tri-r${failed ? " excl" : ""}" data-click="gotoTriageCandidate" data-squad-id="${esc(c.squad_id)}" data-ti="${c.task_idx}" data-si="${c.cell_idx}" data-tip="Jump to the Squads tab and select this cell.">`
          + `<div class="tri-r-main"><div class="tri-r-name">${esc(c.task_name)}</div>`
          + `<div class="tri-r-sub mono">${esc(c.squad_id)} · ${esc(cell)}${c.squad_label ? ` · ${esc(c.squad_label)}` : ""}</div></div>`
          + `<div class="tri-r-right">${triageFocus.sub === null ? `<span class="tri-proj-label">${esc(c.project)}</span>` : ""}`
          + `<span class="tri-type">${c.triage_types.map(esc).join(", ")}</span>${badge}</div></div>`;
      }
      /**
       * One drained (Arbiter-created) review row; click opens it on the Reviews tab.
       * @param {GuardianIndexEntry} g
       * @returns {string}
       */
      function triageDrainedRowHtml(g) {
        return `<div class="tri-r" data-click="gotoReview" data-guardian-id="${esc(g.id)}" data-tip="Open this review on the Reviews tab.">`
          + `<div class="tri-r-main"><div class="tri-r-name mono">${esc(g.name)}</div>`
          + `<div class="tri-r-sub">${g.branch_count} ${g.branch_count === 1 ? "branch" : "branches"}</div></div>`
          + `<div class="tri-r-right">${gdot(g.status)}<span class="tri-r-status">${esc(g.status)}</span></div></div>`;
      }

      /**
       * One cell row in the details pane — the Squads tab's kv-row + Go shape.
       * @param {TriageCandidateView} c
       * @param {string} tip
       * @returns {string}
       */
      function triageCellRowHtml(c, tip) {
        return `<div class="kv-row"><span class="v" style="flex:1;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap">${esc(c.task_name)}</span>`
          + `<span class="k mono" style="min-width:0">${esc(c.cell_name || c.cell_id)}</span>`
          + `<button class="btn tri-go" data-click="gotoTriageCandidate" data-squad-id="${esc(c.squad_id)}" data-ti="${c.task_idx}" data-si="${c.cell_idx}" data-tip="${esc(tip)}">Go</button></div>`;
      }
      /**
       * The details pane for the selected pool: read view with ✎ Edit, or edit mode.
       * @returns {string}
       */
      function triagePaneHtml() {
        const p = triagePools.find((x) => triagePreviewKey(x.project, x.triage_type) === triageSelKey);
        if (!p) return `<div class="tri-empty">Select a pool.</div>`;
        if (triagePaneEdit && triagePaneEdit.key !== triageSelKey) triagePaneEdit = null;
        const m = triageMath(p);
        const state = m.isReady ? `<span class="tri-sdot ready"></span>ready to drain`
          : m.stalled ? `<span class="tri-sdot stalled"></span>no trigger`
          : `<span class="tri-sdot filling"></span>filling`;
        let h = `<div class="tri-pane-title"><span class="tri-type tri-pt">${esc(p.triage_type)}</span><span class="tri-pane-state">${state}</span></div>`
          + `<div class="tri-proj-label" style="margin-bottom:6px">${esc(p.project)}</div>`
          + `<div class="tri-pane-count">${triageCountWordsHtml(p)}</div>`;
        if (triageError) h += `<div class="tri-err">${esc(triageError)}</div>`;
        if (triagePaneEdit) return h + triagePaneEditFormHtml();

        const queued = triageCandidatesFor(p, "queued");
        const inflight = triageCandidatesFor(p, "scheduled");
        h += `<h3 class="section">Triggers</h3>${triageTriggersHtml(p, true)}`;
        h += `<h3 class="section">Pooled cells <span style="color:var(--faint)">${queued.length}</span></h3>`
          + (queued.length ? queued.map((c) => triageCellRowHtml(c, "Jump to this cell on the Squads tab — selects its squad, task, and cell.")).join("")
            : `<div class="kv-row"><span class="v" style="color:var(--muted)">None</span></div>`);
        if (inflight.length) {
          h += `<h3 class="section">In flight <span style="color:var(--faint)">${inflight.length}</span></h3>`
            + inflight.map((c) => triageCellRowHtml(c, "Jump to this cell on the Squads tab. It hasn't finished, so it isn't counting toward the threshold yet.")).join("");
        }
        const confirm = triageDrainConfirms[triagePreviewKey(p.project, p.triage_type)];
        h += `<div class="btn-row">`
          + `<button class="btn primary" onclick="startTriagePaneEdit()" data-tip="Edit this pool's triggers — its count threshold and every cron schedule, all at once.\nNothing changes until you Save.">✎ Edit</button>`
          + `<button class="btn"${p.count > 0 ? "" : " disabled"} data-click="requestDrainTriagePool" data-project="${esc(p.project)}" data-triage-type="${esc(p.triage_type)}" data-tip="Force-create one review right now from every eligible candidate in this pool.\nBypasses only the count threshold; no cron schedule is needed. You confirm before anything happens.">Drain now</button>`
          + `</div>${confirm ? triageDrainConfirmLine(confirm) : ""}`;
        return h;
      }

      /**
       * Edit mode's form: the count threshold plus one block per cron schedule.
       * Inputs carry ids so `preserveUserState` keeps focus across a poll.
       * @returns {string}
       */
      function triagePaneEditFormHtml() {
        const e = /** @type {TriagePaneEditDraft} */ (triagePaneEdit);
        let h = `<h3 class="section">Count threshold</h3>`
          + `<div class="tri-tedit" onkeydown="triagePaneEditKey(event)">`
          + `<div class="tri-tedit-row"><label for="tri-pe-n">Every</label>`
          + `<input id="tri-pe-n" class="tri-n mono" type="number" min="1" step="1" placeholder="none" value="${esc(e.threshold)}" oninput="triagePaneEditHints()" data-tip="Drain once this many viable cells are pooled, in threshold-sized batches — each full batch becomes its own review.\nLeave blank for no count trigger." />`
          + `<span class="tri-hint">cells</span></div>`
          + `<div class="tri-hint" id="tri-pe-n-hint"></div></div>`
          + `<h3 class="section">Cron schedules</h3>`;
        if (!e.scheds.length) h += `<div class="tri-hint" style="margin-bottom:6px">No schedules.</div>`;
        e.scheds.forEach((d, i) => {
          h += `<div class="tri-tedit tri-pe-sched" data-i="${i}" onkeydown="triagePaneEditKey(event)">`
            + `<div class="tri-tedit-row"><label for="tri-pe-cron-${i}">Cron</label>`
            + `<input id="tri-pe-cron-${i}" class="tri-cron mono tri-pe-cron" placeholder="0 0 9 * * Mon" value="${esc(d.cron)}" oninput="triagePaneEditHints()" data-tip="Cron expression in UTC with a leading seconds field: sec min hour day month weekday (weekday 1-7 from Sunday, or Sun-Sat)." />`
            + `<button class="btn danger tri-sm" style="margin-left:auto" onclick="triagePaneEditRemoveSched(${i})" data-tip="Remove this schedule. It's only deleted when you Save — Cancel keeps it.">Remove</button></div>`
            + `<div class="tri-tedit-row"><label for="tri-pe-every-${i}">Every</label>`
            + `<input id="tri-pe-every-${i}" class="tri-n mono tri-pe-every" type="number" min="1" value="${esc(d.every)}" oninput="triagePaneEditHints()" data-tip="Fire on every Nth occurrence of the expression, counted from the anchor — 2 with a weekly expression means every other week." />`
            + `<span class="tri-hint">occurrence</span></div>`
            + `<div class="tri-tedit-row"><label for="tri-pe-anchor-${i}">Anchor</label>`
            + `<input id="tri-pe-anchor-${i}" class="tri-pe-anchor" type="date" value="${esc(d.anchor)}" data-tip="Reference date the every-Nth count is measured from. Stored as UTC." /></div>`
            + `<div class="tri-hint tri-pe-hint"></div></div>`;
        });
        h += `<button class="btn tri-sm" onclick="triagePaneEditAddSched()" data-tip="Add a cron drain schedule for this exact pool key. Every schedule races the others and the count threshold; first to fire wins.">＋ Add schedule</button>`
          + `<div class="btn-row">`
          + `<button class="btn primary" onclick="saveTriagePaneEdit()" data-tip="Save every trigger change at once.\nAn edited schedule is replaced (removed and re-added), so its fired count starts over. A removed schedule cannot be brought back; this cannot be undone.">Save</button>`
          + `<button class="btn" onclick="cancelTriagePaneEdit()" data-tip="Discard these edits and leave edit mode. Nothing was changed.">Cancel</button></div>`;
        return h;
      }
      /**
       * Enters edit mode for the selected pool: every trigger becomes editable at once.
       * @returns {void}
       */
      function startTriagePaneEdit() {
        const p = triagePools.find((x) => triagePreviewKey(x.project, x.triage_type) === triageSelKey);
        if (!p) return;
        closeTriagePoolMenu();
        triageError = "";
        triagePaneEdit = {
          key: triagePreviewKey(p.project, p.triage_type),
          threshold: p.threshold === null || p.threshold === undefined ? "" : String(p.threshold),
          scheds: triageSchedulesFor(p).map((s) => ({
            id: s.id, cron: s.cron_expr, every: String(s.every_n),
            anchor: new Date(s.anchor_date_ms).toISOString().slice(0, 10),
          })),
        };
        renderTriage();
        const first = document.getElementById("tri-pe-n");
        if (first) first.focus();
      }
      /**
       * Leaves edit mode without saving.
       * @returns {void}
       */
      function cancelTriagePaneEdit() { triagePaneEdit = null; triageError = ""; renderTriage(); }
      /**
       * Copies the edit form's current values into the draft, so a re-render keeps them.
       * @returns {void}
       */
      function readTriagePaneDraft() {
        const e = triagePaneEdit;
        if (!e) return;
        const n = /** @type {HTMLInputElement|null} */ (document.getElementById("tri-pe-n"));
        if (!n) return;
        e.threshold = n.value;
        document.querySelectorAll(".tri-pe-sched").forEach((row) => {
          const d = e.scheds[Number(row.getAttribute("data-i"))];
          if (!d) return;
          d.cron = /** @type {HTMLInputElement} */ (row.querySelector(".tri-pe-cron")).value;
          d.every = /** @type {HTMLInputElement} */ (row.querySelector(".tri-pe-every")).value;
          d.anchor = /** @type {HTMLInputElement} */ (row.querySelector(".tri-pe-anchor")).value;
        });
      }
      /**
       * Adds a blank schedule block to the draft.
       * @returns {void}
       */
      function triagePaneEditAddSched() {
        if (!triagePaneEdit) return;
        readTriagePaneDraft();
        triagePaneEdit.scheds.push({ id: null, cron: "", every: "1", anchor: new Date().toISOString().slice(0, 10) });
        const i = triagePaneEdit.scheds.length - 1;
        renderTriage();
        const el = document.getElementById(`tri-pe-cron-${i}`);
        if (el) el.focus();
      }
      /**
       * Drops one schedule block from the draft (deleted for real only on Save).
       * @param {number} i
       * @returns {void}
       */
      function triagePaneEditRemoveSched(i) {
        if (!triagePaneEdit) return;
        readTriagePaneDraft();
        triagePaneEdit.scheds.splice(i, 1);
        renderTriage();
      }
      /**
       * Refreshes every hint in the edit form in place; true when the draft is valid.
       * @returns {boolean}
       */
      function triagePaneEditHints() {
        const e = triagePaneEdit;
        if (!e) return false;
        readTriagePaneDraft();
        const p = triagePools.find((x) => triagePreviewKey(x.project, x.triage_type) === e.key);
        let ok = true;
        const eff = triageThresholdEffect(p ? p.count : 0, e.threshold, e.scheds.length);
        const nh = document.getElementById("tri-pe-n-hint");
        if (nh) { nh.textContent = eff.text; nh.classList.toggle("bad", !eff.ok); }
        if (!eff.ok) ok = false;
        document.querySelectorAll(".tri-pe-sched").forEach((row) => {
          const d = e.scheds[Number(row.getAttribute("data-i"))];
          if (!d) return;
          const err = triageCronError(d.cron) || (/^\d+$/.test(d.every.trim()) && Number(d.every) >= 1 ? "" : "Every must be a whole number of 1 or more.")
            || (d.anchor ? "" : "Pick an anchor date.");
          const h = /** @type {HTMLElement} */ (row.querySelector(".tri-pe-hint"));
          if (err) { h.textContent = err; h.classList.add("bad"); ok = false; return; }
          const spec = /** @type {TriageCronSpec} */ (triageParseCron(d.cron));
          const next = triageCronNextAfter(spec, Date.now());
          h.textContent = next === null ? "Never fires within ten years." : `Fires in UTC — first match after now: ${triageFormatNext(next)}.`;
          h.classList.remove("bad");
        });
        return ok;
      }
      /**
       * Enter saves, Escape cancels — anywhere in the edit form.
       * @param {KeyboardEvent} ev
       * @returns {void}
       */
      function triagePaneEditKey(ev) {
        if (ev.key === "Enter") { ev.preventDefault(); void saveTriagePaneEdit(); }
        if (ev.key === "Escape") { ev.preventDefault(); cancelTriagePaneEdit(); }
      }
      /**
       * Applies the whole draft: the threshold, then removed, edited
       * (remove + re-add — the API has no in-place update), and new schedules.
       * Stops at the first failure and keeps edit mode open with the error.
       * @returns {Promise<void>}
       */
      async function saveTriagePaneEdit() {
        const e = triagePaneEdit;
        if (!e || !triagePaneEditHints()) return;
        const p = triagePools.find((x) => triagePreviewKey(x.project, x.triage_type) === e.key);
        if (!p) return;
        /**
         * @param {string} url
         * @param {string} method
         * @param {object} [body]
         * @returns {Promise<Response>}
         */
        const send = (url, method, body) => fetch(url, body === undefined ? { method } : {
          method, headers: { "Content-Type": "application/json" }, body: JSON.stringify(body),
        });
        const existing = triageSchedulesFor(p);
        try {
          const raw = e.threshold.trim();
          const threshold = raw === "" ? null : Number(raw);
          if (threshold !== (p.threshold ?? null)) {
            const r = await send("/api/triage/pools/threshold", "POST", { project: p.project, triage_type: p.triage_type, threshold });
            if (!r.ok) throw new Error(await responseError(r, "saving the threshold failed"));
          }
          for (const s of existing) {
            const d = e.scheds.find((x) => x.id === s.id);
            const anchorMs = d ? Date.parse(`${d.anchor}T00:00:00Z`) : 0;
            const changed = d && (d.cron.trim().split(/\s+/).join(" ") !== s.cron_expr || Number(d.every) !== s.every_n || anchorMs !== s.anchor_date_ms);
            if (d && !changed) continue;
            const r = await send(`/api/triage/schedules/${encodeURIComponent(String(s.id))}`, "DELETE");
            if (!r.ok) throw new Error(await responseError(r, "removing a schedule failed"));
            if (d) d.id = null;
          }
          for (const d of e.scheds) {
            if (d.id !== null) continue;
            const r = await send("/api/triage/schedules", "POST", {
              project: p.project, triage_type: p.triage_type, cron_expr: d.cron.trim().split(/\s+/).join(" "),
              anchor_date_ms: Date.parse(`${d.anchor}T00:00:00Z`), every_n: Number(d.every),
            });
            if (!r.ok) throw new Error(await responseError(r, "adding a schedule failed"));
          }
          triagePaneEdit = null;
          triageError = "";
        } catch (err) {
          triageError = err instanceof Error && err.message !== "Failed to fetch" ? err.message : "daemon unreachable";
        }
        await pollTriage();
      }
      /**
       * Opens a pool's triggers in the details pane's edit mode, from anywhere
       * (the card menu, or a Configuration-tab schedule row).
       * @param {string} project
       * @param {string} triageType
       * @returns {void}
       */
      function editTriagePoolTriggers(project, triageType) {
        if (!triagePools.some((p) => p.project === project && p.triage_type === triageType)) return;
        triageView = "flow";
        triageSelKey = triagePreviewKey(project, triageType);
        triagePaneEdit = null;
        if (!triageInFocus(project)) triageFocus = { proj: null, sub: null };
        startTriagePaneEdit();
      }

      /**
       * Right-click menu on a pool card: open, edit triggers, drain.
       * @param {MouseEvent} e
       * @param {HTMLElement} el - the card, carrying data-project / data-triage-type
       * @returns {void}
       */
      function openTriagePoolMenu(e, el) {
        e.preventDefault();
        e.stopPropagation();
        closeTriagePoolMenu();
        const project = el.dataset.project || "", triageType = el.dataset.triageType || "";
        const pool = triagePools.find((p) => p.project === project && p.triage_type === triageType);
        if (!pool) return;
        const attrs = `data-project="${esc(project)}" data-triage-type="${esc(triageType)}"`;
        const menu = document.createElement("div");
        menu.className = "ctx-menu";
        menu.id = "triage-pool-menu";
        menu.innerHTML = `<div class="ctx-group">${esc(triageType)} · ${esc(project)}</div>`
          + `<div data-click="selectTriagePool" ${attrs} data-tip="Show this pool in the details pane.">Open in details</div>`
          + `<div data-click="editTriagePoolTriggers" ${attrs} data-tip="Open this pool in the details pane in edit mode — its count threshold and every cron schedule, editable together.">✎ Edit triggers…</div>`
          + `<div class="ctx-sep"></div>`
          + `<div ${pool.count > 0 ? `data-click="requestDrainTriagePool" ${attrs}` : `class="ctx-disabled"`} data-tip="Force-create one review right now from every eligible candidate in this pool. Bypasses only the count threshold; you confirm in the details pane first.">Drain now…</div>`;
        document.body.appendChild(menu);
        const r = menu.getBoundingClientRect();
        menu.style.left = Math.min(e.clientX, window.innerWidth - r.width - 8) + "px";
        menu.style.top = Math.min(e.clientY, window.innerHeight - r.height - 8) + "px";
      }
      /**
       * Closes the pool card menu, if open.
       * @returns {void}
       */
      function closeTriagePoolMenu() { const m = document.getElementById("triage-pool-menu"); if (m) m.remove(); }
      document.addEventListener("click", () => closeTriagePoolMenu());

      /**
       * Switches between the Flow and Configuration views.
       * @param {"flow"|"cfg"} v
       * @returns {void}
       */
      function setTriageView(v) { triageView = v; triageError = ""; renderTriage(); }
      /**
       * Toolbar name search.
       * @param {string} v
       * @returns {void}
       */
      function onTriageQuery(v) { triageQuery = v; renderTriage(); }
      /**
       * Toolbar "show excluded" checkbox.
       * @param {boolean} on
       * @returns {void}
       */
      function onTriageShowExcluded(on) { triageShowExcluded = on; renderTriage(); }
      /**
       * Narrows the tab to a project (and optionally one subproject); null for every project.
       * @param {string|null} proj
       * @param {string|null} sub - "" is the project's own (no-subproject) pool
       * @returns {void}
       */
      function setTriageFocus(proj, sub) {
        triageFocus = { proj, sub };
        const sel = triagePools.find((p) => triagePreviewKey(p.project, p.triage_type) === triageSelKey);
        if (!sel || !triageInFocus(sel.project)) {
          const first = triageVisiblePools()[0];
          triageSelKey = first ? triagePreviewKey(first.project, first.triage_type) : null;
          triagePaneEdit = null;
        }
        renderTriage();
      }
      /**
       * Collapses or expands one project section of the overview.
       * @param {string} proj
       * @returns {void}
       */
      function toggleTriageProject(proj) {
        if (triageCollapsedProjects.has(proj)) triageCollapsedProjects.delete(proj); else triageCollapsedProjects.add(proj);
        renderTriage();
      }
      /**
       * Selects a pool into the details pane (leaving edit mode for a different pool).
       * @param {string} project
       * @param {string} triageType
       * @returns {void}
       */
      function selectTriagePool(project, triageType) {
        const k = triagePreviewKey(project, triageType);
        if (k !== triageSelKey) triagePaneEdit = null;
        triageSelKey = k;
        closeTriagePoolMenu();
        renderTriage();
      }
      /**
       * Expands or collapses Drained / Excluded. Opening Drained fetches its reviews.
       * @param {string} which
       * @returns {Promise<void>}
       */
      async function toggleTriageStage(which) {
        if (which === "excluded") { triageExcludedOpen = !triageExcludedOpen; renderTriage(); return; }
        triageDrainedOpen = !triageDrainedOpen;
        if (!triageDrainedOpen) { triageDrained = null; renderTriage(); return; }
        renderTriage();
        await loadTriageDrained();
        renderTriage();
      }
      /**
       * Type dropdown: show or hide one triage type.
       * @param {string} name
       * @param {boolean} on
       * @returns {void}
       */
      function triageToggleType(name, on) {
        if (on) triageHiddenTypes.delete(name); else triageHiddenTypes.add(name);
        renderTriage();
      }
      /**
       * Type dropdown all/none.
       * @param {boolean} on
       * @returns {void}
       */
      function triageAllTypes(on) {
        triageHiddenTypes.clear();
        if (!on) triageTypes.forEach((t) => triageHiddenTypes.add(t.name));
        renderTriage();
      }

      /**
       * Configuration view: the type registry and every cron drain schedule.
       * @returns {string}
       */
      function triageCfgHtml() {
        const err = triageError ? `<div class="tri-err">${esc(triageError)}</div>` : "";
        const types = triageTypes.length ? triageTypes.map(triageTypeRowHtml).join("") : `<tr><td colspan="5" class="tri-empty">No Triage types registered yet.</td></tr>`;
        const scheds = triageSchedules.length ? triageSchedules.map(triageScheduleRowHtml).join("") : `<tr><td colspan="7" class="tri-empty">No schedules configured.</td></tr>`;
        return `<div class="tri-cfg">${err}
            <div class="tri-card">
              <h2>Triage types</h2>
              <p class="tri-card-note">A cell that opts in with <code>triage = true</code> is classified into one of these. Each description is fed to the Arbiter's classification prompt alongside every other type's — write it so the Arbiter can tell this type apart from the rest.</p>
              <table class="proj-table"><thead><tr>
                <th data-tip="The name a cell's inline triage_type value, or the Arbiter's own classification, resolves against.">Name</th>
                <th data-tip="Short human-readable label shown alongside the name.">Label</th>
                <th data-tip="Fed to the Arbiter's classification prompt.">Description</th>
                <th data-tip="When this type was registered.">Registered</th><th></th>
              </tr></thead><tbody>${types}</tbody></table>
              <div class="tri-form-row">
                <input id="triage-type-name" type="text" placeholder="name (e.g. security)" style="width:170px" class="mono" data-tip="Lowercase identifier a cell's triage_type resolves against. Re-using an existing name updates that type in place." />
                <input id="triage-type-label" type="text" placeholder="label" style="width:150px" data-tip="Short human-readable label shown beside the name." />
                <input id="triage-type-desc" type="text" placeholder="description the Arbiter classifies against" style="flex:1;min-width:240px" data-tip="Fed to the Arbiter's classification prompt. Be specific about what separates this type from the others." />
                <button class="btn primary" onclick="registerTriageType()" data-tip="Register this triage type, or update it if the name already exists.">Register</button>
              </div>
            </div>
            <div class="tri-card">
              <h2>Drain schedules</h2>
              <p class="tri-card-note">A cron trigger on one exact pool key. Firing drains that key's whole pool into a single review — unlike a count threshold, which drains in threshold-sized batches. Whichever trigger fires first wins.</p>
              <table class="proj-table"><thead><tr>
                <th data-tip="The pool key (project, optional subproject, and triage type) this schedule drains.">Pool</th>
                <th data-tip="Cron expression in UTC, with a leading seconds field.">Cron</th>
                <th data-tip="Fires on every Nth occurrence of the expression, counted from the anchor.">Every</th>
                <th data-tip="The reference date the every-N count is measured from.">Anchor</th>
                <th data-tip="When this schedule next drains its pool (UTC).">Next</th>
                <th data-tip="When the daemon last advanced this schedule.">Last checked</th><th></th>
              </tr></thead><tbody>${scheds}</tbody></table>
              <div class="tri-form-row">
                <input id="triage-sched-project" type="text" placeholder="project" style="width:140px" data-tip="Registered project name this schedule drains." />
                <input id="triage-sched-sub" type="text" placeholder="subproject (optional)" style="width:170px" data-tip="Only for a project that uses subproject keying (RAL-346): the subproject whose pool this schedule drains. Leave blank for the project's own pool." />
                <input id="triage-sched-type" type="text" placeholder="triage type" style="width:130px" data-tip="Must be a registered triage type." />
                <input id="triage-sched-cron" type="text" placeholder="0 0 9 * * Mon" style="width:160px" class="mono" data-tip="Cron expression in UTC with a leading seconds field: sec min hour day month weekday (weekday 1-7 from Sunday, or Sun-Sat)." />
                <input id="triage-sched-anchor" type="date" style="width:150px" data-tip="Reference date the every-Nth count is measured from. Stored as UTC." />
                <input id="triage-sched-every-n" type="number" min="1" step="1" value="1" style="width:80px" data-tip="Fire on every Nth occurrence of the expression, counted from the anchor — 1 means every occurrence." />
                <button class="btn primary" onclick="addTriageSchedule()" data-tip="Add this drain schedule. It races the pool's count threshold and any other schedule on the same key.">Add</button>
              </div>
            </div>
          </div>`;
      }
      /**
       * One registered triage type's table row.
       * @param {TriageTypeView} t
       * @returns {string}
       */
      function triageTypeRowHtml(t) {
        const action = t.name === "unclassified"
          ? `<span class="badge" data-tip="The built-in fallback type assigned when classification fails, times out, or is ambiguous (single attempt, no retry). It always exists and can never be removed.">built-in</span>`
          : `<button class="btn danger tri-sm" data-click="removeTriageType" data-name="${esc(t.name)}" data-tip="Deregister this triage type.\nCells already pooled or classified under it keep that assignment — only future classification is affected. This cannot be undone.">Remove</button>`;
        return `<tr><td><span class="tri-type">${esc(t.name)}</span></td><td>${esc(t.label || "—")}</td>`
          + `<td style="color:var(--muted);font-size:12px">${esc(t.description || "—")}</td>`
          + `<td style="color:var(--muted)">${fmtProjCreated(t.created_at_ms)}</td><td>${action}</td></tr>`;
      }
      /**
       * One cron schedule's table row, with Edit (opens the pool's edit mode) and Remove.
       * @param {TriageScheduleView} s
       * @returns {string}
       */
      function triageScheduleRowHtml(s) {
        const next = triageScheduleNextFire(s, Date.now());
        const hasPool = triagePools.some((p) => p.project === s.project && p.triage_type === s.triage_type);
        return `<tr><td><span class="tri-proj-label">${esc(s.project)}</span> <span class="tri-type">${esc(s.triage_type)}</span></td>`
          + `<td class="mono">${esc(s.cron_expr)}</td>`
          + `<td>${s.every_n > 1 ? `${s.every_n}th` : "each"}</td>`
          + `<td style="color:var(--muted)">${new Date(s.anchor_date_ms).toISOString().slice(0, 10)}</td>`
          + `<td>${next === null ? "—" : `${esc(triageFormatNext(next))} <span style="color:var(--muted)">${esc(triageFormatIn(next, Date.now()))}</span>`}</td>`
          + `<td style="color:var(--muted)">${s.last_checked_ms ? fmtProjCreated(s.last_checked_ms) : "never"}</td>`
          + `<td style="white-space:nowrap">`
          + (hasPool ? `<button class="btn tri-sm" data-click="editTriagePoolTriggers" data-project="${esc(s.project)}" data-triage-type="${esc(s.triage_type)}" data-tip="Open this schedule's pool in the Flow view's edit mode — its threshold and every schedule, editable together.">✎ Edit</button> ` : "")
          + `<button class="btn danger tri-sm" data-click="removeTriageSchedule" data-id="${s.id}" data-tip="Remove this drain schedule. Its pool keeps any count threshold it has.\nThis cannot be undone.">Remove</button></td></tr>`;
      }
      /**
       * Registers (or updates) a Triage type from the register form's fields.
       * @returns {Promise<void>}
       */
      async function registerTriageType() {
        const name = /** @type {HTMLInputElement} */ (byId("triage-type-name")).value.trim();
        const label = /** @type {HTMLInputElement} */ (byId("triage-type-label")).value.trim();
        const description = /** @type {HTMLInputElement} */ (byId("triage-type-desc")).value.trim();
        if (!name) { triageError = "Type name is required."; renderTriage(); return; }
        try {
          const r = await fetch("/api/triage/types", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ name, label, description }),
          });
          triageError = r.ok ? "" : await responseError(r, "register failed");
        } catch (e) { triageError = "daemon unreachable"; }
        await pollTriage();
      }
      /**
       * Deregisters a Triage type. The daemon refuses this for the built-in "unclassified" type.
       * @param {string} name
       * @returns {Promise<void>}
       */
      async function removeTriageType(name) {
        if (!confirm(`Deregister Triage type "${name}"?\n\nCells already pooled or classified under it keep that assignment -- only future classification/validation against this name is affected.`)) return;
        try {
          const r = await fetch(`/api/triage/types/${encodeURIComponent(name)}`, { method: "DELETE" });
          triageError = r.ok ? "" : await responseError(r, "remove failed");
        } catch (e) { triageError = "daemon unreachable"; }
        await pollTriage();
      }
      /**
       * Adds a cron schedule from the Configuration form; a subproject joins the
       * project as the composite `project::subproject` pool key.
       * @returns {Promise<void>}
       */
      async function addTriageSchedule() {
        const proj = /** @type {HTMLInputElement} */ (byId("triage-sched-project")).value.trim();
        const sub = /** @type {HTMLInputElement} */ (byId("triage-sched-sub")).value.trim();
        const triageType = /** @type {HTMLInputElement} */ (byId("triage-sched-type")).value.trim();
        const cronExpr = /** @type {HTMLInputElement} */ (byId("triage-sched-cron")).value.trim().split(/\s+/).join(" ");
        const anchorDate = /** @type {HTMLInputElement} */ (byId("triage-sched-anchor")).value;
        const everyNRaw = /** @type {HTMLInputElement} */ (byId("triage-sched-every-n")).value.trim();
        if (!proj || !triageType || !cronExpr || !anchorDate) {
          triageError = "Project, triage type, cron expression, and anchor date are all required.";
          renderTriage();
          return;
        }
        const cronErr = triageCronError(cronExpr);
        if (cronErr) { triageError = cronErr; renderTriage(); return; }
        try {
          const r = await fetch("/api/triage/schedules", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({
              project: sub ? `${proj}::${sub}` : proj, triage_type: triageType, cron_expr: cronExpr,
              anchor_date_ms: Date.parse(`${anchorDate}T00:00:00Z`), every_n: everyNRaw === "" ? 1 : Number(everyNRaw),
            }),
          });
          triageError = r.ok ? "" : await responseError(r, "add schedule failed");
        } catch (e) { triageError = "daemon unreachable"; }
        await pollTriage();
      }
      /**
       * Removes a configured cron schedule entry.
       * @param {string} id
       * @returns {Promise<void>}
       */
      async function removeTriageSchedule(id) {
        if (!confirm("Remove this Triage schedule?\n\nThe pool's count threshold (if any) and any other schedules on this key are unaffected.")) return;
        try {
          const r = await fetch(`/api/triage/schedules/${encodeURIComponent(id)}`, { method: "DELETE" });
          triageError = r.ok ? "" : await responseError(r, "remove failed");
        } catch (e) { triageError = "daemon unreachable"; }
        await pollTriage();
      }

      // RALPHUS-TRIAGE-TAB-HANDLERS:BEGIN
      /**
       * RAL-449: the inline Confirm/Cancel line shown under the details pane's
       * "Drain now" button once it has been clicked. There is nothing to fetch
       * first -- draining now always takes every currently eligible
       * candidate, so the line is built from the pool's already-known state.
       * @param {TriagePoolDrainConfirm} confirm
       * @returns {string}
       */
      function triageDrainConfirmLine(confirm) {
        const thresholdText = confirm.threshold === null || confirm.threshold === undefined
          ? "no threshold is configured"
          : `the configured threshold of ${confirm.threshold}`;
        return `<div class="row" style="gap:4px;margin:4px 0 0;max-width:440px" data-tip="Bypasses only this pool's count threshold -- every other review-creation invariant (Arbiter origin, project defaults, only completed cells) stays the same as an automatic drain, and no cron schedule is needed.">
            <span style="color:var(--muted);font-size:11px">Create 1 review from ${confirm.count} candidate(s) now, bypassing ${thresholdText}.</span>
            <button class="btn primary" style="padding:2px 8px;font-size:11px" data-click="confirmDrainTriagePool" data-project="${esc(confirm.project)}" data-triage-type="${esc(confirm.triage_type)}" data-tip="Create the review now.">Confirm & drain</button>
            <button class="btn" style="padding:2px 8px;font-size:11px" data-click="cancelDrainTriagePool" data-project="${esc(confirm.project)}" data-triage-type="${esc(confirm.triage_type)}" data-tip="Discard; nothing was changed.">Cancel</button>
          </div>`;
      }
      /**
       * Shows the Confirm/Cancel line for a pool's "Drain now" (RAL-449),
       * captured from the pool's current state at click time, and selects
       * that pool so the line is visible in the details pane.
       * @param {MouseEvent} e
       * @param {string} project
       * @param {string} triageType
       * @returns {void}
       */
      function requestDrainTriagePool(e, project, triageType) {
        const pool = triagePools.find((p) => p.project === project && p.triage_type === triageType);
        if (!pool || pool.count <= 0) return;
        triageSelKey = triagePreviewKey(project, triageType);
        triageDrainConfirms[triagePreviewKey(project, triageType)] = {
          project, triage_type: triageType, count: pool.count, threshold: pool.threshold,
        };
        renderTriage();
      }
      /**
       * Force-creates a review from every eligible candidate currently in
       * this pool, bypassing its configured count threshold and without
       * requiring a cron schedule (RAL-449).
       * @param {string} project
       * @param {string} triageType
       * @returns {Promise<void>}
       */
      async function confirmDrainTriagePool(project, triageType) {
        const key = triagePreviewKey(project, triageType);
        if (!triageDrainConfirms[key]) return;
        try {
          const r = await fetch("/api/triage/pools/drain", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ project, triage_type: triageType }),
          });
          triageError = r.ok ? "" : await responseError(r, "drain failed");
        } catch (err) { triageError = "daemon unreachable"; }
        delete triageDrainConfirms[key];
        await pollTriage();
      }
      /**
       * Discards one pool's pending "Drain now" confirmation without changing anything (RAL-449).
       * @param {string} project
       * @param {string} triageType
       * @returns {void}
       */
      function cancelDrainTriagePool(project, triageType) {
        delete triageDrainConfirms[triagePreviewKey(project, triageType)];
        renderTriage();
      }
      // RALPHUS-TRIAGE-TAB-HANDLERS:END
