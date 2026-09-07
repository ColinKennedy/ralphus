# Common manual-testing commands

```bash
# Validate a task file (offline, no daemon needed)
ralphus validate task.toml
# or
cargo run -p ralphus-daemon -- validate task.toml

# Submit a task
ralphus submit task.toml
ralphus submit task.toml --hold          # stages as Queued, not Pending

# Check system health (daemon reachable, git on PATH, runner available)
ralphus check health

# Query the daemon API directly
curl http://127.0.0.1:7890/api/daemon
curl http://127.0.0.1:7890/api/tasks
curl http://127.0.0.1:7890/api/squads/<id>

# Run a specific Rust unit-test module
cargo nextest run -p ralphus-daemon scheduler::
cargo nextest run -p ralphus-core validate::

# Run a single Python test by name
cd cli && uv run pytest -k test_render_test_svg_includes_commit_labels -s

# Build and run the keygen tool (author-only; not distributed)
cargo run -p ralphus-keygen -- generate
cargo run -p ralphus-keygen -- sign --key ralphus-private.key --name "Name" --expiry 2027-01-01

# Secure-dist build (daemon + librarian refuse to start without ralphus.lic)
cargo build --release --features ralphus-daemon/secure-dist,ralphus-librarian/secure-dist
```
