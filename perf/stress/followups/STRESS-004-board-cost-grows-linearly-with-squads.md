# STRESS-004: board payload and latency grow linearly with history; reads collapse at about 1,000 squads

- **Severity:** high. A long-lived instance becomes unusable as squads
  accumulate.
- **Found by:** `ralphus-stress` scenario `volume`
- **Found at:** `035ef1e70` (staging, 2026-10-18)
- **Status:** fixed on `stress-fixes-high`, which serves a 275 KB board in
  ~2 ms at 10,000 squads, flat from 1,000. The board lists a window;
  the per-squad hydration cost and archiving remain (see "What's done").

## Numbers

32 closed-loop users per window; release build; 56-CPU host.

| Squads | Projects | `GET /api/tasks` size | Single board GET | Read ok req/s (errors) | Mixed ok req/s (errors) | Daemon RSS |
|---:|---:|---:|---:|---:|---:|---:|
| 133 | 5 | 110 KB | 88 ms | 33 (0%) | 32 (0%) | 35 MB |
| 1,012 | 25 | 1.1 MB | 753 ms | 4 (35%) | 5 (26%) | 114 MB |
| 5,004 | 100 | 5.5 MB | 3.5 s | ~0 (94%) | 1 (76%) | 477 MB |
| 10,007 | 250 | 11 MB | 7.2 s | 1 (73%) | 1 (78%) | 662 MB |

During this scenario the watchdog logged
`store lock UNREACHABLE for …ms`; that is covered by STRESS-005.

## Root cause

- **There is no limit or pagination.** `GET /api/tasks` returns every squad
  ever submitted. `list_squads_conn_limited` (`store.rs:6222-6289`) runs
  with no limit, and each squad costs about 12–13 statements (see
  STRESS-003). Payload size and latency are therefore O(total squads),
  including finished or long-dead ones.
- **`/api/task-index` and `/api/queue` scale the same way.**
  `server.rs:2613`/`2637`; `store.rs:13333-13392`.

## Fix direction

- Paginate or window the board API: by default, active plus the last N
  finished squads. The librarian then asks for more on scroll.
- Keep a summary table or materialized view per squad so the board does not
  re-hydrate every cell and proof.
- Archive old terminal squads out of the hot query.
- Together with STRESS-003's snapshot cache, this should make board cost
  independent of history size.

## UX risk: high

Of all these fixes, this one changes the board's behaviour the most. The
board assumes `GET /api/tasks` holds **every** squad, so with a window:

- **Search, filters, sorting, counts and "select all" / bulk actions**
  would silently cover only the loaded page. A search would miss an old
  squad, and a count would be wrong without any sign.
- **Hovercards and derived UI** deliberately render only from data already
  loaded ("Lazy by default" in `librarian/AGENTS.md`;
  `librarian/assets/board/22-hovercards.js`). They would go blank for
  squads outside the window.
- **Deep links** from notifications, mailbox entries, entity URIs or review
  links to an old squad would land on "not found".
- **The Tasks table** (`/api/task-index`, `librarian/assets/board/15-tasks.js`)
  and the cache in `10-tab-registry.js` that maps squad ids to labels
  assume the full list, so names would show as raw ids.

How to avoid it:

- **Default window:** every non-terminal squad plus the most recent N
  finished ones, with "Load more" or infinite scroll.
- **Server-side search and filters.** `/api/tasks` already accepts
  `status`/`name`/`sort`; extend it with an offset/cursor and full-history
  search. The board's search box must query the server once a window is
  active.
- **Show "showing N of M"** wherever a count or bulk action appears, so
  partial data is never mistaken for all data.
- **Fetch one squad on demand** (`GET /api/squads/{id}`) for deep links and
  for hovercards on squads outside the window. That needs an explicit,
  documented exception to the "lazy" rule.
- **Build the UI first, then switch the default window on.** Ship the UI
  changes behind the larger default (or "all") first.

## What's done

**Daemon.** `GET /api/tasks` and `GET /api/task-index` accept
`?window=N` (capped at 1000), `&before=<cursor>` and `&q=<text>`
(`server.rs::board_window`, `store.rs::list_squads_window_conn`).

- A window is every unfinished squad plus the `N` newest finished ones.
- `before` pages older finished squads only.
- `q` is a case-insensitive id/label substring match over all of history,
  with `%`/`_` matched literally.
- The response gains a `window` object: `total`, `loaded`, `has_more`,
  `next_before`.
- Without `?window` the response is byte-for-byte unchanged, because the CLI's
  selector resolution needs the full list.
- Tests: `daemon/tests/board_window.rs`.

**Board.** It asks for `?window=200` (`BOARD_WINDOW` in `20-util.js`, the
`RALPHUS-BOARD-WINDOW` region). The UX mitigations above are in place:

- **"Showing N of M squads · Load older"** footer under the Squads list and
  the Tasks table, shown only while older history is unloaded. "Load older"
  pages through the cursor.
- **The filter boxes also search the daemon** (`searchUnloadedSquads`,
  debounced 300 ms, at least 2 characters) while history is unloaded, and pin
  the matches into the list.
- **Deep links and "go to squad" selections** for a squad outside the window
  are fetched by id (`GET /api/squads/{id}`) and pinned (`pinMissingSquads`).
- An older daemon without windowing ignores the parameter and gets the old
  behaviour; the board sees no `window` object and shows no footer.
- Tests: `test/board-window.test.mjs` and `test/tasks-poll.test.mjs`.

**Not covered yet:**

- The go-to (Ctrl-K) search, the prefs "hidden names" list, and the
  add-dependency and waypoint pickers still read only the loaded squads.
- Each listed squad is still hydrated with about 12 statements. A window
  bounds *how many*, so cost no longer grows with history. A per-squad
  summary table would cut the per-squad cost itself.
- Old terminal squads are not archived.

## Verify

Run `ralphus-stress run … --scenarios volume --volume 1000:25,10000:250`.
The 10,000-squad row's single GET and read ok req/s should be within a small
factor of the 1,000-squad row.
