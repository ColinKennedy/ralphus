# syntax=docker/dockerfile:1.7
#
# ralphus container execution mode (RAL-225).
#
# Builds one image containing the daemon, librarian, and the Python runner,
# so the whole stack -- and therefore every locally-executed agent
# subprocess the runner spawns (claude/codex CLIs, verify commands, etc) --
# runs confined to this container's filesystem instead of the bare host.
# See docs/container-mode.md for exactly what this does and does not confine
# before relying on it.
#
# This is an ADDITIONAL, opt-in execution mode -- bare-subprocess (today's
# behavior, via scripts/build-debug.sh / scripts/start-ralphus.sh) remains
# the default and is unaffected by this file's existence.
#
# Build context is the repository root (see docker/docker-compose.yml's
# `build.context: ..`) -- daemon/librarian pull in ralphus-core, ralphus-auth
# etc as workspace path deps, so the whole workspace must be visible to
# `cargo build`.

# ---- Rust builder: ralphus-daemon + ralphus-librarian ---------------------
# Floating on the latest stable Rust, same as `.github/workflows/ci.yml`'s
# `rustup toolchain install stable` -- NOT pinned to the workspace's
# `rust-version = "1.85"` floor (that field is a documented minimum, not
# what Cargo.lock's resolved dependency versions actually require to build;
# several transitive deps (icu_*, idna_adapter) already need a newer rustc
# than 1.85 to compile as locked).
FROM rust:bookworm AS rust-builder
WORKDIR /src
COPY . .
# `rusqlite`'s `bundled` feature compiles SQLite from source (no
# libsqlite3-dev needed); the `rust:*-bookworm` image already ships a C
# toolchain for that. win32job/sysinfo's Windows-only deps
# (`daemon/Cargo.toml`'s `[target."cfg(windows)".dependencies]`) are inert on
# this Linux build target, so they add nothing to compile here.
RUN cargo build --release -p ralphus-daemon -p ralphus-librarian

# ---- Python builder: the ralphus-runner venv (uv-managed) -----------------
FROM python:3.12-slim-bookworm AS python-builder
RUN pip install --no-cache-dir uv
WORKDIR /src/cli-py
COPY cli-py/pyproject.toml cli-py/uv.lock cli-py/README.md ./
COPY cli-py/src ./src
# `--extra runner` pulls in pydantic-ai (the native agent backend);
# `--no-dev` drops test/lint tooling (pytest, mypy, ruff, ...) that has no
# business in a runtime image; `--frozen` refuses to silently drift from the
# committed lockfile.
RUN uv sync --extra runner --no-dev --frozen

# ---- Runtime ----------------------------------------------------------------
FROM python:3.12-slim-bookworm AS runtime

# tmux: real Linux tmux backs the same tmux-wrapped session/live-view/
# terminal-log machinery `daemon/src/tmux.rs` already drives on a bare host
# -- no psmux, no Windows-only quirks (see AGENTS.md's "A tmux-wrapped
# session's pane can vanish..." gotcha, which is Windows/psmux-specific).
# git: agent sessions run `git` inside their workspace as part of normal
# task work.
RUN apt-get update \
    && apt-get install -y --no-install-recommends tmux git ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Non-root throughout -- part of the hardening posture alongside the
# read-only rootfs / dropped capabilities set in docker-compose.yml.
#
# Shell must be a REAL shell, not /usr/sbin/nologin: tmux resolves the
# pane's interpreter from this user's passwd entry (`default-shell`) even
# when it's given an explicit command to run, and execs that as the
# session's shell. `nologin` only blocks interactive login -- it still
# "succeeds" as an execve() target, just by printing "This account is
# currently not available." and exiting immediately, which silently killed
# every tmux pane before this was caught by manually verifying the tmux
# live-view path actually works in this container (RAL-225's own
# acceptance criterion). `nologin` on this uid would have blocked an SSH/
# console login, which was never how this uid is used anyway -- only
# `docker exec`/process spawning, which a shell is required for.
RUN useradd --create-home --uid 10001 --shell /bin/bash ralphus

COPY --from=rust-builder /src/target/release/ralphus-daemon /usr/local/bin/ralphus-daemon
COPY --from=rust-builder /src/target/release/ralphus-librarian /usr/local/bin/ralphus-librarian
COPY --from=python-builder /src/cli-py/.venv /opt/ralphus-runner-venv
COPY docker/entrypoint.sh /usr/local/bin/ralphus-entrypoint.sh
RUN chmod +x /usr/local/bin/ralphus-entrypoint.sh /usr/local/bin/ralphus-daemon /usr/local/bin/ralphus-librarian

ENV PATH="/opt/ralphus-runner-venv/bin:${PATH}" \
    RALPHUS_RUNNER_CMD="/opt/ralphus-runner-venv/bin/ralphus-runner" \
    RALPHUS_BIND_ADDR="0.0.0.0" \
    RALPHUS_DAEMON_URL="http://127.0.0.1:7890" \
    HOME="/home/ralphus"

# /workspaces is the ONLY directory this image expects agent sessions'
# `cwd` to live under -- docker-compose.yml bind-mounts exactly one host
# directory there and nowhere else, which is the actual confinement
# boundary (enforced by the container's mount namespace, not by this
# Dockerfile). /home/ralphus/.ralphus holds daemon state (SQLite DB,
# terminal logs, Cartographer) -- separate from the workspace mount since
# it is the daemon's own bookkeeping, not something an agent session should
# read or write.
RUN mkdir -p /workspaces /home/ralphus/.ralphus \
    && chown -R ralphus:ralphus /workspaces /home/ralphus

WORKDIR /workspaces
USER ralphus

EXPOSE 7890 7474
ENTRYPOINT ["/usr/local/bin/ralphus-entrypoint.sh"]
