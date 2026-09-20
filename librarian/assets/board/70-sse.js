      // ---------- polling ----------
      // RAL-7: a periodic/pushed refresh re-renders panes via `el.innerHTML = …`, which resets
      // scroll position and wipes in-progress text in a pane's inputs. Captures
      // the user's scroll position and focused field beforehand and restores them
      // after render — live updates keep flowing without clobbering typing.
      // RALPHUS-PRESERVE-USER-STATE:BEGIN
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
      // RAL-431 (follow-up): `userIsSelecting()` is page-wide, so selecting
      // text anywhere (e.g. copying a cwd path out of the details pane) froze
      // the sidebar and graph too, not just the pane holding the selection --
      // their status badges kept showing whatever state was live at the
      // moment of selection, indefinitely, since nothing re-checks once a
      // selection is left dangling (e.g. the tab loses focus before it's
      // cleared). Scope the skip to whichever element the selection is
      // actually anchored inside.
      /**
       * Checks whether the user's active text/DOM selection is anchored inside `el`, so only that pane's re-render needs to be skipped.
       * @param {HTMLElement|null} el
       * @returns {boolean}
       */
      function selectionWithin(el) {
        if (!el) return false;
        const a = /** @type {HTMLInputElement|HTMLTextAreaElement|Element|null} */ (document.activeElement);
        if (a && el.contains(a) && "selectionStart" in a && a.selectionStart != null && a.selectionStart !== a.selectionEnd) return true;
        const s = window.getSelection && window.getSelection();
        return !!(s && !s.isCollapsed && String(s).length && s.anchorNode && el.contains(s.anchorNode));
      }
      // RALPHUS-PRESERVE-USER-STATE:END
      // RALPHUS-RENDER-ALL:BEGIN
      /**
       * Re-renders the sidebar and graph unconditionally, and the details
       * pane unless it would clobber an open edit form or the user's
       * in-pane selection.
       *
       * RAL-436 (follow-up to RAL-431): `editing` used to gate this whole
       * function at each call site (`pollTasks`), so opening any edit form
       * anywhere froze the sidebar *and* graph -- every status pill and the
       * whole dependency graph sat stale until the form was closed, no
       * matter what elsewhere on the board changed. Scoping the skip to just
       * the details pane, the same fix RAL-431 already applied to
       * `selectionWithin`, lets the rest of the board keep updating live.
       * @returns {void}
       */
      function renderAll() {
        renderSquads();
        renderGraph();
        const details = document.getElementById("details");
        if (editing || selectionWithin(details)) return; // keep the open edit form / user's in-pane selection intact
        preserveUserState(details, renderDetails);
      }
      // RALPHUS-RENDER-ALL:END
      // RAL-430: dozens of local UI toggles (peek boxes, terminal-log
      // attempt history, prompt tabs, menus) each re-render whichever of the
      // details pane / review-detail pane currently owns them with a plain
      // `if (sel.kind) renderDetails(); if (selectedGuardian)
      // renderReviewDetail();` pair -- every one of those innerHTML swaps
      // reset the pane's scroll position (and any nested peek box's) back to
      // the top, which is what made the live terminal viewer and system
      // prompt view keep jumping. Routing both branches through
      // `preserveUserState` here gives every such call site the same fix in
      // one place, rather than each needing its own bespoke save/restore.
      // RALPHUS-RERENDER-OWNING-PANE:BEGIN
      /**
       * Re-renders whichever of the details pane / review-detail pane is
       * currently showing something, preserving scroll position and focused-
       * input state on each (RAL-430).
       * @returns {void}
       */
      function rerenderOwningPane() {
        if (sel.kind) preserveUserState(document.getElementById("details"), renderDetails);
        if (selectedGuardian) preserveUserState(document.getElementById("review-detail"), renderReviewDetail);
      }
      // RALPHUS-RERENDER-OWNING-PANE:END
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
       * Monotonic render-ownership ticket for `pollTasks` (RAL-390): each
       * `pollTasks` call claims a number at start, and only the call still
       * holding the latest one when its data lands may write `squads` or
       * render, regardless of which fetch resolves first. Same latest-wins
       * pattern `reviewPollSeq` already proved out for `pollReviews`
       * (RAL-382).
       *
       * `updateCounter` *observes* this ticket without claiming it. It must
       * never claim one: it refreshes the counter but renders nothing, so a
       * claim would let it supersede an in-flight `pollTasks` — which then
       * abandons itself on the `seq !== tasksPollSeq` check and skips its
       * render, while `updateCounter` writes the fresh `squads` and paints
       * nothing. The result was a board holding correct data behind a stale
       * DOM until an unrelated event forced a render, which is why clicking
       * a squad away and back "fixed" a status badge that had stopped
       * updating on its own.
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
       * Bumped every time a new in-flight `/api/tasks` request is minted (or
       * discarded via `invalidateTasksFetch`, RAL-406) -- each request's own
       * cleanup compares against this to tell whether it still owns
       * `tasksFetchInFlight` before clearing it, so a slow, already-superseded
       * request settling late can't clobber a newer request's in-flight slot.
       */
      let tasksFetchGeneration = 0;
      /**
       * Fetches and parses `/api/tasks`, reusing the current in-flight
       * request if one is already running instead of starting a duplicate.
       * @returns {Promise<any>}
       */
      function fetchTasksShared() {
        if (!tasksFetchInFlight) {
          const gen = ++tasksFetchGeneration;
          tasksFetchInFlight = (async () => {
            try {
              return await (await fetch("/api/tasks")).json();
            } finally {
              // Only clear the slot if it's still this request's -- a
              // slow request that `invalidateTasksFetch` (RAL-406) already
              // discarded, settling after a newer request has since claimed
              // the slot, must not drop that newer request's own reference
              // and force a redundant extra fetch.
              if (tasksFetchGeneration === gen) tasksFetchInFlight = null;
            }
          })();
        }
        return tasksFetchInFlight;
      }
      /**
       * The in-flight `/api/task-index` request `updateCounter` shares, and
       * its generation counter -- the same dedup `fetchTasksShared` above
       * does for `/api/tasks`, deliberately kept as a separate pair rather
       * than factored into a shared helper, so the RAL-406 invalidation
       * logic guarding the squad-status path stays exactly as written and
       * tested.
       * @type {Promise<any>|null}
       */
      let taskIndexFetchInFlight = null;
      /** Generation counter for `taskIndexFetchInFlight`, mirroring `tasksFetchGeneration`. */
      let taskIndexFetchGeneration = 0;
      /**
       * Fetches and parses `/api/task-index`, reusing the current in-flight
       * request if one is already running instead of starting a duplicate.
       *
       * `updateCounter` reads this compact endpoint rather than `/api/tasks`:
       * it needs `daemon.running`/`max_concurrent` and a squad list carrying
       * ids, labels and task names, all of which the index carries, while the
       * full board view is ~9x larger (6.2MB vs 684KB against a real squad
       * history). The Squads tab's own `pollTasks` still reads `/api/tasks`,
       * since it renders the per-cell detail only that response has.
       * @returns {Promise<any>}
       */
      function fetchTaskIndexShared() {
        if (!taskIndexFetchInFlight) {
          const gen = ++taskIndexFetchGeneration;
          taskIndexFetchInFlight = (async () => {
            try {
              return await (await fetch("/api/task-index")).json();
            } finally {
              if (taskIndexFetchGeneration === gen) taskIndexFetchInFlight = null;
            }
          })();
        }
        return taskIndexFetchInFlight;
      }
      /**
       * Discards the current in-flight `/api/tasks` request (if any) so the
       * next `fetchTasksShared` caller issues a brand-new one instead of
       * piggybacking on it (RAL-406). A squad/task/cell mutation (cancel,
       * restart, retry, set-status, delete, ...) can finish committing
       * server-side while an `/api/tasks` request sent *before* the mutation
       * is still in flight -- without this, the post-mutation `tick()`'s
       * poll would share that stale in-flight request via `fetchTasksShared`
       * and render pre-mutation status, which then looks "stuck" until some
       * unrelated later poll (e.g. a tab switch) finally issues a fresh one.
       * Does not abort the stale request itself -- it may still be relied on
       * by a caller that started before the mutation -- it only stops new
       * callers from reusing it.
       * @returns {void}
       */
      function invalidateTasksFetch() {
        tasksFetchInFlight = null;
        taskIndexFetchInFlight = null;
        // A squad mutation can rewrite a prompt (the details pane's edit
        // form), so the cached text has to be refetched rather than kept.
        promptCacheSquadId = null;
        promptCache = {};
      }
      // ---- prompt-text cache (details pane) ----
      // `GET /api/tasks` omits `cell.prompt`/`cell.system_prompt`/
      // `proof.system_prompt` -- 79% of that response, and only ever shown
      // for the one selected cell or proof step. The text therefore lives
      // here, keyed within one squad, instead of on the rows themselves:
      // every poll replaces `squads` wholesale with a fresh array whose
      // prompt fields are null, so anything stored on a row is lost each
      // tick. Caching separately and re-applying *before* the first render
      // is what keeps the details pane from blanking and repainting on
      // every refresh.
      /** @type {string|null} squad id the cached text belongs to. */
      let promptCacheSquadId = null;
      /** @type {{[key: string]: {prompt?: string, system_prompt?: string|null}}} */
      let promptCache = {};
      /** @type {Map<string, Promise<boolean>>} squad id -> in-flight fetch promise, so a poll burst or a rapid reselect of the same squad shares one request instead of one each. */
      let promptFetchesInFlight = new Map();
      /**
       * Cache key for a cell.
       * @param {number} ti
       * @param {number} si
       * @returns {string}
       */
      function promptKeyCell(ti, si) { return `t${ti}c${si}`; }
      /**
       * Cache key for a cell-scoped proof step.
       * @param {number} ti
       * @param {number} si
       * @param {number} vi
       * @returns {string}
       */
      function promptKeyCellProof(ti, si, vi) { return `t${ti}c${si}p${vi}`; }
      /**
       * Cache key for a task-scoped proof step.
       * @param {number} ti
       * @param {number} vi
       * @returns {string}
       */
      function promptKeyTaskProof(ti, vi) { return `t${ti}p${vi}`; }
      /**
       * Starts loading prompt text for the current selection if it is not
       * already cached.
       *
       * Called from `renderDetails`, which every selection path funnels
       * through, so picking a cell fetches its text immediately. Previously
       * this only ran at the tail of `pollTasks`, so a selection made on a
       * quiet daemon sat empty until the next poll -- up to the 60s
       * reconciliation tick.
       *
       * Cheap to call on every render: it returns immediately once the
       * squad is cached or a fetch for it is already in flight. The
       * callback re-checks the *current* selection rather than assuming
       * `wanted` is still shown -- the selection can move to a different
       * squad while this fetch is in flight, and retrying for whatever is
       * selected now is what keeps the pane from getting stuck without a
       * prompt until an unrelated future render happens to retry it.
       * @returns {void}
       */
      function syncPromptCache() {
        if (!selectedSquadId || promptCacheSquadId === selectedSquadId) return;
        if (!squads.some((r) => r.id === selectedSquadId)) return;
        const wanted = selectedSquadId;
        ensurePromptCache(wanted).then((loaded) => {
          if (loaded) applyPromptCache();
          // Only retry if the selection moved on to a different, not-yet-
          // cached squad while this fetch was in flight -- a failed fetch
          // for the squad still selected falls back to the next poll, same
          // as before, rather than hammering the daemon in a tight loop.
          if (selectedSquadId && selectedSquadId !== wanted && promptCacheSquadId !== selectedSquadId) {
            syncPromptCache();
            return;
          }
          preserveUserState(document.getElementById("details"), renderDetails);
        });
      }
      /**
       * Copies cached prompt text back onto the current `squads` rows.
       * Called before the first render of every poll, so the details pane
       * paints with text already in place.
       * @returns {void}
       */
      function applyPromptCache() {
        if (!promptCacheSquadId) return;
        const squad = squads.find((r) => r.id === promptCacheSquadId);
        if (!squad) return;
        (squad.tasks || []).forEach((t, ti) => {
          (t.proof || []).forEach((v, vi) => {
            const e = promptCache[promptKeyTaskProof(ti, vi)];
            if (e) v.system_prompt = e.system_prompt;
          });
          (t.cells || []).forEach((c, si) => {
            const e = promptCache[promptKeyCell(ti, si)];
            if (e) { c.prompt = e.prompt; c.system_prompt = e.system_prompt; }
            (c.proof || []).forEach((v, vi) => {
              const pe = promptCache[promptKeyCellProof(ti, si, vi)];
              if (pe) v.system_prompt = pe.system_prompt;
            });
          });
        });
      }
      /**
       * Loads one squad's prompt text from `GET /api/squads/{id}` (~10KB),
       * unless it is already cached or in flight. Fetching is keyed on the
       * squad, not the poll, so switching selection inside a squad costs
       * nothing and a standing poll re-fetches nothing.
       * @param {string|null} id
       * @returns {Promise<boolean>} whether fresh text was loaded
       */
      async function ensurePromptCache(id) {
        if (!id || promptCacheSquadId === id) return false;
        const existing = promptFetchesInFlight.get(id);
        if (existing) return existing;
        if (!squads.some((r) => r.id === id)) return false;
        const promise = (async () => {
          try {
            const res = await fetch(`/api/squads/${encodeURIComponent(id)}`);
            if (!res.ok) return false;
            /** @type {SquadView} */
            const detail = await res.json();
            /** @type {{[key: string]: {prompt?: string, system_prompt?: string|null}}} */
            const map = {};
            (detail.tasks || []).forEach((t, ti) => {
              (t.proof || []).forEach((v, vi) => { map[promptKeyTaskProof(ti, vi)] = { system_prompt: v.system_prompt }; });
              (t.cells || []).forEach((c, si) => {
                map[promptKeyCell(ti, si)] = { prompt: c.prompt, system_prompt: c.system_prompt };
                (c.proof || []).forEach((v, vi) => { map[promptKeyCellProof(ti, si, vi)] = { system_prompt: v.system_prompt }; });
              });
            });
            promptCache = map;
            promptCacheSquadId = id;
            return true;
          } catch (e) {
            return false; // transient -- the next poll retries
          } finally {
            promptFetchesInFlight.delete(id);
          }
        })();
        promptFetchesInFlight.set(id, promise);
        return promise;
      }
      // RALPHUS-TASKS-POLL-SEQ:END
      // ---- cross-squad task/cell index (on demand) ----
      // Three board features are genuinely cross-squad -- go-to search, the
      // header's running-work dropdown, and the review worktree-linkage
      // lookup -- but each needs only shallow fields (task name/project,
      // cell state/name/cwd, proof state), never the per-cell prompt text or
      // the rest of the full board view. They read this index instead, so
      // the standing `/api/tasks` poll does not have to carry the whole task
      // tree on their behalf.
      /** @type {SquadView[]} */
      let taskIndex = [];
      /** Whether {@link loadTaskIndex} has ever completed, so an on-demand opener can tell "empty" from "not fetched yet". */
      let taskIndexLoaded = false;
      /**
       * Records a freshly fetched `/api/task-index` squad list.
       * @param {SquadView[]|undefined} squadList
       * @returns {void}
       */
      function setTaskIndex(squadList) {
        taskIndex = squadList || [];
        taskIndexLoaded = true;
      }
      /**
       * Fetches `/api/task-index` into {@link taskIndex}. Called by the
       * features above right before they open, and for free by
       * `updateCounter`, which already reads that endpoint on every tab
       * whose own poll does not.
       * @returns {Promise<void>}
       */
      async function loadTaskIndex() {
        try {
          // Shares `updateCounter`'s in-flight request rather than issuing a
          // second one: on every tab where that runs, this costs nothing.
          setTaskIndex((await fetchTaskIndexShared()).squads);
        } catch (e) { /* transient -- the opener falls back to whatever is cached */ }
      }
      // RALPHUS-UPDATE-COUNTER:BEGIN
      /**
       * Fetches the daemon status counter and `squads` cache from
       * `/api/task-index`, without triggering task-view rendering.
       *
       * Reads the compact index rather than `/api/tasks` -- see
       * `fetchTaskIndexShared` for the size argument. This runs on every tab
       * *except* Squads and Tasks (whose own polls refresh the counter from
       * the response they already fetch), so on the Reviews tab it was
       * pulling the full 6.2MB board on every pushed event batch purely to
       * repaint two integers. `pollTasksTab` already writes this same
       * `/api/task-index` shape into `squads`, so the cache stays consistent
       * with what the Tasks tab puts there.
       *
       * RAL-390: observes (never claims) `pollTasks`'s render-ownership
       * ticket -- see the `tasksPollSeq` declaration above for why claiming
       * one silently dropped renders.
       * @returns {Promise<void>}
       */
      async function updateCounter() {
        const seq = tasksPollSeq;
        try {
          const d = await fetchTaskIndexShared();
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
        // These four are independent of each other and of the per-tab refresh
        // below -- none reads another's result -- so they run concurrently
        // rather than as a sequential chain that sums four round-trips.
        // `updateCounter` joins them for the same reason: every tab's branch
        // awaited it before starting its own fetch, for no dependency.
        //
        // It is skipped entirely on the two tabs whose own poll below reads
        // the counter and the `squads` cache out of the same response it
        // would have fetched. Keeping it there cost a second, serial
        // `/api/tasks` round-trip on the critical path of every
        // post-mutation refresh -- a cancel/restart calls `tick()` the
        // moment its POST returns, so that duplicate was doubling the delay
        // before the board showed the new status.
        const selfCountingTab = tab === "squads" || tab === "tasks";
        const globalPolls = Promise.all([
          pollWhoAmI(), pollHidden(), pollWatches(), pollMailbox(),
          ...(selfCountingTab ? [] : [updateCounter()]),
        ]);
        // The active tab's own fetch runs *alongside* the four global polls
        // above, not after them -- none of the four is reviews/tasks/etc.
        // data, so a slow whoami/hidden/watches/mailbox must never delay the
        // tab the user is actually looking at from painting.
        // RAL-345: the Tasks/Squads project-filter dropdowns bind to the live
        // registered-project list (`registeredProjectNames`), refreshed on every
        // poll of those tabs so a newly registered project shows up without a reload.
        // `findLinkedCells` runs inside the Reviews tab's synchronous render,
        // so the index it reads has to be in place before `pollReviews`.
        /** @type {Promise<void>} */
        let tabPoll;
        if (tab === "reviews") tabPoll = loadTaskIndex().then(() => pollReviews());
        else if (tab === "resources") tabPoll = pollResources();
        else if (tab === "queue") tabPoll = (queueUI.autoUpdate || !queueLoaded) ? pollQueue() : Promise.resolve();
        else if (tab === "cartographer") tabPoll = pollCartographer();
        else if (tab === "projects") tabPoll = pollProjects();
        else if (tab === "machines") tabPoll = pollMachines();
        else if (tab === "triage") tabPoll = pollTriage();
        else if (tab === "users") tabPoll = pollUsers();
        else if (tab === "secrets") tabPoll = pollSecretEnvNames();
        else if (tab === "agents") tabPoll = Promise.all([pollAgentBackendCommands(), pollAgentProfiles()]).then(() => undefined);
        else if (tab === "worktree-retirement") tabPoll = pollWorktreeRetirements();
        else if (tab === "health") tabPoll = pollHealth();
        else if (tab === "prefs") tabPoll = pollPrefs();
        else if (tab === "tasks") tabPoll = refreshRegisteredProjectNames().then(() => pollTasksTab());
        else tabPoll = refreshRegisteredProjectNames().then(() => pollTasks());
        await Promise.all([globalPolls, tabPoll]);
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
      /**
       * The live `/api/events` connection, tracked so a tab that regains
       * visibility (see the `visibilitychange` handler below) can tell
       * whether it needs to reconnect instead of trusting that the 60s
       * fallback alone will catch up.
       * @type {EventSource|null}
       */
      let currentEventSource = null;
      /** @type {ReturnType<typeof setTimeout>|null} */
      let sseRefreshTimer = null;
      /** @type {Set<string>} event kinds ("squad"/"guardian"/"other") seen since the last flush */
      let sseRefreshKinds = new Set();
      /** @type {Set<string>} guardian ids referenced by a pending event, since the last flush */
      let sseRefreshGuardianIds = new Set();
      /** Whether a pending event carries a squad id, even if it is classified as a guardian event. */
      let sseRefreshHasSquadChange = false;
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
       * @param {boolean} hasSquadChange
       * @returns {Promise<void>}
       */
      async function applySseRefresh(kinds, guardianIds, hasSquadChange) {
        if (tab === "tasks") {
          // RAL-345: an SSE-driven poll of this tab doesn't go through
          // `tick()`, so refresh the registered-project list here too --
          // keeps the project-filter dropdown current between full ticks.
          await refreshRegisteredProjectNames();
          await pollTasksTab();
          await refreshBanner();
          return;
        }
        // The Squads tab's own poll already refreshes the daemon-status
        // counter and the `squads` cache from the very same `/api/tasks`
        // response, so `updateCounter` here would be a second, redundant
        // round-trip against an endpoint that takes seconds on a large squad
        // history -- and, worse, a racing one (see `tasksPollSeq`). Going
        // straight to `pollTasks` is both cheaper and the only path that
        // actually repaints. It runs for every batch, not just one carrying
        // a squad id: a guardian-only batch can still change what this tab
        // renders (a cell's review badge), and the old `hasSquadChange` gate
        // meant those batches refreshed the data without ever painting it.
        if (tab === "squads") {
          // RAL-345: same as the Tasks-tab path above -- the project-filter
          // dropdown binds to the live registered-project list, refreshed on
          // SSE-driven polls too (no full tick involved).
          await refreshRegisteredProjectNames();
          await pollTasks();
          await refreshBanner();
          return;
        }
        await updateCounter();
        if (tab === "reviews") {
          // Deliberately not gated on `kinds.has("guardian")`. A branch
          // becomes ready when the cells behind it finish, and a cell-state
          // Cartographer row carries no `guardian_id` -- so it arrives
          // classified "squad" (see `EventKind::for_row` in
          // `daemon/src/events.rs`), and the old guardian-only gate dropped
          // exactly the events that flip a branch's status badge. Those
          // badges then sat stale until the 60s reconciliation tick or a
          // manual click away and back.
          await pollReviews();
          if (selectedGuardian && guardianIds.has(selectedGuardian)) await refreshExpandedBranchMessages(selectedGuardian);
        } else if (tab === "queue" && (hasSquadChange || kinds.has("squad")) && (queueUI.autoUpdate || !queueLoaded)) {
          // `hasSquadChange` covers a row that carries a squad id but was
          // classified "guardian" because it also carries a guardian id.
          await pollQueue();
        } else if (tab === "cartographer") {
          // Every event is Cartographer-worthy by construction -- always
          // refresh this tab's own view of the log, regardless of kind.
          await pollCartographer();
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
        if (row.squad_id) sseRefreshHasSquadChange = true;
        if (sseRefreshTimer) return;
        sseRefreshTimer = setTimeout(() => {
          const kinds = sseRefreshKinds; sseRefreshKinds = new Set();
          const guardianIds = sseRefreshGuardianIds; sseRefreshGuardianIds = new Set();
          const hasSquadChange = sseRefreshHasSquadChange; sseRefreshHasSquadChange = false;
          sseRefreshTimer = null;
          applySseRefresh(kinds, guardianIds, hasSquadChange);
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
        currentEventSource = es;
        es.onopen = () => { byId("conn").className = "dot on"; };
        es.onerror = () => {
          byId("conn").className = "dot off";
          es.close();
          if (currentEventSource === es) currentEventSource = null;
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
      // A backgrounded/minimized tab has both its `setInterval` fallback and
      // (per browser) its SSE delivery throttled or fully paused -- without
      // this, a tab left in the background across a dropped connection can
      // sit on stale data indefinitely, since nothing ever runs to notice the
      // connection is dead or to catch up once it's looked at again. Firing
      // an immediate reconcile plus a connection-health check the moment the
      // tab becomes visible closes that gap without waiting on the 60s timer.
      document.addEventListener("visibilitychange", () => {
        if (document.visibilityState !== "visible") return;
        tick();
        if (!currentEventSource || currentEventSource.readyState === EventSource.CLOSED) {
          connectEventStream();
        }
      });
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
       * Refreshes the current user's hidden-squad/hidden-review/hidden-task
       * sets (RAL-328/RAL-331/RAL-365) from the daemon. Called from `tick()`
       * regardless of the active tab, since goto-search and both sidebars
       * need it. Silent on failure (e.g. no default_user configured and no
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
          hiddenTaskKeys = new Set(d.hidden.filter((h) => h.kind === "task" && h.squad_id != null && h.task_idx != null).map((h) => `${h.squad_id}:${h.task_idx}`));
        } catch (e) { /* transient -- the next tick retries */ }
      }
      // RALPHUS-POLL-TASKS-HELPERS:BEGIN
      /**
       * RAL-419: resolves a pending hash route's selection against the loaded squad,
       * reconciling a dangling index to its nearest surviving parent. The result is
       * recorded in the per-squad caches *after* resolution (never before), so a
       * later sidebar return restores exactly what the authoritative hash produced.
       * @param {ParsedHash} ph
       * @param {SquadView} squad
       * @returns {SelStateTasks}
       */
      function resolvePendingSquadSelection(ph, squad) {
        const wanted = selForPendingHash(ph, squad);
        const rec = reconcileSquadSelection(squad, wanted);
        if (rec.stale) clearSquadSelection(squad.id);
        else storeSquadSelection(squad.id, rec.sel, []);
        return rec.sel;
      }
      /**
       * RAL-419: a poll/SSE refresh can rewrite the graph under the live selection.
       * When the selected entity no longer exists, fall back to its nearest surviving
       * parent, clear the stale cache entry, and drop the (now meaningless) node
       * multi-selection. No-op while editing or with no focused squad.
       * @returns {void}
       */
      function reconcileLiveSelection() {
        if (!selectedSquadId || editing) return;
        const squad = findSquad(selectedSquadId);
        if (!squad) return; // squad deleted -- the UI's existing empty state stays; pruneSquadSelCache already cleaned its entries
        const rec = reconcileSquadSelection(squad, sel);
        if (!rec.stale) return;
        sel = rec.sel;
        nodeMultiSel = new Set();
        clearSquadSelection(selectedSquadId);
      }
      /**
       * RAL-419: first paint with no selection — return to the last-focused squad
       * (restoring its cached selection) when it still exists, else the first listed
       * squad. Callers render.
       * @returns {void}
       */
      function restoreInitialSquadSelection() {
        const id = (lastSquadId && findSquad(lastSquadId)) ? lastSquadId : squads[0].id;
        applySquadFocus(id);
      }
      // RALPHUS-POLL-TASKS-HELPERS:END
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
          // Before any render: the fresh rows carry null prompt fields, so
          // without this the details pane paints empty and only fills in
          // once the fetch below lands -- a visible blank-and-repaint on
          // every single poll.
          applyPromptCache();
          const wantSquad = pendingHash ? squadForPendingHash(pendingHash) : undefined;
          if (pendingHash && wantSquad) {
            const want = pendingHash; pendingHash = null;
            const resolved = resolvePendingSquadSelection(want, wantSquad);
            selectedSquadId = wantSquad.id;
            revealedSquadId = wantSquad.id;
            sel = resolved;
            nodeMultiSel = new Set();
            renderSortChips(); renderStatusFilters(); renderAll();
          } else {
            // RAL-419: a poll/SSE refresh can rewrite the graph under the current
            // selection (or delete squads outright) — reconcile the live selection
            // against the fresh data and prune caches for squads that no longer exist.
            pruneSquadSelCache(squadSelCache, squadNodeCache, squads.map((r) => r.id));
            reconcileLiveSelection();
            if (!selectedSquadId && squads.length) { pendingHash = null; restoreInitialSquadSelection(); renderAll(); syncHash(); }
            else renderAll(); // renderAll() itself skips the details pane while editing -- see its doc comment
          }
          // Fire-and-forget: only actually fetches when the focused squad
          // changed, so a standing poll costs nothing here.
          ensurePromptCache(selectedSquadId).then((loaded) => {
            if (!loaded || seq !== tasksPollSeq) return;
            applyPromptCache();
            if (!editing) renderAll();
          });
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
        // `g.branches` may not have merged in yet on the very first poll
        // after selecting a review -- that fetch runs concurrently with
        // this one, not before it. Skip for this cycle; the next poll picks
        // it up once full detail has landed.
        if (!g || !g.branches) return;
        const failedIds = new Set(g.branches.filter((b) => b.merge_status === "failed").map((b) => b.id));
        Object.keys(branchConflicts).forEach((k) => {
          if (k.startsWith(`${gid}:`) && !failedIds.has(k.slice(gid.length + 1))) delete branchConflicts[k];
        });
        await Promise.all([...failedIds].map((bid) => fetchBranchConflicts(gid, bid)));
      }
      /**
       * Fetches and caches the live drift check for one open PR (RAL-190).
       * Silent on failure -- a transient error leaves the previous cached
       * value in place until the next poll. Skips (rather than piling up a
       * second concurrent request) a PR whose previous fetch hasn't resolved
       * yet -- the daemon's per-PR `git fetch` can take several seconds, well
       * past this poll's own interval.
       * @param {string} prId
       * @returns {Promise<void>}
       */
      async function fetchPrSyncStatus(prId) {
        if (prSyncStatusInFlight.has(prId)) return;
        prSyncStatusInFlight.add(prId);
        try {
          const res = await fetch(`/api/pull-requests/${prId}/sync-status`);
          if (!res.ok) return;
          /** @type {PrSyncStatus} */
          const data = await res.json();
          prSyncStatus[prId] = data;
        } catch (e) { /* transient -- the next poll retries */ }
        finally { prSyncStatusInFlight.delete(prId); }
      }
      /**
       * Fetches the PRs submitted for a review and caches them, so the PR
       * badge/link (which only needs pr_url/pr_number/ci_status, all present
       * on this response) can render immediately. Also kicks off a live
       * drift check (RAL-190) for each still-open one that has a recorded
       * forge number, but does NOT wait on it -- `fetchPrSyncStatus` does its
       * own `git fetch` per PR, serialized per repo on the daemon side, which
       * can take many seconds per PR and has nothing to do with whether the
       * badge itself is ready to show. It writes into `prSyncStatus`
       * independently and the next poll tick picks it up whenever it lands.
       * Only polled for the open review, mirroring `pollBranchConflicts`.
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
          open.forEach((p) => { fetchPrSyncStatus(p.id); });
        } catch (e) { /* transient -- the next poll retries */ }
      }
      /** @type {{[id: string]: Promise<void>}} in-flight per-guardian full-detail fetches, keyed by guardian id -- a selection-triggered fetch (`ensureGuardianDetailLoaded`) and a concurrently-running `pollReviews` cycle for the same id share one request instead of firing two. */
      const guardianDetailFetches = {};
      /**
       * Fetches one guardian's full `GuardianView` detail and merges it onto
       * its existing `guardians[]` entry in place (see the `guardians`
       * declaration in `05-engines.js`) -- does not render; callers decide
       * when/whether to. Concurrent calls for the same id share one in-flight
       * fetch rather than issuing duplicate requests.
       * @param {string} gid
       * @returns {Promise<void>}
       */
      function fetchGuardianDetail(gid) {
        const inFlight = guardianDetailFetches[gid];
        if (inFlight) return inFlight;
        const p = fetch(`/api/guardians/${gid}`)
          .then((r) => (r.ok ? r.json() : null))
          .then((detail) => {
            if (!detail) return;
            const entry = guardians.find((x) => x.id === gid);
            if (entry) Object.assign(entry, detail);
          })
          .catch(() => {})
          .finally(() => { delete guardianDetailFetches[gid]; });
        guardianDetailFetches[gid] = p;
        return p;
      }
      /**
       * Kicks off `fetchGuardianDetail` for `id` if it isn't already loaded
       * on `guardians[]`, and re-renders the detail pane once it lands.
       * `pollReviews` keeps a selected guardian's detail fresh on every poll,
       * but that only runs on the next incidental SSE event or the 60s
       * reconciliation fallback -- without this, a review selected outside
       * that cycle (a click, a hash/cross-tab navigation) sat on "Loading
       * review…" until whatever poll happened to fire next, which could be
       * many seconds away. Callers should still do their own synchronous
       * `renderReviewDetail()` first, for the immediate loading-placeholder
       * paint -- this only handles the async follow-up once data arrives.
       * @param {string} id
       * @returns {void}
       */
      function ensureGuardianDetailLoaded(id) {
        const g = guardians.find((x) => x.id === id);
        if (!g || g.branches) return;
        fetchGuardianDetail(id).then(() => {
          if (selectedGuardian === id) preserveUserState(document.getElementById("review-detail"), renderReviewDetail);
        });
      }
      // RALPHUS-REVIEW-POLL:BEGIN
      /** Monotonic sequence over `pollReviews` invocations — each call captures its number at start; a call that is no longer the freshest abandons itself (RAL-382). */
      let reviewPollSeq = 0;
      /**
       * Polls `/api/guardian-index` (the lean per-review summary -- see
       * `GuardianIndexEntry`) and re-renders the Reviews tab.
       * RAL-382: overlapping invocations are guarded by a monotonic sequence
       * counter — each call captures its number at start and abandons itself if
       * a newer poll has started by the time any of its awaits resolve, so an
       * older, slower poll can never overwrite the newer poll's data or render.
       * @returns {Promise<void>}
       */
      async function pollReviews() {
        const seq = ++reviewPollSeq;
        try {
          const fresh = await (await fetch("/api/guardian-index")).json();
          // A newer poll started while this fetch was in flight — abandon this
          // one without touching `guardians`, whose fresher value belongs to it.
          if (seq !== reviewPollSeq) { console.debug("pollReviews: superseded, abandoning"); return; }
          // `fresh` is the lean `/api/guardian-index` shape -- carry forward
          // any full detail a prior `fetchGuardianDetail` already merged
          // onto the outgoing `guardians[]` entry (its `.branches` etc.),
          // rather than discarding it wholesale. Without this, the entry
          // reverts to lean-only on every single poll (this fires on every
          // SSE event, often every 1-2s), which made `renderReviewDetail`'s
          // "is full detail loaded?" check flip back to "no" and flash the
          // loading placeholder even for a review that was already fully
          // loaded and hasn't changed.
          const priorById = new Map(guardians.map((g) => [g.id, g]));
          guardians = /** @type {GuardianView[]} */ (fresh).map((lean) => {
            const prior = priorById.get(lean.id);
            return prior && prior.branches ? Object.assign({}, prior, lean) : lean;
          });
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
          /**
           * Renders now, with whatever's already loaded (the lean guardian
           * list itself, plus any detail/branch/PR/conflict data cached
           * from a previous poll) -- don't make the whole tab wait on the
           * per-selected-guardian refreshes below. `pollPullRequests` hits
           * `GET /api/pull-requests/{id}/sync-status`, which does a real
           * `git fetch` and can take many seconds per open PR (serialized
           * per repo on the daemon side) -- the list/sidebar has no reason
           * to sit blank that whole time when its own data already
           * arrived. A `function` declaration (not `const`) so it's
           * hoisted and safe to call from the async detail-fetch callback
           * below, wherever that callback happens to land relative to this
           * point in the function body.
           * @returns {void}
           */
          function renderIfNotSelecting() {
            renderReviews();
            const detail = document.getElementById("review-detail");
            if (selectionWithin(detail)) return; // keep the user's in-pane selection intact
            preserveUserState(detail, renderReviewDetail);
          }
          renderIfNotSelecting();
          // Keep every expanded branch's feedback thread fresh (RAL-272) so a
          // guardian's async acknowledgment appears without a manual refresh,
          // and fetch this guardian's full detail (RAL-121: also the
          // daemon's "the user is looking at this one" signal -- promotes
          // its preliminary change-summary job to high priority instead of
          // every review in the list computing one eagerly). These are
          // independent of each other (each caches into its own state and
          // none reads another's result), so they run concurrently
          // (RAL-234) instead of as a sequential await chain -- the old
          // chained-await version summed all round-trips instead of taking
          // the max of them. Folding `fetchGuardianDetail` into this same
          // batch (not a separate, uncoordinated fetch+render) means the
          // detail pane updates once, coherently, when everything lands --
          // three independent chains each re-rendering on their own used to
          // race and visibly flash back to the loading placeholder.
          if (selectedGuardian) {
            const gid = selectedGuardian;
            await Promise.all([
              fetchGuardianDetail(gid),
              refreshExpandedBranchMessages(gid, { silent: true }),
              pollBranchConflicts(gid),
              pollPullRequests(gid),
              pollPrErrors(gid),
            ]);
            // The awaited refreshes may have been overtaken by a newer poll
            // (or re-selection) — a stale render now would show old data.
            if (seq !== reviewPollSeq) { console.debug("pollReviews: superseded, abandoning"); return; }
            // Re-render now that the slower per-branch/PR/detail data has landed.
            renderIfNotSelecting();
          }
        } catch (e) { byId("conn").className = "dot off"; }
      }
      // RALPHUS-REVIEW-POLL:END
