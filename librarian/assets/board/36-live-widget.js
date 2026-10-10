      // ---- Shared live-view widget (RAL-605) ----
      // One component renders a step's live view in both places that show one:
      // the review inspector's Live tab and the squad details pane. It lists
      // every run/attempt of the step (history bar, step back/forward,
      // "Latest") and offers Terminal, Prompt, System Prompt and Agent Log
      // views over whichever one is selected. A caller describes itself with a
      // `LiveViewCtx` (runs, how a run maps to a peek key, how its prompt is
      // drawn); everything else -- per-scope state, actions, controls -- lives
      // here. Nothing in this file fetches by itself except the System Prompt
      // text a user asks for, and a repaint never re-fetches.

      /**
       * What a caller hands `liveViewWidget`.
       * @typedef {object} LiveViewCtx
       * @property {string} scope - Unique per widget instance; keys the run/sub-view state and the actions' `data-scope`.
       * @property {string} title - The identity line.
       * @property {BranchRun[]} runs - Every run, oldest first.
       * @property {() => void} repaint - Repaints the pane that owns this widget.
       * @property {boolean} canThink - Whether the Thinking toggle applies to this agent.
       * @property {(run: BranchRun|null) => string} keyFor - The peek key for a run (null when there are none yet).
       * @property {string} systemKey - The step-level peek key the System Prompt is fetched under.
       * @property {string} closeKey - Peek key a "Hide" button toggles, or "" for no such button.
       * @property {string} pickerAttrs - Attributes making the run picker a button, or "" for a plain label.
       * @property {string} pickerTip - Tooltip for the run picker.
       * @property {string} noRunsText - Shown when there are no runs.
       * @property {string} noRunsTip - Tooltip for the no-runs bar.
       * @property {(run: BranchRun|null) => string} promptView - The Prompt sub-view's HTML.
       * @property {(run: BranchRun|null, key: string, isLatest: boolean) => string} terminalFoot - Buttons under the terminal.
       * @property {string} systemFoot - Buttons under the system prompt.
       */
      /**
       * The widgets most recently painted, by scope, so a click that carries
       * only `data-scope` can find the context (and repaint callback) it
       * belongs to.
       * @type {{[scope: string]: LiveViewCtx}}
       */
      const liveWidgets = {};
      /**
       * Which run each widget is walked back to, per scope. Absent (or past
       * the end) means the newest run -- the live one.
       * @type {{[scope: string]: number}}
       */
      const liveRunIdx = {};
      /**
       * Which sub-view each widget shows, per scope: terminal | prompt | system | agentlog.
       * @type {{[scope: string]: string}}
       */
      const liveSub = {};
      /** @type {{[sub: string]: string}} What each sub-view shows. */
      const LIVE_SUB_TIP = {
        terminal: "The run's captured terminal output.\nThe newest run streams; an earlier one is the daemon's persisted record of it.",
        prompt: "The instruction this run's agent was given — the task it was asked to do, as opposed to the standing rules it works under.",
        agentlog: "Experimental. The run's tool calls as collapsible widgets with their input and output, filterable by tool, status and text.\nRebuilt from the terminal text, so it is as complete as that text is. Secrets are masked.",
        system: "The exact system prompt this run's agent received: ralphus's hidden instructions plus the authored prompt.\nRead-only reference — changing it means changing where it is authored.\nAdmin-only view.",
      };
      /**
       * Formats a run's "05:12:30 · passed" metadata line.
       * @param {BranchRun} run - The run.
       * @returns {string}
       */
      function runMeta(run) {
        if (run.meta !== undefined) return run.meta;
        const t = run.start_at_ms
          ? new Date(run.start_at_ms).toLocaleTimeString([], { hour12: false, hour: "2-digit", minute: "2-digit", second: "2-digit" })
          : "";
        // Duration only where the daemon measured one -- a gate's elapsed_ms.
        // Nothing here is derived from wall-clock guesses.
        const dur = run.elapsed_ms ? ` · ${fmtDurationMs(run.elapsed_ms)}` : "";
        const note = run.start_recorded === false ? " · start not recorded" : "";
        return (run.outcome ? `${t} · ${run.outcome}` : t) + dur + note;
      }
      /**
       * Formats a measured duration the way the rest of the board does.
       * @param {number} ms - Milliseconds.
       * @returns {string}
       */
      function fmtDurationMs(ms) {
        const secs = Math.round(ms / 1000);
        if (secs < 60) return `${secs}s`;
        return `${Math.floor(secs / 60)}m ${secs % 60}s`;
      }
      /**
       * Which run index a widget is showing, clamped to the list it actually
       * has (runs grow as the step works).
       * @param {BranchRun[]} runs - The widget's runs.
       * @param {string} scope - The widget's scope.
       * @returns {number}
       */
      function curRunIdx(runs, scope) {
        if (!runs.length) return -1;
        const want = liveRunIdx[scope];
        if (want === undefined) return runs.length - 1;
        return Math.max(0, Math.min(runs.length - 1, want));
      }
      /**
       * Repaints the widget registered under `scope`.
       * @param {string} scope - The widget's scope.
       * @returns {void}
       */
      function repaintLive(scope) {
        const ctx = liveWidgets[scope];
        if (ctx) ctx.repaint();
      }
      /**
       * Steps a widget one run backwards or forwards.
       * @param {string} scope - The widget's scope.
       * @param {number} dir - -1 for the previous run, 1 for the next.
       * @returns {void}
       */
      function stepLiveRun(scope, dir) {
        const ctx = liveWidgets[scope];
        if (!ctx || !ctx.runs.length) return;
        liveRunIdx[scope] = Math.max(0, Math.min(ctx.runs.length - 1, curRunIdx(ctx.runs, scope) + dir));
        ctx.repaint();
      }
      /**
       * Jumps a widget back to its newest run -- the live one.
       * @param {string} scope - The widget's scope.
       * @returns {void}
       */
      function jumpToLatestLiveRun(scope) {
        delete liveRunIdx[scope];
        repaintLive(scope);
      }
      /**
       * Switches a widget's sub-view.
       * @param {string} scope - The widget's scope.
       * @param {string} sub - terminal | prompt | system | agentlog.
       * @returns {void}
       */
      function setLiveSub(scope, sub) {
        liveSub[scope] = sub;
        repaintLive(scope);
        const ctx = liveWidgets[scope];
        // Fetched on first open and dropped on leaving, like every other
        // lazily-loaded pane -- reopening refetches rather than caching.
        if (sub === "system" && ctx) ensurePeekSystemPrompt(ctx.systemKey).then(() => repaintLive(scope));
      }
      /**
       * The live-view widget: walk back through every run of a step and read
       * whichever one you land on. Nothing attaches until a caller renders it
       * -- callers gate this on the user having opened the view.
       * @param {LiveViewCtx} ctx - What to show and how the caller maps a run to a peek key.
       * @returns {string}
       */
      function liveViewWidget(ctx) {
        liveWidgets[ctx.scope] = ctx;
        const runs = ctx.runs;
        const ri = curRunIdx(runs, ctx.scope);
        const run = ri >= 0 ? runs[ri] : null;
        const key = ctx.keyFor(run);
        // The key names the shown run, so it changes as runs load or are
        // stepped through; whichever one is on screen is the one polled.
        if (!peekOpen[key]) {
          peekOpen[key] = true;
          setTimeout(() => fetchPeek(key, true), 0);
        }
        const isLatest = ri === runs.length - 1;
        const sub = liveSub[ctx.scope] || "terminal";
        const ended = !!peekEnded[key];
        // Whether the pane still holds this run's text at all, separate from
        // whether that text is still moving: the pane is reused between passes,
        // so only the newest one's output is still in it. A finished session's
        // record is not "historical" in the sense the walk-back note means --
        // it is this run's own output, just no longer growing, and the liveness
        // dot and the pane's own banner already say so.
        const onNewestTape = !!run && isLatest;
        const live = onNewestTape && !ended;
        const sc = esc(ctx.scope);

        // 1 - who am I looking at, and is it still moving
        const closeBtn = ctx.closeKey
          ? `<button class="btn" style="padding:1px 7px;font-size:11px" data-click="togglePeek" data-key="${esc(ctx.closeKey)}"
              data-tip="Collapse this live view. Reopening it fetches the output again.">✕ Hide</button>`
          : "";
        const identity = `<div class="peek-head" style="margin-bottom:8px">
            <span class="peek-dot${live ? "" : " ended"}"></span>
            <span class="mono" style="font-size:11px">${esc(ctx.title)}</span>
            <span style="flex:1"></span>
            <span class="rg-sub" style="font-size:11px;color:var(--faint)">${live ? "streaming" : (isLatest ? "session ended" : "historical")}</span>
            <button class="copy-btn" data-click="copyPeekText" data-key="${esc(key)}" data-scope="${sc}" data-tip="Copy what this tab is currently showing.">⧉</button>
            ${closeBtn}
          </div>`;

        // 2 - which run
        const pickerInner = run
          ? `<span class="hkind ${esc(run.kind)}">${esc(run.kind)}</span>
              <span class="hlabel mono">${esc(run.label)}</span>
              <span class="hmeta">${esc(runMeta(run))}</span>
              <span class="hcount num">${ri + 1}/${runs.length}</span>${ctx.pickerAttrs ? " &#9662;" : ""}`
          : "";
        const histBar = runs.length && run ? `<div class="histbar${onNewestTape ? "" : " past"}">
            <button class="hnav" data-click="stepLiveRun" data-scope="${sc}" data-dir="-1" ${ri <= 0 ? "disabled" : ""}
              data-tip="Step back to the previous run.">&#9664;</button>
            ${ctx.pickerAttrs
    ? `<button class="hpick" ${ctx.pickerAttrs} data-tip="${esc(ctx.pickerTip)}">${pickerInner}</button>`
    : `<span class="hpick" data-tip="${esc(ctx.pickerTip)}">${pickerInner}</span>`}
            <button class="hnav" data-click="stepLiveRun" data-scope="${sc}" data-dir="1" ${ri >= runs.length - 1 ? "disabled" : ""}
              data-tip="Step forward to the next run.">&#9654;</button>
            ${isLatest ? "" : `<button class="btn" style="padding:3px 8px;font-size:11px" data-click="jumpToLatestLiveRun" data-scope="${sc}"
              data-tip="Jump back to the newest run — the one still streaming.">&#8677; Latest</button>`}
          </div>
          ${onNewestTape ? "" : `<div class="histnote">You are walked back to ${esc(run.label)}
            from ${esc(runMeta(run))} — an earlier run than the newest one.</div>`}`
          : `<div class="histbar"><span class="hpick" data-tip="${esc(ctx.noRunsTip)}">
              <span class="hkind">no runs</span><span class="hmeta">${esc(ctx.noRunsText)}</span></span></div>`;

        // 3 - how it is rendered. Terminal-only controls, but they stay put on
        //     the prompt views rather than vanishing: appearing and disappearing
        //     on every tab swap reflows the pane. aria-disabled, not `disabled`,
        //     so the tooltip explaining why still fires.
        const showDebug = peekShowsDebug(key);
        const showThinking = peekShowsThinking(key);
        const dead = sub !== "terminal";
        const deadTip = "Applies to the Terminal view.\nSwitch to Terminal to use it.";
        const ctl = `<div class="peek-ctl">
            <button class="tgl ${showDebug ? "on" : ""}${dead ? " off" : ""}" ${dead ? 'aria-disabled="true"' : ""}
              data-click="toggleShowDebugMessagesBtn" data-key="${esc(key)}" data-scope="${sc}"
              data-tip="${dead ? deadTip : "Show ralphus's own diagnostic/telemetry events inline, where they happened.\nOff by default so routine monitoring shows what the agent did.\nThis only changes what is rendered here — the daemon's logs always keep everything."}">
              <span class="bx"></span>Debug messages</button>
            <input class="typefilter${dead ? " dead" : ""}" placeholder="Filter types (e.g. read glob or -usage)"
              value="${esc(peekTypeFilterInput[key] || "")}" ${dead ? "disabled" : ""}
              oninput="setPeekTypeFilter('${esc(key)}',this.value)" aria-label="Filter log types"
              data-tip="${dead ? `${deadTip}\n${LIVE_VIEW_TYPE_FILTER_TIP}` : LIVE_VIEW_TYPE_FILTER_TIP}">
            ${ctx.canThink ? `<button class="tgl ${showThinking ? "on" : ""}${dead ? " off" : ""}" ${dead ? 'aria-disabled="true"' : ""}
              data-click="toggleShowThinkingBtn" data-key="${esc(key)}" data-scope="${sc}"
              data-tip="${dead ? deadTip : "Show the model's own reasoning expanded inline.\nOff folds each block to a single &lt;thinking…&gt; line.\nPurely a display choice — the reasoning is always captured, so toggling re-renders text already loaded without refetching."}">
              <span class="bx"></span>Thinking</button>` : ""}
          </div>`;

        // 4 - which view, seated directly on what it switches
        const subs = [["terminal", "Terminal"], ["prompt", "Prompt"], ["system", "System Prompt"], ["agentlog", "Agent Log (experimental)"]];
        const tabs = `<div class="subtabs">${subs.map((s) => `<button class="subtab ${sub === s[0] ? "on" : ""}" `
          + `data-click="setLiveSub" data-scope="${sc}" data-sub="${s[0]}" `
          + `data-tip="${esc(LIVE_SUB_TIP[s[0]])}">${s[1]}</button>`).join("")}</div>`;

        const top = identity + histBar + ctl + tabs;
        // The system prompt is per step, not per run: `setLiveSub` fetches it under the step-level key.
        if (sub === "system") return top + liveSystemPromptView(ctx);
        if (sub === "prompt") return top + ctx.promptView(run);
        if (sub === "agentlog") return top + liveAgentLogView(key);
        return top + liveTerminalView(ctx, key, run, onNewestTape);
      }
      /**
       * The widget's Terminal view: the run's captured output, framed, with
       * the controls that act on it underneath.
       * @param {LiveViewCtx} ctx - The widget's context.
       * @param {string} key - The peek key.
       * @param {BranchRun|null} run - The run being shown.
       * @param {boolean} isLatest - Whether that run is the newest one that used
       *   a tmux pane, and so the only one whose text the live view still holds.
       * @returns {string}
       */
      function liveTerminalView(ctx, key, run, isLatest) {
        const shown = peekContent[key];
        return `<div style="position:relative"><div class="runterm" id="peek-pre-${peekCssKey(key)}" style="height:${peekPaneHeight}px" tabindex="0" data-key="${esc(key)}"
            onscroll="onPeekScroll(this.dataset.key)" onkeydown="handlePeekKeydown(event,this.dataset.key)"
            data-tip="Scroll through this run's output.\nClick here then press Ctrl+End to jump to the latest, or Ctrl+Home for the start.">${shown !== undefined ? transcriptHtml(shown) : "Loading…"}</div>
          <button id="peek-jump-${peekCssKey(key)}" class="peek-jump-btn" style="display:none" data-click="peekScrollToBottom" data-key="${esc(key)}" data-tip="Jump to the latest output.\nAppears once you've scrolled up from the bottom — also triggerable with Ctrl+End while the terminal is focused.">↓ Jump to latest</button></div>${ctx.terminalFoot(run, key, isLatest)}
          <div class="hint">Read-only — nothing typed here reaches the agent. Every run's text is
          captured separately, so walking back survives a restart.</div>`;
      }
      // RALPHUS-PROMPTBOX-SCROLL:BEGIN
      /** @type {{[scrollKey: string]: number}} Scroll offset of each live-view prompt box, surviving repaints. */
      const promptBoxScroll = {};
      /**
       * A scrollable live-view prompt box whose scroll offset is remembered, so
       * a periodic repaint does not send the reader back to the top.
       * @param {string} scrollKey - Identifies this box's content across repaints.
       * @param {string} text - The plain text to show.
       * @returns {string}
       */
      function reviewPromptBox(scrollKey, text) {
        return `<div class="promptbox" data-scroll-key="${esc(scrollKey)}" onscroll="savePromptBoxScroll(this)">${esc(text)}</div>`;
      }
      /**
       * Remembers a prompt box's scroll offset.
       * @param {HTMLElement} el - The `.promptbox` that scrolled.
       * @returns {void}
       */
      function savePromptBoxScroll(el) {
        const k = el.dataset.scrollKey;
        if (k) promptBoxScroll[k] = el.scrollTop;
      }
      /**
       * Re-applies remembered offsets to freshly painted prompt boxes.
       * @returns {void}
       */
      function restorePromptBoxScroll() {
        document.querySelectorAll(".promptbox[data-scroll-key]").forEach((el0) => {
          const el = /** @type {HTMLElement} */ (el0);
          const top = promptBoxScroll[el.dataset.scrollKey || ""];
          if (top) el.scrollTop = top;
        });
      }
      // RALPHUS-PROMPTBOX-SCROLL:END
      /**
       * The widget's System Prompt view.
       * @param {LiveViewCtx} ctx - The widget's context.
       * @returns {string}
       */
      function liveSystemPromptView(ctx) {
        if (!currentUserIsAdmin) {
          return `<div class="absent-run">
              <div class="abody"><div class="at">Admin-only view</div>
                <div class="as">The effective system prompt is shown to administrators only.</div></div>
            </div>`;
        }
        const ps = peekSystemPrompt[ctx.systemKey];
        const body = ps === undefined || ps === "loading" ? "Loading…" : peekPromptDisplay(ps);
        return `${reviewPromptBox(`${ctx.scope}:${ctx.systemKey}:system`, body)}
          ${ctx.systemFoot}
          <div class="hint">Read-only — the effective system prompt actually appended to this agent
          invocation: ralphus's hidden instructions plus the authored prompt.</div>`;
      }
      // ---- Squad-side adapter ----
      // A squad cell or proof step has no run list of its own; its runs are the
      // terminal-log attempts the daemon persisted for it. They are listed
      // once, when the widget is first opened, and the newest one stands for
      // the live pane (the plain peek key). An earlier attempt is read through
      // a key pinned to it (`base|a<N>`, see `splitPeekAttempt`).
      /** @type {{[baseKey: string]: AttemptMeta[]|"loading"}} Persisted attempts per step, fetched when its widget opens and dropped when it closes. */
      const liveAttempts = {};
      /**
       * Fetches a step's persisted attempts the first time its widget is
       * painted, then repaints the pane that owns it.
       * @param {string} baseKey - The step's plain peek key.
       * @returns {Promise<void>}
       */
      async function loadLiveAttempts(baseKey) {
        if (liveAttempts[baseKey] !== undefined) return;
        liveAttempts[baseKey] = "loading";
        const url = terminalLogAttemptsUrlFor(baseKey);
        /** @type {AttemptMeta[]} */
        let list = [];
        try {
          const resp = url ? await fetch(url) : null;
          if (resp && resp.ok) list = (await resp.json()).attempts || [];
        } catch (_) {
          list = [];
        }
        // Closed while the request was in flight: leave nothing behind.
        if (!peekOpen[baseKey]) { delete liveAttempts[baseKey]; return; }
        liveAttempts[baseKey] = list;
        rerenderOwningPane();
      }
      /**
       * Builds one run row for the squad adapter.
       * @param {string} id - "current", or `a<N>` for a pinned earlier attempt.
       * @param {string} kind - Picker badge.
       * @param {string} label - Picker label.
       * @param {string} meta - Picker metadata line.
       * @returns {BranchRun}
       */
      function squadLiveRun(id, kind, label, meta) {
        return { id, kind, label, meta, start_at_ms: 0, end_at_ms: null, elapsed_ms: 0, outcome: "", agent: "", start_recorded: true };
      }
      /**
       * The runs of a squad step: its earlier persisted attempts, then the
       * current one. Before the list has loaded (or when nothing is persisted
       * yet) the current run is the only one.
       * @param {string} baseKey - The step's plain peek key.
       * @param {boolean} withAttempts - False for a step whose daemon endpoint has no per-attempt reads.
       * @param {string} currentMeta - Metadata line for the current run.
       * @returns {BranchRun[]}
       */
      function squadLiveRuns(baseKey, withAttempts, currentMeta) {
        const stored = withAttempts ? liveAttempts[baseKey] : undefined;
        const persisted = Array.isArray(stored) ? stored : [];
        const when = (/** @type {AttemptMeta} */ a) => `${fmtAttemptSize(a.size_bytes)} · ${new Date(a.modified_ms).toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" })}`;
        const earlier = persisted.slice(0, -1).map((a) => squadLiveRun(`a${a.attempt}`, a.attempt === 0 ? "initial" : "reattach", `attempt ${a.attempt}`, when(a)));
        const newest = persisted.length ? persisted[persisted.length - 1] : null;
        const label = newest ? `attempt ${newest.attempt} · current` : "current run";
        return [...earlier, squadLiveRun("current", "current", label, currentMeta)];
      }
      /**
       * Describes a squad cell, proof step or manual-checks run to the widget.
       * @param {object} o - What this step is.
       * @param {string} o.baseKey - The step's plain peek key; also the widget's scope.
       * @param {string} o.title - The identity line.
       * @param {boolean} o.canThink - Whether the Thinking toggle applies.
       * @param {string} o.promptText - The step's authored instruction or command, "" when there is none.
       * @param {string} o.promptHint - What that text is, for the Prompt tab's footnote.
       * @param {boolean} o.withAttempts - Whether earlier attempts can be read.
       * @param {string} o.currentMeta - Metadata line for the current run.
       * @param {string} o.closeKey - Peek key a Hide button toggles, or "".
       * @returns {LiveViewCtx}
       */
      function squadLiveCtx(o) {
        // Lazy: nothing is requested for a step whose live view is closed.
        if (o.withAttempts && peekOpen[o.baseKey]) loadLiveAttempts(o.baseKey);
        return {
          scope: o.baseKey,
          title: o.title,
          runs: squadLiveRuns(o.baseKey, o.withAttempts, o.currentMeta),
          repaint: rerenderOwningPane,
          canThink: o.canThink,
          keyFor: (run) => (run && run.id !== "current" ? `${o.baseKey}|${run.id}` : o.baseKey),
          systemKey: o.baseKey,
          closeKey: o.closeKey,
          pickerAttrs: "",
          pickerTip: "The run you are looking at. Step with the arrows to read an earlier attempt; each attempt's log was persisted separately.",
          noRunsText: "no runs yet",
          noRunsTip: "This step has not produced a run yet.",
          promptView: () => (o.promptText
            ? `${reviewPromptBox(`${o.baseKey}:prompt`, o.promptText)}
              <div class="hint">${esc(o.promptHint)}</div>`
            : `<div class="absent-run">
                <div class="abody"><div class="at">No authored instruction</div>
                  <div class="as">This step has no prompt or command text of its own to show.</div></div>
              </div>`),
          terminalFoot: () => "",
          systemFoot: "",
        };
      }
      /**
       * Drops a squad step's widget state when it is closed, so the next
       * open starts from the newest run and refetches the attempt list.
       * @param {string} baseKey - The step's plain peek key.
       * @returns {void}
       */
      function dropSquadLive(baseKey) {
        delete liveAttempts[baseKey];
        delete liveRunIdx[baseKey];
        delete liveSub[baseKey];
        delete liveWidgets[baseKey];
        // Earlier attempts opened through this step's widget are pinned keys of their own.
        const pinned = `${baseKey}|a`;
        for (const k of Object.keys(peekOpen)) {
          if (!k.startsWith(pinned) || splitPeekAttempt(k).base !== baseKey) continue;
          delete peekOpen[k];
          delete peekContent[k];
          delete peekTape[k];
          delete peekEnded[k];
          delete peekMissingStrikes[k];
          delete peekLastActivity[k];
        }
      }
      /**
       * The squad details pane's live-view slot for a step: the widget once
       * the user has opened it, nothing before (opening is `squadLiveOpenBtn`'s
       * job, and nothing is fetched until then).
       * @param {LiveViewCtx} ctx - The step's widget context.
       * @returns {string}
       */
      function squadLiveSlot(ctx) {
        return peekOpen[ctx.scope] ? `<div class="live-slot">${liveViewWidget(ctx)}</div>` : "";
      }
      /**
       * The button that opens a squad step's live view; empty while it is open
       * (the widget's own Hide button closes it).
       * @param {string} key - The step's plain peek key.
       * @param {string} label - What the step is called in the tooltip.
       * @returns {string}
       */
      function squadLiveOpenBtn(key, label) {
        if (peekOpen[key]) return "";
        return `<button class="btn primary" data-click="togglePeek" data-key="${esc(key)}" style="border-radius:6px 0 0 6px"
          data-tip="Open ${esc(label)}'s live view: its terminal, prompt and system prompt, with every attempt selectable.\nRead-only — nothing you do here is ever sent to the agent.">Live view</button>`;
      }
