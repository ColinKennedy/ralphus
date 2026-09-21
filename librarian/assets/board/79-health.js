      // ---------- Health tab (RAL-416) ----------
      // Catalog-driven view of every check `ralphus_core::health_catalog`
      // knows about, joined against two live sources: this daemon's own
      // cached hourly Free-tier sweep (GET /api/health/report) and every
      // configured [machine.targets.*] entry's always-live, on-demand
      // report (GET /api/machines/targets/health -- unchanged by RAL-416,
      // reused as-is). Admin-only (ADMIN_ONLY_TABS in 10-tab-registry.js),
      // same convention as Machines/Triage/Projects/Users/Secrets.
      //
      // "Check now" only exists for this daemon's own row
      // (POST /api/health/report/refresh) -- there is deliberately no
      // remote-target counterpart here: /api/machines/targets/health is
      // already always computed live (never cached), so the tab's own
      // "↻ Refresh" button re-fetching it *is* that tab's "check now" for
      // every remote row. A dedicated per-target check-now action would
      // need its own API shape decision RAL-416 left open -- see this
      // ticket's own report.

      /** @type {HealthCatalogEntry[]} */
      let healthCatalog = [];
      /** @type {Map<string, HealthCatalogEntry>} */
      let healthCatalogById = new Map();
      /** @type {HealthSweepReport|null} */
      let healthLocalReport = null;
      /** @type {TargetHealthReport[]} */
      let healthRemoteTargets = [];
      /** Set while a POST is in flight, so the button can't be double-clicked. */
      let healthRefreshing = false;
      /** Board-local error string for the Health tab's own fetch/refresh failures. */
      let healthError = "";

      /**
       * Fetches the catalog, this daemon's cached sweep report, and every
       * remote target's always-live report, then re-renders. Same
       * connection-dot/timestamp convention as every other polled tab.
       * @returns {Promise<void>}
       */
      async function pollHealth() {
        try {
          const [catalogRes, reportRes, targetsRes] = await Promise.all([
            fetch("/api/health/catalog"),
            fetch("/api/health/report"),
            fetch("/api/machines/targets/health"),
          ]);
          if (!catalogRes.ok || !reportRes.ok || !targetsRes.ok) {
            healthError = await responseError(
              !catalogRes.ok ? catalogRes : !reportRes.ok ? reportRes : targetsRes,
              "health fetch failed",
            );
          } else {
            healthError = "";
          }
          const catalogBody = catalogRes.ok ? await catalogRes.json() : { catalog: [] };
          const reportBody = reportRes.ok ? await reportRes.json() : null;
          const targetsBody = targetsRes.ok ? await targetsRes.json() : { targets: [] };
          healthCatalog = catalogBody.catalog || [];
          healthCatalogById = new Map(healthCatalog.map((e) => [e.id, e]));
          healthLocalReport = reportBody;
          healthRemoteTargets = targetsBody.targets || [];
          byId("conn").className = "dot on";
          markUpdated();
          renderHealthPage();
        } catch (e) { markUnreachable(); }
      }
      /**
       * Re-runs this daemon's own Free-tier sweep immediately
       * (POST /api/health/report/refresh) instead of waiting for the next
       * hourly pass, then re-polls the whole tab.
       * @returns {Promise<void>}
       */
      async function refreshHealthNow() {
        if (healthRefreshing) return;
        healthRefreshing = true;
        renderHealthPage();
        try {
          const r = await fetch("/api/health/report/refresh", { method: "POST" });
          healthError = r.ok ? "" : await responseError(r, "check now failed");
        } catch (e) { healthError = "daemon unreachable"; }
        healthRefreshing = false;
        await pollHealth();
      }
      /**
       * Status dot + label for one check's `pass`/`warn`/`fail`/`skip`
       * status -- `--done`/`--warn`/`--failed`/`--muted` per docs/colors.md
       * (`--warn` is the documented "Cartographer warning-level" hue,
       * reused here since RAL-416 checks use the identical vocabulary;
       * `--muted` is "not applicable", matching MachineProviderView's own
       * "not checked" convention just above).
       * @param {string} status
       * @returns {string}
       */
      function healthStatusBadge(status) {
        const color = status === "pass" ? "--done" : status === "warn" ? "--warn" : status === "skip" ? "--muted" : "--failed";
        return `<span class="dot" style="background:${cvar(color)}"></span> ${esc(status)}`;
      }
      /**
       * Renders one check row, joining a live status/detail against its
       * catalog entry (when known -- a check id the board hasn't fetched a
       * matching catalog entry for yet degrades to showing its raw id
       * rather than failing to render).
       * @param {{id: string, status: string, detail: string}} check
       * @returns {string}
       */
      function healthCheckRowHtml(check) {
        const entry = healthCatalogById.get(check.id);
        const label = entry ? entry.label : check.id;
        const meta = entry
          ? `${esc(entry.requirement)} · ${esc(entry.cost_tier)}`
          : "unrecognized catalog id";
        const impact = entry ? entry.impact : "";
        const remediation = entry ? entry.remediation : "";
        const copyPayload = [
          `${label} (${check.id}): ${check.status}`,
          check.detail,
          impact ? `Impact: ${impact}` : "",
          remediation ? `Remediation: ${remediation}` : "",
        ].filter(Boolean).join("\n");
        return `<tr>
            <td>${healthStatusBadge(check.status)}</td>
            <td><span class="mono" data-tip="catalog id: ${esc(check.id)}\n${esc(meta)}">${esc(label)}</span></td>
            <td style="max-width:420px;overflow:hidden;text-overflow:ellipsis" data-tip="${esc(check.detail)}">${esc(check.detail)}</td>
            <td style="max-width:320px;overflow:hidden;text-overflow:ellipsis" data-tip="${esc(impact)}\n\nRemediation: ${esc(remediation)}">${esc(impact)}</td>
            <td>${copyBtn(copyPayload)}</td>
          </tr>`;
      }
      /**
       * Renders one group of checks (this daemon's local row, or one
       * remote target) as its own labeled table.
       * @param {string} title
       * @param {string} tip
       * @param {{id: string, status: string, detail: string}[]} checks
       * @param {number|null} [checkedAtMs]
       * @returns {string}
       */
      function healthGroupHtml(title, tip, checks, checkedAtMs) {
        const when = checkedAtMs ? ` <span style="color:var(--muted)">(checked ${esc(fmtProjCreated(checkedAtMs))})</span>` : checkedAtMs === null ? ` <span style="color:var(--muted)">(not checked yet)</span>` : "";
        const rows = checks.length
          ? checks.map(healthCheckRowHtml).join("")
          : `<tr><td colspan="5" class="empty">No checks.</td></tr>`;
        return `<div style="margin-bottom:16px">
            <div class="row" style="justify-content:space-between;align-items:baseline">
              <b data-tip="${esc(tip)}">${esc(title)}</b>${when}
            </div>
            <table class="proj-table"><thead><tr>
              <th>Status</th><th>Check</th><th>Detail</th><th>Impact</th><th></th>
            </tr></thead><tbody>${rows}</tbody></table>
          </div>`;
      }
      /**
       * Renders the Health tab: this daemon's synthetic local row (with its
       * own "Check now" action) followed by one group per configured
       * [machine.targets.*] entry, or an empty state when neither exists.
       * @returns {void}
       */
      function renderHealthPage() {
        const parts = [];
        if (healthError) {
          parts.push(`<div class="empty" style="color:var(--failed)">${esc(healthError)}</div>`);
        }
        parts.push(`<div class="row" style="justify-content:space-between;margin-bottom:8px">
            <span style="color:var(--muted)" data-tip="This daemon's own Free-tier checks refresh automatically on an hourly background sweep (configurable via [health].poll_interval_secs). Remote targets below are always checked live -- the tab's own Refresh button re-checks them.">Local sweep is hourly by default</span>
            <button class="btn" ${healthRefreshing ? "disabled" : ""} onclick="refreshHealthNow()" data-tip="Re-run this daemon's own Free-tier checks immediately instead of waiting for the next hourly sweep.">${healthRefreshing ? "Checking…" : "⟳ Check now (local)"}</button>
          </div>`);
        const localMachine = healthLocalReport ? healthLocalReport.machine : "daemon (local)";
        parts.push(healthGroupHtml(
          localMachine,
          "This daemon's own host -- a synthetic row alongside real [machine.targets.*] rows below, so both render in one list.",
          (healthLocalReport && healthLocalReport.checks) || [],
          healthLocalReport ? healthLocalReport.checked_at_ms : null,
        ));
        for (const t of healthRemoteTargets) {
          parts.push(healthGroupHtml(
            `${t.target} (${t.machine})`,
            "A configured [machine.targets.*] entry -- always checked live when this tab loads or you hit Refresh, never cached.",
            t.checks || [],
            undefined,
          ));
        }
        byId("health").innerHTML = parts.join("");
      }
