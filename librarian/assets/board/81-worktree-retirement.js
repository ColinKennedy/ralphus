      // ---------- Worktree retirement tab (RAL-385, RAL-386) ----------
      // Originally a popup reachable from the Reviews tab; moved to its own
      // admin-only tab (ADMIN_ONLY_TABS in 25-chrome.js) so it follows the
      // same gated-tab pattern as Machines/Triage/Projects/Users/Secrets
      // instead of a one-off modal. Deliberately NOT gated server-side the
      // way those tabs are (see `crate::server::require_admin`'s callers)
      // -- this view is read-only and every user can already pull the
      // identical data with `ralphus review worktree-retirements`, so
      // hiding it here is a UI convenience, not an attempt to restrict who
      // can see it.

      // States in lifecycle order; RETIREMENT_STATE_COLORS maps each to a
      // documented board color variable (docs/colors.md) for its status dot.
      // RAL-386 added deferred/opted_out for machine-provider-backed
      // worktrees: --waiting (deferred, "held back, try again later" --
      // same hue as a scheduler down-time hold) and --muted (opted_out, a
      // deliberate neutral decision, not a warning) keep both visually
      // distinct from the red --failed they are not.
      const RETIREMENT_STATES = ["scheduled", "eligible", "claimed", "failed", "deferred", "opted_out", "retired"];
      /** @type {{[state: string]: string}} */
      const RETIREMENT_STATE_COLORS = { scheduled:"--pending", eligible:"--queued", claimed:"--ignored", failed:"--failed", deferred:"--waiting", opted_out:"--muted", retired:"--cancelled" };
      /** @type {WorktreeRetirementEntry[]} rows from the last poll */
      let retirementEntries = [];
      /** @type {string[]} active state filter chips (empty = every state) */
      let retirementStateFilters = [];
      /**
       * Polls `/api/worktree-retirements` for the Worktree retirement tab
       * and re-renders it. Same connection-dot/timestamp convention as
       * every other polled tab (`pollTriage`, `pollProjects`, ...).
       * @returns {Promise<void>}
       */
      async function pollWorktreeRetirements() {
        try {
          const d = await (await fetch("/api/worktree-retirements")).json();
          retirementEntries = d.entries || [];
          byId("conn").className = "dot on";
          markUpdated();
          renderWorktreeRetirementPage();
        } catch (e) { markUnreachable(); }
      }
      /**
       * Renders one retirement state's dot + label, using a documented color
       * variable per state (docs/colors.md: scheduled=grey, eligible=purple
       * like queued work, claimed=amber caution, failed=red, deferred=pink
       * like a scheduler down-time hold, opted_out=neutral grey, retired=grey).
       * @param {string} s
       * @returns {string}
       */
      function retirementStateBadge(s) {
        const color = Object.prototype.hasOwnProperty.call(RETIREMENT_STATE_COLORS, s) ? RETIREMENT_STATE_COLORS[s] : "--muted";
        return `<span class="dot" style="background:${cvar(color)}"></span> ${esc(s)}`;
      }
      /**
       * Toggles one state filter chip on the Worktree retirement tab and re-renders.
       * @param {string} s
       * @returns {void}
       */
      function toggleRetirementState(s) {
        retirementStateFilters = retirementStateFilters.includes(s)
          ? retirementStateFilters.filter((f) => f !== s)
          : [...retirementStateFilters, s];
        renderWorktreeRetirementPage();
      }
      /**
       * Renders the Worktree retirement tab: filter chips over
       * RETIREMENT_STATES, then one row per entry matching the active
       * filters -- state badge, review, worktree path, eligible-at (or last
       * attempt) time, and a detail column carrying the blocking claim, the
       * failure error, or (RAL-386) a machine provider's deferred/opted-out
       * reason.
       * @returns {void}
       */
      function renderWorktreeRetirementPage() {
        const rows = retirementEntries
          .filter((e) => retirementStateFilters.length === 0 || retirementStateFilters.includes(e.state))
          .slice()
          .sort((a, b) => a.eligible_at_ms - b.eligible_at_ms || a.path.localeCompare(b.path));
        const chips = RETIREMENT_STATES.map((s) => {
          const active = retirementStateFilters.includes(s);
          return `<label data-tip="Filter the list to worktrees in the '${esc(s)}' state. Click again to include it in every-state view.">
              <input type="checkbox" ${active ? "checked" : ""} onchange="toggleRetirementState('${esc(s)}')"> ${retirementStateBadge(s)}
            </label>`;
        }).join("");
        const rowsHtml = rows.length
          ? rows.map((e) => {
              const when = e.last_attempt_ms ?? e.eligible_at_ms;
              // RAL-386: `error` is now also populated for `deferred`/
              // `opted_out`, neither of which is a failure -- branch on
              // `e.state` first so those two never pick up the red
              // --failed styling or "cleanup attempt failed" wording a
              // genuine `failed` row gets.
              const retryHint = e.retry_at_ms
                ? ` (retry hint: ${esc(new Date(e.retry_at_ms).toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" }))})`
                : "";
              const detail = e.state === "failed"
                ? `<span style="color:var(--failed)" data-tip="The last retirement attempt failed with this error. The daily sweep retries automatically; keep an eye on it or investigate manually.">${esc(e.error)}</span>`
                : e.state === "deferred"
                ? `<span style="color:var(--waiting)" data-tip="The machine provider asked to try again later instead of retiring this worktree now. Not a failure -- the daily sweep still retries on its normal cadence regardless of any retry hint.">${esc(e.error ?? "deferred")}${retryHint}</span>`
                : e.state === "opted_out"
                ? `<span style="color:var(--muted)" data-tip="A machine provider, or an operator's static machine retirement policy, declined to ever retire this worktree automatically. Not a failure -- nobody is trying, by design.">${esc(e.error ?? "opted out")}</span>`
                : e.claim_kind
                ? `<span data-tip="This worktree is over 30 days old but is still claimed by a non-terminal ${esc(e.claim_kind)} (${esc(e.claim_state ?? "")}). The daemon keeps it and has raised a mailbox escalation instead of deleting it.">${esc(e.claim_kind)}: ${esc(e.claim_owner ?? "")} (${esc(e.claim_state ?? "")})</span>`
                : e.state === "retired"
                ? `<span style="color:var(--muted)" data-tip="This worktree was removed on its retirement attempt. The row stays visible for as long as the review exists.">removed ${esc(new Date(when).toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" }))}</span>`
                : e.state === "eligible"
                ? `<span style="color:var(--muted)" data-tip="Old enough to retire. Anyone watching this review already got one mailbox heads-up; the sweep after that notice removes it, unless it becomes claimed again first.">${esc(new Date(e.eligible_at_ms).toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" }))}</span>`
                : `<span style="color:var(--muted)" data-tip="When the worktree becomes old enough to retire (not yet -- 'scheduled' means it is not old enough yet).">${esc(new Date(e.eligible_at_ms).toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" }))}</span>`;
              return `<tr>
                  <td>${retirementStateBadge(e.state)}</td>
                  <td class="mono">${esc(e.guardian_name)}</td>
                  <td class="mono" style="max-width:340px;overflow:hidden;text-overflow:ellipsis" data-tip="${esc(e.path)}">${esc(e.path)}</td>
                  <td>${detail}</td>
                </tr>`;
            }).join("")
          : `<tr><td colspan="4" class="empty">No review worktrees${retirementStateFilters.length ? " in the selected states" : ""}.</td></tr>`;
        byId("worktree-retirement").innerHTML = `
          <div class="status-filters row" id="retirement-state-filters">${chips}</div>
          <table class="proj-table"><thead><tr>
            <th data-tip="The worktree's retirement state.">State</th>
            <th data-tip="The review the worktree belongs to.">Review</th>
            <th data-tip="The worktree path on disk (or the removed path, for retired rows).">Path</th>
            <th data-tip="scheduled/eligible: when the worktree becomes (or became) eligible. claimed: the claim holding it. failed: why the attempt failed. deferred: a machine provider's reason for trying again later. opted_out: why it is never retired automatically. retired: when it was removed.">Detail</th>
          </tr></thead><tbody>${rowsHtml}</tbody></table>`;
      }
