## 11. Open questions

### 11.1 Is `kind` a closed enum?

`AGENTS.md` requires asking rather than guessing on new-field validation.
Suggestion is a closed set — `discovery`, `decision`, `hazard`, `deferred` —
because open strings become forty synonyms for "note" within a month and
nothing is filterable. But a closed set rejects a kind wanted later. Real
trade; needs a decision before phase 1's schema.

### 11.2 Does a prophecy survive its squad's deletion?

Ghosts cascade-delete. If a prophecy does too, deleting a squad silently strips
the reasoning out of an already-open PR. Leaning **survives**, which means a
nullable squad reference and an orphan-retention policy.

### 11.3 Does a prophecy feed forward into prompts, or only outward to humans?

Feeding it back into dependent cells makes it a better ghost. It also
reintroduces the context-cost problem ghost's 4000-char cap exists to prevent.
Cheapest resolution: outward only in v1, revisit after phase 5.

### 11.4 Do we want the pre-existing credential exposure ticketed?

Independent of prophecy: a cell's agent can read `daemon.token` and assert any
`X-Ralphus-User`, so it can already cancel squads and merge reviews (§5).
RAL-252 (verified login) and RAL-225 (container mode) are the two existing
threads that would close it. Reasonable to accept for a single-user dev tool —
but better as a written decision than an unexamined default. Note
`SECURITY_AUDIT.local.md` does not currently mention it.
