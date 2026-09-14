      // ---------- RAL-414: dormant-by-default cold-navigation timing ----------
      /**
       * Mirrors `daemon/src/perf_timing.rs`'s opt-in shape: disabled by
       * default, turned on with `?ralphusTiming=1` in the URL (persisted to
       * `localStorage` so a bookmarked/reloaded board keeps it on, and
       * `?ralphusTiming=0` turns it back off), and cheap when off --
       * [`timeNav`] just calls straight through to the wrapped function with
       * no extra Promise chain or `performance.now()` call.
       *
       * When enabled, each board-tab navigation (`showTab` -> `tick()`) is
       * recorded as one history entry: total elapsed time, the specific
       * tab-driving fetch's duration (read from the browser's own Resource
       * Timing entries, not hand-instrumented per poll function), and that
       * fetch's `Server-Timing` breakdown (populated automatically by the
       * browser from the response header `crate::perf_timing` emits) -- so
       * server and client phases show up side by side with no bespoke wire
       * format. `renderMs` is derived as `totalMs - fetchMs` rather than
       * measured directly: every poll function fetches and renders inline,
       * so splitting that out would mean threading a timer through every one
       * of them; the browser's own Resource Timing entry already gives the
       * fetch boundary for free.
       *
       * Test suites (e.g. `cli-py/tests/test_board_cold_load_perf.py`) read
       * `window.RalphusTiming.getLast(tab)` via `page.evaluate` after a cold
       * navigation completes.
       * @returns {boolean}
       */
      function perfTimingReadEnabled() {
        try {
          const params = new URLSearchParams(location.search);
          if (params.get("ralphusTiming") === "1") {
            localStorage.setItem("ralphusTiming", "1");
            return true;
          }
          if (params.get("ralphusTiming") === "0") {
            localStorage.removeItem("ralphusTiming");
            return false;
          }
          return localStorage.getItem("ralphusTiming") === "1";
        } catch (_e) {
          // Storage/URL access can throw in a locked-down embed; timing is a
          // diagnostic, never worth breaking the board over.
          return false;
        }
      }
      const perfTimingEnabled = perfTimingReadEnabled();
      /** @type {Record<string, Array<object>>} per-tab ring buffer of recorded navigations. */
      const perfTimingHistory = {};
      /** Ring-buffer size per tab -- enough for a short manual session without growing unbounded. */
      const PERF_TIMING_MAX_ENTRIES_PER_TAB = 20;
      /**
       * The endpoint whose Resource Timing entry represents "the tab's own
       * data load" -- the same endpoints `daemon/src/perf_timing.rs` names
       * `"tasks"`/`"task-index"`/`"worktree-retirements"`/`"projects"` in its
       * `Server-Timing` header. Every other tab still gets a `totalMs` figure
       * (see [`perfTimingRecordNav`]) but no `fetchMs`/`server` breakdown,
       * since its endpoint isn't instrumented server-side.
       * @type {Record<string, string>}
       */
      const PERF_TIMING_TAB_ENDPOINTS = {
        squads: "/api/tasks",
        tasks: "/api/task-index",
        "worktree-retirement": "/api/worktree-retirements",
        projects: "/api/projects",
      };
      /**
       * Parses a `PerformanceResourceTiming` entry's `serverTiming` array
       * (the browser's own parse of the response's `Server-Timing` header)
       * into plain `{name, duration, description}` objects, since
       * `serverTiming` itself is a live host-object array some environments
       * don't serialize cleanly (e.g. through `page.evaluate`'s JSON
       * round-trip).
       * @param {PerformanceResourceTiming} entry
       * @returns {Array<{name: string, duration: number, description: string}>}
       */
      function perfTimingServerPhases(entry) {
        if (!entry.serverTiming) return [];
        return Array.from(entry.serverTiming).map((s) => ({
          name: s.name,
          duration: s.duration,
          description: s.description,
        }));
      }
      /**
       * Finds the most recent Resource Timing entry for `tab`'s own endpoint
       * that started at or after `navStart` -- i.e. the fetch this specific
       * navigation triggered, not a stale one from a prior visit to the same
       * tab or the periodic SSE-reconciliation poll.
       * @param {string} navTab
       * @param {number} navStart
       * @returns {PerformanceResourceTiming|null}
       */
      function perfTimingFindNavEntry(navTab, navStart) {
        const endpoint = PERF_TIMING_TAB_ENDPOINTS[navTab];
        if (!endpoint) return null;
        const entries = /** @type {PerformanceResourceTiming[]} */ (
          performance.getEntriesByType("resource")
        );
        let best = null;
        for (const entry of entries) {
          if (entry.startTime < navStart) continue;
          if (!entry.name.includes(endpoint)) continue;
          if (!best || entry.startTime > best.startTime) best = entry;
        }
        return best;
      }
      /**
       * Records one navigation's timing under `navTab`. Called only from
       * [`timeNav`], which already checked `perfTimingEnabled`.
       * @param {string} navTab
       * @param {number} navStart `performance.now()` at the top of the navigation.
       * @returns {void}
       */
      function perfTimingRecordNav(navTab, navStart) {
        const totalMs = performance.now() - navStart;
        const navEntry = perfTimingFindNavEntry(navTab, navStart);
        const entry = {
          tab: navTab,
          totalMs,
          fetchMs: navEntry ? navEntry.duration : null,
          renderMs: navEntry ? Math.max(0, totalMs - navEntry.duration) : null,
          server: navEntry ? perfTimingServerPhases(navEntry) : [],
          ts: Date.now(),
        };
        const list = perfTimingHistory[navTab] || (perfTimingHistory[navTab] = []);
        list.push(entry);
        if (list.length > PERF_TIMING_MAX_ENTRIES_PER_TAB) list.shift();
      }
      /**
       * Wraps a board-tab navigation's async work (`tick()`) with timing,
       * when enabled. Disabled, this is exactly `fn()` -- no extra Promise,
       * no `performance.now()` call, so `showTab` pays nothing on the hot
       * path.
       * @param {string} navTab
       * @param {() => Promise<void>} fn
       * @returns {Promise<void>}
       */
      function perfTimingTimeNav(navTab, fn) {
        if (!perfTimingEnabled) return fn();
        const navStart = performance.now();
        return fn().then((v) => {
          perfTimingRecordNav(navTab, navStart);
          return v;
        });
      }
      /**
       * Dormant-by-default cold-navigation timing (RAL-414). See the module
       * doc comment above [`perfTimingReadEnabled`] for the full design.
       */
      const RalphusTiming = {
        enabled: perfTimingEnabled,
        timeNav: perfTimingTimeNav,
        /**
         * The most recently recorded navigation entry for `getLastTab`, or
         * `null` if timing is disabled or that tab has never been navigated
         * to while enabled.
         * @param {string} getLastTab
         * @returns {object|null}
         */
        getLast(getLastTab) {
          const list = perfTimingHistory[getLastTab];
          return list && list.length ? list[list.length - 1] : null;
        },
        /**
         * Clears recorded history for `resetTab`, or every tab when
         * omitted -- use before a navigation a test wants to measure in
         * isolation from earlier ones in the same page session.
         * @param {string} [resetTab]
         * @returns {void}
         */
        reset(resetTab) {
          if (resetTab) delete perfTimingHistory[resetTab];
          else for (const k of Object.keys(perfTimingHistory)) delete perfTimingHistory[k];
        },
      };
      // `window` has no `RalphusTiming` property in the DOM lib types --
      // this is a page-defined global, read back by test suites via
      // `page.evaluate(() => window.RalphusTiming...)`, not a browser API.
      /** @type {*} */ (window).RalphusTiming = RalphusTiming;
