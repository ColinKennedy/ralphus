      // ---- Tooltip engine (RAL-40) ----
      // Attach data-tip="..." to any element for a styled dark-theme tooltip.
      // Multi-line content: use \n in the attribute value.
      /** Wires up the global hover-tooltip engine driven by `data-tip="..."` attributes; self-invoking, no exports. */
      (function () {
        const tip = document.createElement("div");
        tip.id = "board-tip"; document.body.appendChild(tip);
        let mx = 0, my = 0;
        /** @type {HTMLElement|null} */
        let cur = null;
        /**
         * Repositions the tooltip element to track the last known mouse coordinates, flipping side/edge to stay on-screen.
         * @returns {void}
         */
        function reposition() {
          if (!cur) return;
          const w = tip.offsetWidth, h = tip.offsetHeight, pad = 14;
          let tx = mx + pad, ty = my - h - pad;
          if (tx + w > window.innerWidth - 8) tx = mx - w - pad;
          if (ty < 8) ty = my + pad;
          tip.style.left = tx + "px"; tip.style.top = ty + "px";
        }
        document.addEventListener("mousemove", (/** @type {MouseEvent} */ e) => { mx = e.clientX; my = e.clientY; reposition(); });
        document.addEventListener("mouseover", (/** @type {MouseEvent} */ e) => {
          const el = /** @type {HTMLElement|null} */ (/** @type {HTMLElement} */ (e.target).closest("[data-tip]"));
          if (!el) { if (cur) { cur = null; tip.classList.remove("show"); } return; }
          if (el === cur) return;
          cur = el; tip.textContent = el.dataset.tip ?? "";
          tip.classList.add("show"); reposition();
        });
        document.addEventListener("mouseout", (/** @type {MouseEvent} */ e) => {
          const el = /** @type {HTMLElement|null} */ (/** @type {HTMLElement} */ (e.target).closest("[data-tip]"));
          if (!el) return;
          const related = /** @type {HTMLElement|null} */ (e.relatedTarget);
          if (related && related.closest && related.closest("[data-tip]") === el) return;
          cur = null; tip.classList.remove("show");
        });
      })();

      // ---- Click/contextmenu delegation engine (RAL-231) ----
      // Structural fix for the onclick/oncontextmenu attributes that used to
      // interpolate ids/keys/state strings directly into inline JS source,
      // e.g. an onclick attribute built as onSquadClick(event,'<the id>') -- esc() cannot actually
      // defend that: the browser HTML-decodes an attribute value before the
      // JS parser compiles it, so an escaped quote is restored to a literal
      // one right before it could still terminate the string literal. The
      // fix removes string-interpolated JS from attribute values entirely:
      // elements instead carry data-click/data-ctx (an action key) plus
      // plain data-* fields, and one delegated listener per event type looks
      // the key up in CLICK_HANDLERS/CTX_HANDLERS and invokes it with
      // (event, el.dataset). Mirrors the data-tip engine above.
      // Registered with `capture: true` (not the tooltip engine's default
      // bubble phase): several existing document-level bubble listeners
      // close open menus on an "outside click" (closeSquadMenu,
      // closeGraphMenu, closeStatusPicker, closeQueueMenu, ...). A handler
      // here often calls e.stopPropagation() expecting to suppress those --
      // that only works if this dispatcher runs strictly before them, since
      // stopPropagation() does not stop sibling listeners already registered
      // on the same node (document), only later phases/ancestors. Capture
      // always runs before bubble on the same node, so stopPropagation()
      // called from here reliably cuts the event off before it ever reaches
      // those bubble-phase listeners, matching the old inline-onclick
      // behavior where the handler ran at the target itself, ahead of any
      // document-level bubble listener.
      /** @typedef {(e: MouseEvent, ds: DOMStringMap) => void} DelegatedHandler */
      /** @type {{[action: string]: DelegatedHandler}} */
      const CLICK_HANDLERS = {
        openSquadLogs: (e, ds) => openSquadLogs(e, ds.id || ""),
        openSquadTimeline: (e, ds) => openSquadTimeline(e, ds.id || ""),
        onSquadClick: (e, ds) => onSquadClick(e, ds.squadId || ""),
        openSquadMenu: (e, ds) => openSquadMenu(e, ds.squadId || ""),
        activateSquad: (e, ds) => activateSquad(e, ds.squadId || ""),
        renameSquad: (e, ds) => renameSquad(ds.squadId || ""),
        squadMenuActivate: (e, ds) => squadMenuAct(ds.squadId || "", "activate"),
        retrySquad: (e, ds) => retrySquad(ds.squadId || ""),
        restartSquad: (e, ds) => restartSquad(ds.squadId || ""),
        cancelSquad: (e, ds) => cancelSquad(ds.squadId || ""),
        openAddDependencyDialogFor: (e, ds) => openAddDependencyDialogFor(e, ds.squadId || ""),
        openStatusPickerForSquadMenuItem: (e, ds) => { e.stopPropagation(); closeSquadMenu(); openStatusPickerForSquad(e, ds.squadId || ""); },
        deleteSquad: (e, ds) => deleteSquad(ds.squadId || ""),
        openLogsFromSquadMenu: (e, ds) => { closeSquadMenu(); openLogs(ds.squadId || ""); },
        confirmRestart: (e, ds) => confirmRestart(ds.restartUrl || ""),
        confirmCancelSquad: (e, ds) => confirmCancelSquad(ds.cancelUrl || ""),
        toggleTerminalMenu: (e, ds) => toggleTerminalMenu(ds.key || ""),
        togglePeek: (e, ds) => togglePeek(ds.key || ""),
        togglePeekStopProp: (e, ds) => { togglePeek(ds.key || ""); e.stopPropagation(); },
        openAgentProofTerminalMenuItem: (e, ds) => { closeTerminalMenu(ds.key || ""); openAgentProofTerminal(ds.squadId || "", Number(ds.ti), ds.scope || "", Number(ds.si), Number(ds.vi)); },
        openProofTerminalMenuItem: (e, ds) => { closeTerminalMenu(ds.key || ""); openProofTerminal(ds.squadId || "", Number(ds.ti), ds.scope || "", Number(ds.si), Number(ds.vi)); },
        toggleHistoryMenuItem: (e, ds) => { closeTerminalMenu(ds.key || ""); toggleHistory(ds.key || ""); },
        openAgentTerminalMenuItem: (e, ds) => { closeTerminalMenu(ds.key || ""); openAgentTerminal(ds.squadId || "", Number(ds.ti), Number(ds.si)); },
        remoteTerminalMenuItem: (e, ds) => { closeTerminalMenu(ds.key || ""); openRemoteTerminal(ds.squadId || "", Number(ds.ti), Number(ds.si)); },
        resumeAutomationMenuItem: (e, ds) => { closeTerminalMenu(ds.key || ""); resumeAutomation(ds.squadId || "", Number(ds.ti), Number(ds.si)); },
        openTerminalMenuItem: (e, ds) => { closeTerminalMenu(ds.key || ""); openTerminal(ds.squadId || "", Number(ds.ti), Number(ds.si)); },
        openGuardianBranchTerminalMenuItem: (e, ds) => { closeTerminalMenu(ds.key || ""); openGuardianBranchTerminal(ds.gid || "", ds.bid || "", ds.mode || ""); },
        openGuardianManualChecksTerminalMenuItem: (e, ds) => { closeTerminalMenu(ds.key || ""); openGuardianManualChecksTerminal(ds.gid || "", ds.mode || ""); },
        copyPeekText: (e, ds) => copyPeekText(e, ds.key || ""),
        peekScrollToBottom: (e, ds) => peekScrollToBottom(ds.key || ""),
        closeHistoryAttempt: (e, ds) => closeHistoryAttempt(ds.key || ""),
        viewHistoryAttempt: (e, ds) => viewHistoryAttempt(ds.key || "", Number(ds.attempt)),
        toggleHistory: (e, ds) => toggleHistory(ds.key || ""),
        doPickStatus: (e, ds) => doPickStatus(ds.state || ""),
        selectAddDependencyTarget: (e, ds) => selectAddDependencyTarget(ds.squadId || ""),
        focusSel: (e, ds) => focusSel(ds.squadId || ""),
        unpickSel: (e, ds) => unpickSel(ds.squadId || ""),
        onGraphNodeClick: (e, ds) => onGraphNodeClick(e, ds.squadId || "", /** @type {"task"|"cell"|"proof"} */ (ds.kind || ""), Number(ds.ti), Number(ds.si), Number(ds.vi)),
      };
      /** @type {{[action: string]: DelegatedHandler}} */
      const CTX_HANDLERS = {
        openSquadMenu: (e, ds) => openSquadMenu(e, ds.squadId || ""),
        openProofMenu: (e, ds) => { e.preventDefault(); e.stopPropagation(); openProofMenu(e, ds.squadId || "", Number(ds.ti), Number(ds.si), Number(ds.vi)); },
        openCellNodeMenu: (e, ds) => openCellNodeMenu(e, ds.squadId || "", Number(ds.ti), Number(ds.si)),
        openTaskNodeMenu: (e, ds) => openTaskNodeMenu(e, ds.squadId || "", Number(ds.ti)),
      };
      // RAL-382: stopPropagation before navigating — the Tasks table renders this
      // handler inside a task row whose inline onclick selects the task; capture-
      // phase stopPropagation keeps that row click from firing on top of the jump.
      CLICK_HANDLERS.gotoReview = (e, ds) => { e.preventDefault(); e.stopPropagation(); gotoReview(ds.guardianId || ""); };
      // RAL-382: the +N suffix on a multi-review task row — opens the Reviews tab's
      // full list without changing the selection, so the user picks which review to
      // open rather than the badge silently choosing one.
      CLICK_HANDLERS.openReviewList = (e, ds) => { e.stopPropagation(); showTab("reviews", true); };
      CLICK_HANDLERS.pick = (e, ds) => pick(/** @type {"squad"|"task"|"cell"|"proof"} */ (ds.kind || "squad"), ds.ti !== undefined ? Number(ds.ti) : 0, ds.si !== undefined ? Number(ds.si) : 0, ds.vi !== undefined ? Number(ds.vi) : -1);
      CLICK_HANDLERS.removeEnvOverrideAt = (e, ds) => removeEnvOverrideAt(ds.apiPath || "", ds.key || "");
      CLICK_HANDLERS.addEnvOverrideAt = (e, ds) => addEnvOverrideAt(ds.apiPath || "");
      CLICK_HANDLERS.editEnvOverrideAt = (e, ds) => editEnvOverrideAt(ds.apiPath || "", ds.key || "", ds.value || "");
      CLICK_HANDLERS.loadCumulativeCost = (e, ds) => loadCumulativeCost(ds.cartoKey || "", ds.squadId || "", ds.elId || "", ds.cartoKind || "cell");
      CLICK_HANDLERS.gotoReviewBranch = (e, ds) => gotoReviewBranch(ds.guardianId || "", ds.branch || "");
      CLICK_HANDLERS.restartCell = (e, ds) => restartCell(ds.squadId || "", Number(ds.ti), Number(ds.si));
      CLICK_HANDLERS.gotoSquadItem = (e, ds) => gotoSquadItem(ds.squadId || "", ds.kind || "", Number(ds.ti), Number(ds.si), Number(ds.vi));
      CLICK_HANDLERS.gotoTriageCandidate = (e, ds) => gotoSquadItem(ds.squadId || "", "cell", Number(ds.ti), Number(ds.si), -1);
      CLICK_HANDLERS.gotoRunningItem = (e, ds) => { gotoSquadItem(ds.squadId || "", ds.kind || "", Number(ds.ti), Number(ds.si), Number(ds.vi)); byId("running-menu").classList.add("hidden"); };
      CLICK_HANDLERS.gotoRunningReview = (e, ds) => {
        if (ds.branch) gotoReviewBranch(ds.guardianId || "", ds.branch);
        else gotoReview(ds.guardianId || "");
        byId("running-menu").classList.add("hidden");
      };
      CLICK_HANDLERS.gotoCartoSquad = (e, ds) => gotoCartoSquad(e, ds.squadId || "");
      CLICK_HANDLERS.gotoCartoGuardian = (e, ds) => gotoCartoGuardian(e, ds.guardianId || "");
      CLICK_HANDLERS.gotoCartoItem = (e, ds) => gotoCartoItem(e, ds.squadId || "", ds.kind || "", Number(ds.ti), Number(ds.si), Number(ds.vi));
      CLICK_HANDLERS.cartoSortBy = (e, ds) => cartoSortBy(ds.col || "");
      CLICK_HANDLERS.toggleCartoPayload = (e, ds) => toggleCartoPayload(Number(ds.id));
      CLICK_HANDLERS.ntRemoveFile = (e, ds) => ntRemoveFile(Number(ds.i));
      CLICK_HANDLERS.setLogsTab = (e, ds) => { logsTab = ds.tab || ""; renderLogs(); };
      CLICK_HANDLERS.showLogsCopyMenu = (e, ds) => showLogsCopyMenu(e, ds.tab || "");
      CLICK_HANDLERS.copyLogsAs = (e, ds) => copyLogsAs(e, ds.tab || "", ds.scope || "current");
      CLICK_HANDLERS.openLinkedOutputPopup = (e, ds) => openLinkedOutputPopup(ds.key || "");
      CLICK_HANDLERS.runSingleManualCheck = (e, ds) => runSingleManualCheck(ds.guardianId || "", Number(ds.i));
      CLICK_HANDLERS.runActionHint = (e, ds) => runActionHint(ds.guardianId || "", Number(ds.i));
      CLICK_HANDLERS.toggleCheckForm = (e, ds) => toggleCheckForm(ds.key || "");
      CLICK_HANDLERS.resolveCheckInput = (e, ds) => resolveCheckInput(ds.guardianId || "", ds.inputName || "");
      CLICK_HANDLERS.runCheckWithInputs = (e, ds) => runCheckWithInputs(/** @type {"manual"|"action"} */ (ds.kind || "manual"), ds.guardianId || "", Number(ds.i));
      CLICK_HANDLERS.gotoWorktreeCell = (e, ds) => { worktreeMenuOpen = {}; gotoSquadItem(ds.squadId || "", "cell", Number(ds.ti), Number(ds.si), -1); };
      CLICK_HANDLERS.toggleWorktreeMenu = (e, ds) => toggleWorktreeMenu(ds.key || "");
      CLICK_HANDLERS.selectGuardian = (e, ds) => selectGuardian(ds.guardianId || "");
      CLICK_HANDLERS.onReviewClick = (e, ds) => onReviewClick(e, ds.guardianId || "");
      CLICK_HANDLERS.openReviewMenu = (e, ds) => openReviewMenu(e, ds.guardianId || "");
      CTX_HANDLERS.openReviewMenu = (e, ds) => openReviewMenu(e, ds.guardianId || "");
      CLICK_HANDLERS.openEditReviewDetailsFromMenu = (e, ds) => { closeSquadMenu(); openEditReviewDetails(ds.guardianId || ""); };
      CLICK_HANDLERS.cancelReview = (e, ds) => cancelReview(ds.guardianId || "");
      CLICK_HANDLERS.reopenReview = (e, ds) => reopenReview(ds.guardianId || "");
      CLICK_HANDLERS.deleteReview = (e, ds) => deleteReview(ds.guardianId || "");
      CLICK_HANDLERS.hideSquadMenuItem = (e, ds) => setSquadHiddenFromMenu(ds.squadId || "", true);
      CLICK_HANDLERS.unhideSquadMenuItem = (e, ds) => setSquadHiddenFromMenu(ds.squadId || "", false);
      CLICK_HANDLERS.hideReviewMenuItem = (e, ds) => setReviewHidden(ds.guardianId || "", true);
      CLICK_HANDLERS.unhideReviewMenuItem = (e, ds) => setReviewHidden(ds.guardianId || "", false);
      CLICK_HANDLERS.toggleAgentInspect = (e, ds) => toggleAgentInspect(e, ds.tid || "");
      CLICK_HANDLERS.pullPrCommits = (e, ds) => pullPrCommits(ds.prId || "");
      CLICK_HANDLERS.submitPrStack = (e, ds) => submitPrStack(ds.guardianId || "");
      CLICK_HANDLERS.toggleBranch = (e, ds) => { e.stopPropagation(); toggleBranch(e, ds.guardianId || "", ds.branchId || ""); };
      CLICK_HANDLERS.toggleBranchEnabled = (e, ds) => { e.stopPropagation(); toggleBranchEnabled(ds.guardianId || "", ds.branch || ""); };
      CLICK_HANDLERS.dismissReenable = (e, ds) => { e.stopPropagation(); dismissReenable(ds.guardianId || "", ds.branchId || ""); };
      CLICK_HANDLERS.openMoveBranchMenu = (e, ds) => { e.stopPropagation(); openMoveBranchMenu(e, ds.guardianId || "", ds.branchId || ""); };
      CLICK_HANDLERS.selectBranchRow = (e, ds) => selectBranchRow(e, ds.guardianId || "", ds.branch || "");
      CLICK_HANDLERS.showChatCopyMenu = (e, ds) => showChatCopyMenu(e, ds.guardianId || "", ds.branchId || "");
      CLICK_HANDLERS.toggleChatBubble = (e, ds) => { e.stopPropagation(); toggleChatBubble(ds.key || ""); };
      CLICK_HANDLERS.gotoSquad = (e, ds) => { e.preventDefault(); gotoSquad(ds.squadId || ""); };
      CLICK_HANDLERS.selectProjectTab = (e, ds) => selectProjectTab(ds.guardianId || "", ds.project || "");
      CLICK_HANDLERS.saveReorder = (e, ds) => saveReorder(ds.guardianId || "");
      CLICK_HANDLERS.discardReorder = (e, ds) => discardReorder(ds.guardianId || "");
      CLICK_HANDLERS.mergeReview = (e, ds) => mergeReview(ds.guardianId || "", ds.status || "");
      CLICK_HANDLERS.stopMerge = (e, ds) => stopMerge(ds.guardianId || "");
      CLICK_HANDLERS.approveReview = (e, ds) => approveReview(ds.guardianId || "");
      CLICK_HANDLERS.syncPrReview = (e, ds) => syncPrReview(ds.guardianId || "");
      CLICK_HANDLERS.runAllManualChecks = (e, ds) => runAllManualChecks(ds.guardianId || "");
      CLICK_HANDLERS.toggleManualMenu = (e, ds) => toggleManualMenu(ds.guardianId || "");
      CLICK_HANDLERS.copyChatAs = (e, ds) => copyChatAs(ds.guardianId || "", ds.branchId || "", ds.format || "");
      CLICK_HANDLERS.doForceStart = (e, ds) => doForceStart(ds.guardianId || "");
      CLICK_HANDLERS.moveBranchTo = (e, ds) => moveBranchTo(ds.guardianId || "", ds.branchId || "", ds.targetId || "");
      CLICK_HANDLERS.dismissReady = (e, ds) => dismissReady(ds.guardianId || "");
      CLICK_HANDLERS.setResSort = (e, ds) => setResSort(ds.col || "");
      CLICK_HANDLERS.jumpToTask = (e, ds) => jumpToTask(ds.squadId || "", Number(ds.ti), Number(ds.si));
      CLICK_HANDLERS.checkMachine = (e, ds) => checkMachine(ds.scheme || "");
      CLICK_HANDLERS.removeMachine = (e, ds) => removeMachine(ds.scheme || "");
      CLICK_HANDLERS.removeTriageType = (e, ds) => removeTriageType(ds.name || "");
      CLICK_HANDLERS.savePoolThreshold = (e, ds) => savePoolThreshold(e, ds.project || "", ds.triageType || "");
      CLICK_HANDLERS.removeTriageSchedule = (e, ds) => removeTriageSchedule(ds.id || "");
      CLICK_HANDLERS.removeUser = (e, ds) => removeUser(ds.name || "");
      CLICK_HANDLERS.saveUserEdit = (e, ds) => saveUserEdit(ds.name || "");
      CLICK_HANDLERS.openUserMenu = (e, ds) => openUserMenu(e, ds.name || "");
      CLICK_HANDLERS.editUserProfile = (e, ds) => editUserProfile(ds.name || "");
      CLICK_HANDLERS.toggleUserAdmin = (e, ds) => toggleUserAdmin(ds.name || "", ds.admin || "0");
      CLICK_HANDLERS.unhidePrefItem = (e, ds) => unhidePrefItem(ds.kind || "", ds.id || "");
      CLICK_HANDLERS.toggleWatch = (e, ds) => toggleWatch(ds.entityUri || "");
      CLICK_HANDLERS.renameSecretEnvName = (e, ds) => renameSecretEnvName(ds.name || "");
      CLICK_HANDLERS.removeSecretEnvName = (e, ds) => removeSecretEnvName(ds.name || "");
      CLICK_HANDLERS.saveProjectEdit = (e, ds) => saveProjectEdit(ds.name || "");
      CLICK_HANDLERS.openProjectTriageThresholds = (e, ds) => openProjectTriageThresholds(ds.name || "");
      CLICK_HANDLERS.openProjectForksModal = (e, ds) => openProjectForksModal(ds.name || "");
      CLICK_HANDLERS.startProjectForkEdit = (e, ds) => startProjectForkEdit(ds.user || "");
      CLICK_HANDLERS.saveProjectForkEdit = (e, ds) => saveProjectForkEdit(ds.user || "");
      CLICK_HANDLERS.removeProjectFork = (e, ds) => removeProjectFork(ds.user || "");
      CLICK_HANDLERS.saveProjectTriageThreshold = (e, ds) => saveProjectTriageThreshold(e, ds.triageType || "");
      CLICK_HANDLERS.queueToggleCollapse = (e, ds) => queueToggleCollapse(e, ds.key || "");
      CLICK_HANDLERS.queueHeaderClick = (e, ds) => queueHeaderClick(e, ds.key || "");
      CTX_HANDLERS.queueHeaderMenu = (e, ds) => queueHeaderMenu(e, ds.kind || "", ds.squadId || "", Number(ds.ti), ds.key || "");
      CLICK_HANDLERS.gotoSquadItemStopProp = (e, ds) => { e.stopPropagation(); gotoSquadItem(ds.squadId || "", ds.kind || "", Number(ds.ti), Number(ds.si), Number(ds.vi)); };
      CLICK_HANDLERS.queueRowClick = (e, ds) => queueRowClick(e, ds.path || "");
      CTX_HANDLERS.queueMenu = (e, ds) => queueMenu(e, ds.path || "");
      CLICK_HANDLERS.openReviewLogs = (e, ds) => openReviewLogs(ds.guardianId || "");
      CLICK_HANDLERS.openReviewTitleMenu = (e, ds) => openReviewTitleMenu(e, ds.guardianId || "");
      CLICK_HANDLERS.openReviewPrStacks = (e, ds) => openReviewPrStacks(ds.guardianId || "");
      CLICK_HANDLERS.openEnvViewer = (e, ds) => { e.stopPropagation(); openEnvViewer(ds.apiPath || ""); };
      CLICK_HANDLERS.openEditReviewDetails = (e, ds) => openEditReviewDetails(ds.guardianId || "");
      CLICK_HANDLERS.addEnvOverrideRow = (e, ds) => addEnvOverrideRow(ds.scope || "", ds.branchId || "");
      CLICK_HANDLERS.removeEnvOverrideRow = (e, ds) => removeEnvOverrideRow(ds.scope || "", ds.branchId || "", Number(ds.i));
      CLICK_HANDLERS.overrideInheritedKey = (e, ds) => overrideInheritedKey(ds.scope || "", ds.branchId || "", ds.key || "", ds.value || "");
      CLICK_HANDLERS.autofixDefaultBranch = (e, ds) => autofixDefaultBranch(ds.project || "", ds.squadId || "", Number(ds.taskIdx));
      document.addEventListener("click", (/** @type {MouseEvent} */ e) => {
        const el = /** @type {HTMLElement|null} */ (/** @type {HTMLElement} */ (e.target).closest("[data-click]"));
        if (!el || !el.dataset.click) return;
        const handler = CLICK_HANDLERS[el.dataset.click];
        if (handler) handler(e, el.dataset);
      }, true);
      document.addEventListener("contextmenu", (/** @type {MouseEvent} */ e) => {
        const el = /** @type {HTMLElement|null} */ (/** @type {HTMLElement} */ (e.target).closest("[data-ctx]"));
        if (!el || !el.dataset.ctx) return;
        const handler = CTX_HANDLERS[el.dataset.ctx];
        if (handler) handler(e, el.dataset);
      }, true);

      const STATES = ["running","done","failed","pending","queued","cancelled","ignored"];
      const GUARDIAN_STATES = ["collecting","merging","merge_failed","merge_stopped","in_review","approved","cancelled","deployed"];
      const SQUAD_STATES = ["queued","pending","running","done","failed","cancelled","ignored"];
      const NODE_STATES = ["pending","running","done","failed","cancelled","ignored"];
      const IRREVERSIBLE_STATES = new Set(["done","failed","cancelled"]);
      let tab = "squads";
      /** @type {SquadView[]} */
      let squads = [];
      let gotoSearchQuery = "";
      let gotoSearchSelected = 0;
      /** @type {GuardianView[]} */
      let guardians = [];
      /** @type {ResourceEntry[]} */
      let resources = [];
      let resSort = { key: "cpu", dir: -1 };   // resource-table sort: column key + direction
