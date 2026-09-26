## 9. Shape of one prophecy

Small enough to be cheap, typed enough to be filterable, not so typed the agent
stalls.

| Field | Type | Notes |
|---|---|---|
| `entity_uri` | text | Owner: `cell:…` / `guardian:…`. Reuses `EntityUri`. |
| `attempt` | int | Keeps attempts 1…N distinct. The reason this table is append-only. |
| `kind` | enum? | Proposal: closed set `discovery` \| `decision` \| `hazard` \| `deferred`. **Validation is an open question — see §11.1.** |
| `body` | text | The note. Per-entry cap. |
| `revision` | text? | Opaque VCS marker, best-effort, `None` when unavailable (mirrors ghost). Needs `ws.git()` to work remotely (§6.3). |
| `created_at_ms` | int | |
| `published_at_ms` | int? | Set when folded into a PR body. |
| `pr_id` | text? | Which PR it landed in. |

Deliberately **not** in v1: a confidence score (§12).
