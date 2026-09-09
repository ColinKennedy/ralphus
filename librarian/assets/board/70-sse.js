      // ---------- polling ----------
      // RAL-7: a periodic/pushed refresh re-renders panes via `el.innerHTML = …`, which resets
      // scroll position and wipes in-progress text in a pane's inputs. Captures
      // the user's scroll position and focused field beforehand and restores them
      // after render — live updates keep flowing without clobbering typing.
      /**
       * Re-renders `el` via `render()` while preserving scroll position and focused-input state (RAL-7).
       * @param {HTMLElement|null} el
       * @param {() => void} render
       * @returns {void}
       */
      function preserveUserState(el, render) {
        if (!el) { render(); return; }
        const active = document.activeElement;
        const focused = /** @type {HTMLInputElement|HTMLTextAreaElement|null} */ (active && el.contains(active) && active.id ? active : null);
        const fId = focused ? focused.id : null;
        const fVal = focused && "value" in focused ? focused.value : null;
        const fStart = focused && "selectionStart" in focused ? focused.selectionStart : null;
        const fEnd = focused && "selectionEnd" in focused ? focused.selectionEnd : null;
        // `el` survives the innerHTML swap (only its children are replaced), so
        // keep the ref; descendant scrollers are recreated, so re-find by id.
        /** @type {[Element|string, number][]} */
        const scrolls = [];
        if (el.scrollTop) scrolls.push([el, el.scrollTop]);
        el.querySelectorAll("[id]").forEach((n) => { if (n.scrollTop) scrolls.push([n.id, n.scrollTop]); });
        render();
        for (const [ref, top] of scrolls) {
          const n = typeof ref === "string" ? document.getElementById(ref) : ref;
          if (n) n.scrollTop = top;
        }
        if (fId) {
          const n = /** @type {HTMLInputElement|HTMLTextAreaElement|null} */ (document.getElementById(fId));
          if (n) {
            if (fVal != null && "value" in n) n.value = fVal;
            n.focus({ preventScroll: true });
            if (fStart != null && n.setSelectionRange) { try { n.setSelectionRange(fStart, fEnd ?? fStart); } catch (_) {} }
          }
        }
      }
      // RAL-7 (follow-up): an innerHTML swap also destroys an active text
      // selection — highlighting a read-only worktree path or prompt text gets
      // wiped every tick. While the user has a live selection (a DOM range, or a
      // non-collapsed range inside a focused input/textarea) anywhere on the
      // page, skip the periodic re-renders entirely so the selection survives;
      // the next tick after they release it refreshes normally.
      /**
       * Checks whether the user currently has an active text/DOM selection, so periodic re-renders can skip and avoid wiping it.
       * @returns {boolean}
       */
      function userIsSelecting() {
        const a = /** @type {HTMLInputElement|HTMLTextAreaElement|Element|null} */ (document.activeElement);
        if (a && "selectionStart" in a && a.selectionStart != null && a.selectionStart !== a.selectionEnd) return true;
        const s = window.getSelection && window.getSelection();
        return !!(s && !s.isCollapsed && String(s).length);
      }
      /**
       * Re-renders the sidebar, graph, and details pane.
       * @returns {void}
       */
      function renderAll() { renderSquads(); renderGraph(); preserveUserState(document.getElementById("details"), renderDetails); }
      /**
       * Formats the daemon status counter text, showing "unlimited" in place
       * of the cap when `maxConcurrent` is 0 (no limit).
       * @param {number} running
       * @param {number} maxConcurrent
       * @returns {string}
       */
      function formatConcurrencyStatus(running, maxConcurrent) {
        const cap = maxConcurrent === 0 ? "unlimited" : String(maxConcurrent);
        return `Running ${running} / ${cap}`;
      }
      // Fetch /api/tasks for the daemon status block only — updates the "Running X / Y"
      // counter and the `squads` cache (used by the running dropdown) without triggering
      // task-view rendering. Called by tick() on non-task tabs so the counter stays
      // accurate regardless of which tab is active.
      // RALPHUS-TASKS-POLL-SEQ:BEGIN
      /**
       * Monotonic sequence shared by `updateCounter`/`pollTasks` (RAL-390) --
       * each call captures its number at start; only the call still holding
       * the latest ticket when its data lands is allowed to write
       * `squads`/the daemon-status counter or render, regardless of which
       * fetch resolves first. Same latest-wins pattern `reviewPollSeq`
       * already proved out for `pollReviews` (RAL-382).
       */
      let tasksPollSeq = 0;
      /**
       * The in-flight `/api/tasks` request `updateCounter`/`pollTasks`
       * currently share (RAL-390 follow-up) -- an overlapping caller
       * piggybacks on this instead of firing its own duplicate fetch.
       * `/api/tasks` can take multiple seconds against a large squad
       * history, and squad-state SSE events fire faster than that; without
       * sharing the request, every poll queued behind the flood and the
       * ticket guard alone made every response land stale, so nothing ever
       * rendered.
       * @type {Promise<any>|null}
       */
      let tasksFetchInFlight = null;
      /**
       * Fetches and parses `/api/tasks`, reusing the current in-flight
       * request if one is already running instead of starting a duplicate.
       * @returns {Promise<any>}
       */
      function fetchTasksShared() {
        if (!tasksFetchInFlight) {
          tasksFetchInFlight = (async () => {
            try {
              return await (await fetch("/api/tasks")).json();
            } finally {
              tasksFetchInFlight = null;
            }
          })();
        }
        return tasksFetchInFlight;
      }
      // RALPHUS-TASKS-POLL-SEQ:END
      // RALPHUS-UPDATE-COUNTER:BEGIN
      /**
       * Fetches /api/tasks for the daemon status counter and `squads` cache, without triggering task-view rendering.
       * RAL-390: shares one in-flight request and a monotonic ticket with `pollTasks` -- see the `tasksPollSeq` declaration above.
       * @returns {Promise<void>}
       */
      async function updateCounter() {
        const seq = ++tasksPollSeq;
        try {
          const d = await fetchTasksShared();
          if (seq !== tasksPollSeq) return; // superseded -- a newer poll's data wins
          /** @type {any} */ (window)._daemonStatus = d.daemon;
          squads = d.squads || [];
          byId("running").textContent = formatConcurrencyStatus(d.daemon.running ?? 0, d.daemon.max_concurrent ?? 0);
        } catch (_) {
          if (seq !== tasksPollSeq) return;
          byId("conn").className = "dot off";
          byId("updated").textContent = "daemon unreachable";
        }
      }
      // RALPHUS-UPDATE-COUNTER:END
      /**
       * Polls whichever data the active tab needs and refreshes the ready-review banner.
       * RAL-167: this is now driven primarily by SSE (see `connectEventStream`/
       * `scheduleSseRefresh` near the bottom of this script), not a fixed
       * interval -- called directly on tab switches/hash navigation, on an
       * incoming push event (debounced), and by a much slower reconciliation
       * fallback. Live-terminal peek panes are refreshed on their own separate
       * interval (`pollOpenPeeks` isn't Cartographer-backed, so push can't
       * drive it) rather than from here.
       * @returns {Promise<void>}
       */
      async function tick() {
        await pollWhoAmI();
        await pollHidden();
        await pollWatches();
        if (tab === "reviews") { await updateCounter(); await pollReviews(); }
        else if (tab === "resources") { await updateCounter(); await pollResources(); }
        else if (tab === "queue") { await updateCounter(); if (queueUI.autoUpdate || !queueLoaded) await pollQueue(); }
        else if (tab === "cartographer") { await updateCounter(); await pollCartographer(); }
        else if (tab === "projects") { await updateCounter(); await pollProjects(); }
        else if (tab === "machines") { await updateCounter(); await pollMachines(); }
        else if (tab === "triage") { await updateCounter(); await pollTriage(); }
        else if (tab === "users") { await updateCounter(); await pollUsers(); }
        else if (tab === "secrets") { await updateCounter(); await pollSecretEnvNames(); }
        else if (tab === "prefs") { await updateCounter(); await pollPrefs(); }
        else if (tab === "tasks") { await updateCounter(); await pollTasksTab(); }
        else await pollTasks();
        await refreshBanner();
      }

      // ---- SSE push (RAL-167) ----
      // Replaces the old fixed `setInterval(tick, 2000)` as the primary
      // live-update trigger: the daemon emits one event per Cartographer
      // write (see `daemon/src/events.rs`) over `GET /api/events`, the
      // librarian proxies it straight through, and this connects to it. A
      // burst of events (e.g. every cell in a squad finishing near-together)
      // is coalesced into a single trailing-edge refresh via
      // `scheduleSseRefresh`. A much slower 60s `setInterval(tick, ...)`
      // stays as a reconciliation fallback for a missed/dropped event -- see
      // the bottom of this script.
      /** @type {ReturnType<typeof setTimeout>|null} */
      let sseRefreshTimer = null;
      /** @type {Set<string>} event kinds ("squad"/"guardian"/"other") seen since the last flush */
      let sseRefreshKinds = new Set();
      /** @type {Set<string>} guardian ids referenced by a pending event, since the last flush */
      let sseRefreshGuardianIds = new Set();
      // Coalescing window: long enough to merge a burst of near-simultaneous
      // events into one refresh, short enough that push still feels instant
      // next to the old 2s poll.
      const SSE_DEBOUNCE_MS = 150;
      /**
       * Applies whatever the active tab needs in response to one coalesced
       * batch of pushed events -- mirrors `tick()`'s per-tab dispatch, plus a
       * feedback-thread refresh when the open review itself just changed.
       * @param {Set<string>} kinds
       * @param {Set<string>} guardianIds
       * @returns {Promise<void>}
       */
      async function applySseRefresh(kinds, guardianIds) {
        await updateCounter();
        if (tab === "reviews" && kinds.has("guardian")) {
          await pollReviews();
          if (selectedGuardian && guardianIds.has(selectedGuardian)) await refreshExpandedBranchMessages(selectedGuardian);
        } else if (tab === "queue" && kinds.has("squad") && (queueUI.autoUpdate || !queueLoaded)) {
          await pollQueue();
        } else if (tab === "cartographer") {
          // Every event is Cartographer-worthy by construction -- always
          // refresh this tab's own view of the log, regardless of kind.
          await pollCartographer();
        } else if (tab === "squads" && kinds.has("squad")) {
          await pollTasks();
        } else if (tab === "tasks" && kinds.has("squad")) {
          await pollTasksTab();
        }
        await refreshBanner();
      }
      /**
       * Records one pushed event and (re)schedules a debounced `applySseRefresh`.
       * @param {string} kind
       * @param {CartographerRow} row
       * @returns {void}
       */
      function scheduleSseRefresh(kind, row) {
        sseRefreshKinds.add(kind);
        if (row.guardian_id) sseRefreshGuardianIds.add(row.guardian_id);
        if (sseRefreshTimer) return;
        sseRefreshTimer = setTimeout(() => {
          const kinds = sseRefreshKinds; sseRefreshKinds = new Set();
          const guardianIds = sseRefreshGuardianIds; sseRefreshGuardianIds = new Set();
          sseRefreshTimer = null;
          applySseRefresh(kinds, guardianIds);
        }, SSE_DEBOUNCE_MS);
      }
      // How long to wait before minting a fresh ticket and reconnecting after
      // `/api/events` drops or a connect attempt fails outright (RAL-222).
      // `EventSource`'s own built-in retry would otherwise replay the exact
      // same (now-consumed, single-use) ticket forever, so `connectEventStream`
      // takes over reconnection itself instead of relying on it.
      const SSE_RECONNECT_DELAY_MS = 3000;
      /**
       * Mints a short-lived, single-use `/api/events` ticket (RAL-222) via
       * `POST /api/events/ticket` -- an `EventSource` cannot set an
       * `Authorization` header itself, so the ticket travels as a `?ticket=...`
       * query param instead. See `daemon/src/token.rs`'s module doc comment
       * for the full design. `null` on any failure (daemon down, request
       * rejected, malformed response) -- the caller retries later.
       * @returns {Promise<string|null>}
       */
      async function fetchEventsTicket() {
        try {
          const resp = await fetch("/api/events/ticket", { method: "POST" });
          if (!resp.ok) return null;
          const data = await resp.json();
          return typeof data.ticket === "string" ? data.ticket : null;
        } catch (_) {
          return null;
        }
      }
      /**
       * Opens the `/api/events` SSE connection and wires each named event
       * (`squad`/`guardian`/`other` -- see `EventKind` in
       * `daemon/src/events.rs`) into `scheduleSseRefresh`. First mints a
       * ticket (RAL-222) and supplies it as a query param, since `EventSource`
       * cannot set custom request headers. Because that ticket is single-use,
       * this owns reconnection itself (minting a fresh ticket each time)
       * rather than trusting `EventSource`'s built-in retry, which would just
       * replay the same, now-invalid, ticket.
       * @returns {Promise<void>}
       */
      async function connectEventStream() {
        const ticket = await fetchEventsTicket();
        if (!ticket) {
          byId("conn").className = "dot off";
          setTimeout(connectEventStream, SSE_RECONNECT_DELAY_MS);
          return;
        }
        const es = new EventSource(`/api/events?ticket=${encodeURIComponent(ticket)}`);
        es.onopen = () => { byId("conn").className = "dot on"; };
        es.onerror = () => {
          byId("conn").className = "dot off";
          es.close();
          setTimeout(connectEventStream, SSE_RECONNECT_DELAY_MS);
        };
        /**
         * @param {MessageEvent} e
         * @param {string} kind
         * @returns {void}
         */
        const onEvent = (e, kind) => {
          try {
            /** @type {CartographerRow} */
            const row = JSON.parse(e.data);
            scheduleSseRefresh(kind, row);
          } catch (_) { /* malformed/heartbeat -- ignore */ }
        };
        es.addEventListener("squad", (e) => onEvent(/** @type {MessageEvent} */ (e), "squad"));
        es.addEventListener("guardian", (e) => onEvent(/** @type {MessageEvent} */ (e), "guardian"));
        es.addEventListener("other", (e) => onEvent(/** @type {MessageEvent} */ (e), "other"));
      }
      /**
       * Resolves a pending hash route's squad reference against the loaded squad
       * list: a squad id, or — for a hand-written RAL-188 URI that omitted the
       * `?id=` sidecar — a squad *label*. An ambiguous label resolves to nothing
       * rather than guessing (§C.3).
       * @param {ParsedHash} ph
       * @returns {SquadView|undefined}
       */
      function squadForPendingHash(ph) {
        if (!ph.squadId) return undefined;
        const exact = findSquad(ph.squadId);
        if (exact || !ph.uri) return exact;
        const labelled = squads.filter((r) => r.label && r.label === ph.squadId);
        return labelled.length === 1 ? labelled[0] : undefined;
      }
      /**
       * Turns a pending hash route into the tasks-tab selection state, from
       * either the RAL-188 URI or the legacy `kind:ti:si[:vi]` string. A URI
       * that names something the squad no longer has falls back to selecting the
       * squad itself, rather than leaving a dangling index.
       * @param {ParsedHash} ph
       * @param {SquadView} squad
       * @returns {SelStateTasks}
       */
      function selForPendingHash(ph, squad) {
        if (ph.uri) return selFromUri(ph.uri, squad) || { kind: "squad", taskIdx: 0, cellIdx: 0, proofIdx: -1 };
        if (ph.sel) {
          const [k, ti, si, vi] = ph.sel.split(":");
          return { kind: k, taskIdx: +ti || 0, cellIdx: +si || 0, proofIdx: vi !== undefined ? +vi : -1 };
        }
        return { kind: "squad", taskIdx: 0, cellIdx: 0, proofIdx: -1 };
      }
      /**
       * Refreshes the current user's hidden-squad/hidden-review sets
       * (RAL-328/RAL-331) from the daemon. Called from `tick()` regardless
       * of the active tab, since goto-search and both sidebars need it.
       * Silent on failure (e.g. no default_user configured and no
       * X-Ralphus-User header sent) -- hidden filtering simply stays
       * inactive until identity resolves.
       * @returns {Promise<void>}
       */
      async function pollHidden() {
        try {
          const res = await fetch("/api/hidden");
          if (!res.ok) return;
          /** @type {{hidden: HiddenItem[]}} */
          const d = await res.json();
          hiddenSquadIds = new Set(d.hidden.filter((h) => h.kind === "squad" && h.squad_id).map((h) => /** @type {string} */ (h.squad_id)));
          hiddenGuardianIds = new Set(d.hidden.filter((h) => h.kind === "review" && h.guardian_id).map((h) => /** @type {string} */ (h.guardian_id)));
        } catch (e) { /* transient -- the next tick retries */ }
      }
      // RALPHUS-POLL-TASKS:BEGIN
      /**
       * Polls `/api/tasks` and re-renders the Squads tab, applying any pending hash-derived selection.
       * RAL-390: shares one in-flight request and a monotonic ticket with `updateCounter` -- see the
       * `tasksPollSeq` declaration above. A call superseded by a newer one before its data lands
       * abandons itself instead of writing stale `squads`/rendering a stale selection.
       * @returns {Promise<void>}
       */
      async function pollTasks() {
        const seq = ++tasksPollSeq;
        try {
          const d = await fetchTasksShared();
          if (seq !== tasksPollSeq) return; // superseded -- a newer poll owns the render
          byId("conn").className = "dot on";
          /** @type {any} */ (window)._daemonStatus = d.daemon;
          byId("running").textContent = formatConcurrencyStatus(d.daemon.running ?? 0, d.daemon.max_concurrent ?? 0);
          byId("updated").textContent = "updated " + new Date().toLocaleTimeString();
          squads = d.squads || [];
          const wantSquad = pendingHash ? squadForPendingHash(pendingHash) : undefined;
          if (pendingHash && wantSquad) {
            const want = pendingHash; pendingHash = null;
            clearNodeMultiSel();
            selectedSquadId = wantSquad.id;
            revealedSquadId = wantSquad.id;
            sel = selForPendingHash(want, wantSquad);
            renderSortChips(); renderStatusFilters(); renderAll();
          } else if (!selectedSquadId && squads.length) { pendingHash = null; selectSquad(squads[0].id); }
          else if (userIsSelecting()) { /* keep the user's text selection intact */ }
          else if (!editing) renderAll();
          else renderSquads();
        } catch (e) {
          if (seq !== tasksPollSeq) return;
          byId("conn").className = "dot off";
          byId("updated").textContent = "daemon unreachable";
        }
      }
      // RALPHUS-POLL-TASKS:END
      /**
       * Fetches the live conflicting-files list for one review branch (RAL-148)
       * and caches it for `branchConflictFiles` to render. Silent on failure --
       * a transient fetch error just leaves the previous cached value in place
       * until the next poll.
       * @param {string} gid
       * @param {string} branchId
       * @returns {Promise<void>}
       */
      async function fetchBranchConflicts(gid, branchId) {
        try {
          const res = await fetch(`/api/guardians/${gid}/branches/${branchId}/conflicts`);
          if (!res.ok) return;
          /** @type {BranchConflicts} */
          const data = await res.json();
          branchConflicts[`${gid}:${branchId}`] = data;
        } catch (e) { /* transient -- the next poll retries */ }
      }
      /**
       * Refreshes the live conflicting-files list (RAL-148) for every branch of
       * `gid` currently in a failed merge state, and drops cached entries for
       * branches that are no longer failed (resolved, disabled, or moved out).
       * Only polled for the open review -- not the whole guardians list -- to
       * keep this off the hot board-poll path (mirrors the RAL-121 pattern
       * `pollReviews` already uses for the per-guardian summary fetch below).
       * @param {string} gid
       * @returns {Promise<void>}
       */
      async function pollBranchConflicts(gid) {
        const g = guardians.find((x) => x.id === gid);
        if (!g) return;
        const failedIds = new Set(g.branches.filter((b) => b.merge_status === "failed").map((b) => b.id));
        Object.keys(branchConflicts).forEach((k) => {
          if (k.startsWith(`${gid}:`) && !failedIds.has(k.slice(gid.length + 1))) delete branchConflicts[k];
        });
        await Promise.all([...failedIds].map((bid) => fetchBranchConflicts(gid, bid)));
      }
      /**
       * Fetches and caches the live drift check for one open PR (RAL-190).
       * Silent on failure -- a transient error leaves the previous cached
       * value in place until the next poll.
       * @param {string} prId
       * @returns {Promise<void>}
       */
      async function fetchPrSyncStatus(prId) {
        try {
          const res = await fetch(`/api/pull-requests/${prId}/sync-status`);
          if (!res.ok) return;
          /** @type {PrSyncStatus} */
          const data = await res.json();
          prSyncStatus[prId] = data;
        } catch (e) { /* transient -- the next poll retries */ }
      }
      /**
       * Fetches the PRs submitted for a review and caches them, along with a
       * live drift check (RAL-190) for each still-open one that has a
       * recorded forge number. Only polled for the open review, mirroring
       * `pollBranchConflicts`.
       * @param {string} gid
       * @returns {Promise<void>}
       */
      async function pollPullRequests(gid) {
        try {
          const res = await fetch(`/api/guardians/${gid}/pull-requests`);
          if (!res.ok) return;
          /** @type {PullRequestView[]} */
          const prs = await res.json();
          pullRequests[gid] = prs;
          const open = prs.filter((p) => p.state === "open" && p.pr_number != null);
          await Promise.all(open.map((p) => fetchPrSyncStatus(p.id)));
        } catch (e) { /* transient -- the next poll retries */ }
      }
      // RALPHUS-REVIEW-POLL:BEGIN
      /** Monotonic sequence over `pollReviews` invocations — each call captures its number at start; a call that is no longer the freshest abandons itself (RAL-382). */
      let reviewPollSeq = 0;
      /**
       * Polls `/api/guardians` and re-renders the Reviews tab.
       * RAL-382: overlapping invocations are guarded by a monotonic sequence
       * counter — each call captures its number at start and abandons itself if
       * a newer poll has started by the time any of its awaits resolve, so an
       * older, slower poll can never overwrite the newer poll's data or render.
       * @returns {Promise<void>}
       */
      async function pollReviews() {
        const seq = ++reviewPollSeq;
        try {
          const fresh = await (await fetch("/api/guardians")).json();
          // A newer poll started while this fetch was in flight — abandon this
          // one without touching `guardians`, whose fresher value belongs to it.
          if (seq !== reviewPollSeq) { console.debug("pollReviews: superseded, abandoning"); return; }
          guardians = fresh;
          checkGuardianNotices(guardians);
          byId("conn").className = "dot on";
          markUpdated();
          if (pendingHash && pendingHash.tab === "reviews") {
            const want = pendingHash; pendingHash = null;
            // `?id=` first (authoritative), then the REVIEW[...] label — which
            // only wins when it matches exactly one review, never silently on
            // a tie (§C.3).
            const byName = want.sel ? guardians.filter((g) => g.name === want.sel) : [];
            if (want.guardianId && findGuardian(want.guardianId)) selectedGuardian = want.guardianId;
            else if (byName.length === 1) selectedGuardian = byName[0].id;
            else if (want.guardianId) selectedGuardian = want.guardianId;
            // RAL-382: a hash-navigated review may not be in the loaded set yet;
            // show the loading placeholder instead of the previous selection.
            if (selectedGuardian && !findGuardian(selectedGuardian)) reviewDetailLoading = selectedGuardian;
          }
          if (!selectedGuardian && guardians.length) {
            const firstVisible = visibleGuardians()[0];
            selectedGuardian = (firstVisible || guardians[0]).id;
          }
          if (selectedGuardian) revealedGuardianId = selectedGuardian;
          syncHash();  // keep URL in sync with selectedGuardian (replaceState)
          // RAL-121: fetching a single guardian is the daemon's "the user is
          // looking at this one" signal — it promotes that guardian's
          // preliminary change-summary job to high priority (or wakes a cold
          // one) on a background worker, instead of every review in this
          // list computing its git-log summary eagerly. Fire-and-forget: the
          // response isn't used since `guardians` (from the list fetch above)
          // already has everything `renderReviewDetail` needs, and the next
          // poll picks up the summary once that worker finishes it.
          if (selectedGuardian) fetch(`/api/guardians/${selectedGuardian}`).catch(() => {});
          // Keep every expanded branch's feedback thread fresh (RAL-272) so a
          // guardian's async acknowledgment appears without a manual refresh.
          // These four fetches are independent of each other (each caches
          // into its own state and none reads another's result), so they run
          // concurrently (RAL-234) instead of as a sequential await chain --
          // the old chained-await version summed all round-trips instead of
          // taking the max of them.
          if (selectedGuardian) {
            await Promise.all([
              refreshExpandedBranchMessages(selectedGuardian, { silent: true }),
              pollBranchConflicts(selectedGuardian),
              pollPullRequests(selectedGuardian),
              pollPrErrors(selectedGuardian),
            ]);
            // The awaited refreshes may have been overtaken by a newer poll
            // (or re-selection) — a stale render now would show old data.
            if (seq !== reviewPollSeq) { console.debug("pollReviews: superseded, abandoning"); return; }
          }
          if (!userIsSelecting()) { renderReviews(); preserveUserState(document.getElementById("review-detail"), renderReviewDetail); }
        } catch (e) { byId("conn").className = "dot off"; }
      }
      // RALPHUS-REVIEW-POLL:END

