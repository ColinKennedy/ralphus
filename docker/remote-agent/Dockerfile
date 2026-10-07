# syntax=docker/dockerfile:1.7

# The work image for container-mode machines (the container the SSH provider
# runs cells, proofs and reviews in on a remote host). It is what an operator
# would pass as `--container-image`: the runner, git, and an agent CLI, with
# no sshd and no daemon. This copy ships a deterministic mock `claude` so the
# fixture needs no credentials.

FROM rust:bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --release -p ralphus-runner

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        git \
        procps \
        tmux \
    && rm -rf /var/lib/apt/lists/*

# A review's rebase commits as whoever git says the committer is, and a
# container has no ~/.gitconfig to inherit. Without a system identity the
# second branch of a stacked review fails with "Committer identity unknown".
RUN git config --system user.name "Ralphus Remote Agent" \
    && git config --system user.email "ralphus-remote-agent@example.invalid"

RUN useradd --create-home --uid 10001 --shell /bin/bash ralphus \
    && install -d -m 0750 -o ralphus -g ralphus /home/ralphus/.ralphus/remote-work

COPY --from=builder /src/target/release/ralphus-runner /usr/local/bin/ralphus-runner
COPY docker/ssh-target/mock-claude.sh /usr/local/bin/claude
RUN chmod 0755 /usr/local/bin/ralphus-runner /usr/local/bin/claude

USER ralphus
WORKDIR /home/ralphus
