## 5. Trust boundary — give the agent no new capability

The tempting version hands the cell a daemon URL and a token so it can `POST` a
prophecy. Refuse it, for the opposite of the obvious reason.

1. **The URL is not a secret.** Hardcoded default `http://127.0.0.1:7890`
   (`cli/src/client.rs:12`). Withholding it protects nothing.
2. **Neither is the token.** Bearer auth *is* enforced on every route
   (RAL-219), but the daemon persists the token as a plain file at
   `state_dir()/daemon.token`, and the CLI reads it from there whenever
   `$RALPHUS_DAEMON_TOKEN` is unset (`cli/src/client.rs:159`). The agent runs
   as the same OS user, so it is a `cat` away.
3. **`ralphus` is already on `PATH`.** A cell can run `ralphus squad cancel`
   today and let the CLI do the token lookup. No env var required.
4. **Caller identity is self-asserted.** The `X-Ralphus-User` header;
   `daemon/src/server.rs:4181` says it outright — *"there is no verified-login
   distinction yet (RAL-252)."* `admin_gated` trusts the header value.

**So the exposure is pre-existing and total.** That is a defensible posture for
a single-user local dev tool and it is not this subsystem's job to fix. But it
does mean a *scoped* prophecy token would be decorative: anything it withheld
is readable anyway. Scoping only becomes real where the cell cannot read the
daemon's filesystem — container mode (RAL-225). Separate ticket, separate
decision (see §11, §13).

**Which is why the marker wins on trust, not just cost.** A stderr line is not
an endpoint: no reachable surface, no credential, and it cannot be pointed at
any other route. And because `forward_runner_event` fills identity from the
owning `RunnerSpec`, the *daemon* decides which cell a prophecy belongs to — a
cell cannot forge another cell's attribution even deliberately.

**On the two remaining env vars.** `RALPHUS_ENTITY_URI` and `RALPHUS_ATTEMPT`
are *information*, not capability — they grant no access, and they would make
any CLI command usable in-cell without the agent guessing ids. Worth doing
eventually, but not needed for a prophecy, and the credential question rides
along with them. Own ticket. `RALPHUS_DAEMON_URL` is cut outright.
