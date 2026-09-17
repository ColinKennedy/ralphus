# RAL-433 — board mutating-action audit (local reference only, not committed)

Audit of every mutating board action found while implementing the unified
notification system (`librarian/assets/board/21-notifications.js`). "Wired"
means the action now calls `notify()` (directly, or via `post()`/`del()`'s
`notifyOpts`) after this change. "Pre-existing" means it already showed some
form of feedback (toast/alert) before this change and was migrated onto the
shared system rather than newly instrumented.

## Squad create / mutate (Squads tab, `40-dialogs.js`, `55-new-task-modal.js`, `60-logs-modal.js`, `25-chrome.js`)
- Squad create — Simple tab (`submitTaskSimple`) — wired (success + error; previously inline-text-only, no toast at all)
- Squad create — Files tab (`submitTaskFiles`) — wired (success + error)
- Squad create — Paste tab (`submitTask` inline branch) — wired (success + error)
- Restart (single node: task/cell/proof, via `confirmRestart`) — wired (success + error); this is the shared chokepoint for every restart button/menu item on the board
- Restart preview compute failure (`showRestartPreview`) — wired (error only, mechanical alert->notify)
- Cancel squad (`confirmCancelSquad`, `cancelSquad`) — wired (success + error); shared chokepoint for the Cancel Squad button
- Cancel preview compute failure (`showCancelPreview`) — wired (error only)
- Stop task/cell/proof node (`stopNode`) — wired (success + error; was previously silent)
- Delete squad (`deleteSquad`) — wired (success + error; was previously silent)
- Set Status (single/multi, `doPickStatus`) — wired (success added for the previously-silent all-succeeded squad path; alert->notify for the failure path)
- Solo/un-solo task (`soloTaskAct`) — wired (was previously silent)
- Add Dependency (`confirmAddDependency`) — wired (success added; was previously silent on success)
- Hide/unhide squad, single (`setSquadHidden`) — error wired (alert->notify); no success toast (low-stakes, instantly-visible toggle, matches review/task equivalents)
- Hide/unhide squad, batch (`setSquadsHiddenBatch`) — wired (success + error; alert->notify)
- Bulk cancel (`bulkCancel`) — wired (was previously fully silent, swallowed all errors)
- Bulk delete (`bulkDelete`) — wired (success + error; was previously silent)
- `reportGraphActionOutcome` (shared bulk graph-node action summary) — alert->notify (success + error)
- Env override add/edit/remove (`45-details-pane.js`) — wired via `post()` `notifyOpts` (success + error; was alert-only on failure, silent on success)
- Open terminal / open agent / resume automation / remote terminal (`25-chrome.js`, `30-live-view.js`) — error wired (mechanical alert->notify); "Resume Automation" already had a success toast pre-existing

## Reviews tab (`65-reviews.js`, `66-review-edit-modal.js`, `10-tab-registry.js`)
- Create review (`createReview`) — wired (success added; error was already inline text, kept)
- Merge/rebase start-or-resume (`mergeReview`) — pre-existing (`showInfoToast`) migrated to `notify("info", ...)`
- Stop rebase (`stopMerge`) — wired (success added; error was already covered via `guardianAction`)
- Approve review (`approveReview`) — wired (success added; error pre-existing via `guardianAction`)
- Sync PR / stack-reorder check (`syncPrReview`) — pre-existing, migrated
- Save review details / edit modal (`66-review-edit-modal.js`) — pre-existing (`showWarningToast`/`showInfoToast`) migrated; added a `success` fallback for the no-op-message case
- PR submission failed / PR stack requested/opened (`submitPrStack`) — pre-existing, migrated
- Refresh PR status, Action PR feedback (`10-tab-registry.js`) — pre-existing (`showReviewError`/`showInfoToast`), migrated
- Delete review (`deleteReview`) — wired (success + error via `del()` `notifyOpts`)
- Hide/unhide review, single (`setReviewHidden`) — error wired (alert->notify), no success toast (parity with squads)
- Hide/unhide review, batch (`bulkHideReviews`/`bulkUnhideReviews`) — error wired (alert->notify)
- Load review details before Edit modal opens (`openEditReviewDetails`) — error wired (alert->notify)
- Guardian one-shot notices (`checkGuardianNotices`) — pre-existing (`showInfoToast`), migrated

## Tasks tab (`15-tasks.js`)
- Watch/unwatch task, cell (`ttToggleWatch`, `ttToggleCellWatch`) — error wired (alert->notify); no success toast (star icon itself is the immediate feedback, same as hide/unhide elsewhere)
- Hide/unhide squad from Tasks tab row menu (`ttHideSquads`) — error wired
- Hide/unhide task, batch (`ttHideTasks`) — error wired

## Queue tab (`80-queue.js`)
- Save staged reorder (`queueSave`) — wired (success + error; was previously fully silent and didn't even check `resp.ok` before applying the response)
- Set Status from Queue context menu — shares `doPickStatus`/`openStatusPicker`, already covered above

## Explicitly out of scope for this pass (flagged, not touched)
These are real mutating actions but sit in admin-only settings screens
(`75-projects-machines.js`, `76-project-review-settings-modal.js`) far
outside the ticket's named categories (squad create, cancel/restart/delete/
retry/feedback/bulk ops, merge/rebase) and the primary Squads/Reviews/Tasks/
Queue workflow surface. Converting all of them would have meaningfully
expanded the blast radius of this change with no clear ask for it:
- Machines: register/check/remove machine
- Triage: register/remove triage type, pool-threshold preview/confirm, drain pool, add/remove schedule
- Users: add/remove/edit user, toggle admin
- Secrets: add/rename/remove secret env name
- Projects: save project edit, project fork add/edit/remove, project triage threshold preview/confirm
- Preferences: set auto-watch, toggle entity watch, unhide pref item

Recommend a follow-up ticket if consistent notification coverage should
extend to the admin surface too.

## Client-side validation gates left as blocking `alert()` (not board-action outcomes)
- `45-details-pane.js`: "not a valid environment variable name" (gates before any network request)
- `75-projects-machines.js`: "Choose at least one of urgent, high, normal." (same — pre-submit form validation, not an action result)

## Notes on the `response.ok` decision
`post()`/`del()` already read `resp.ok` in every call site that needed it
(most already branch on it for their own success/failure bookkeeping) — no
change was needed there. What was added is an *optional* third `notifyOpts`
argument (`{success, errorLabel}`) that, when passed, drives `notify()`
automatically off a **cloned** response so the original body stream stays
untouched for the caller's own `resp.json()` reads. This was low-lift (no
extra network round trip, no added latency — `resp.clone()` is synchronous)
and callers that already have bespoke success/failure handling (dry-run
previews, inline form errors, batch summaries) simply don't pass it, so nothing
is double-reported.
