# syntax=docker/dockerfile:1.7

FROM rust:bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --release -p ralphus-runner

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        git \
        openssh-server \
        procps \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --create-home --uid 10001 --shell /bin/bash ralphus \
    && passwd -d ralphus \
    && install -d -m 0700 -o ralphus -g ralphus /home/ralphus/.ssh \
    && install -d -m 0755 /run/sshd /bootstrap /srv/git \
    && install -d -m 0750 -o ralphus -g ralphus /home/ralphus/.ralphus/remote-work

COPY --from=builder /src/target/release/ralphus-runner /usr/local/bin/ralphus-runner
COPY docker/ssh-target/mock-claude.sh /usr/local/bin/claude
COPY docker/ssh-target/sshd_config /etc/ssh/sshd_config
COPY docker/ssh-target/entrypoint.sh /usr/local/bin/ralphus-ssh-target-entrypoint
COPY docker/ssh-target/reset-origin.sh /usr/local/bin/reset-origin.sh

RUN chmod 0755 \
        /usr/local/bin/ralphus-runner \
        /usr/local/bin/claude \
        /usr/local/bin/ralphus-ssh-target-entrypoint \
        /usr/local/bin/reset-origin.sh

EXPOSE 22
ENTRYPOINT ["/usr/local/bin/ralphus-ssh-target-entrypoint"]
