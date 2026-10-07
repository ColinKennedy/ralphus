#!/usr/bin/env bash
# Remote-machine fixture that has its own Docker engine, for the SSH
# provider's container mode. Counterpart of scripts/ssh-target.sh.
set -euo pipefail
# Git Bash on Windows rewrites absolute paths in docker arguments; keep them literal.
export MSYS_NO_PATHCONV=1

action="${1:-up}"
port="${RALPHUS_SSH_DOCKER_TEST_PORT:-2223}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if command -v cygpath >/dev/null 2>&1; then root="$(cygpath -m "$root")"; fi
state="$root/.docker-ssh-docker-target"
private_key="$state/id_ed25519"
authorized_keys="$state/authorized_keys"
known_hosts="$state/known_hosts"
ssh_config="$state/ssh_config"
compose="$root/docker/ssh-docker-target-compose.yml"
work_image="ralphus-remote-agent:test"

mkdir -p "$state"
if [[ ! -f "$private_key" ]]; then
    ssh-keygen -q -t ed25519 -N '' -C ralphus-docker-docker-test -f "$private_key"
fi
cp "$private_key.pub" "$authorized_keys"
# Windows OpenSSH refuses a private key readable by anyone else; a key made by
# Git Bash's ssh-keygen inherits the directory's ACL.
if command -v icacls >/dev/null 2>&1; then
    icacls "$private_key" /inheritance:r >/dev/null
    icacls "$private_key" /grant:r "${USERNAME:-$USER}:(R)" >/dev/null
fi

export RALPHUS_SSH_DOCKER_TEST_PORT="$port"
export RALPHUS_SSH_TEST_AUTHORIZED_KEYS="$authorized_keys"

case "$action" in
    up)
        docker build --file "$root/docker/remote-agent/Dockerfile" --tag "$work_image" "$root"
        docker compose --file "$compose" up --build --detach --wait
        # Load the work image into the *inner* engine, the one the provider
        # will create the work container on.
        docker save "$work_image" | docker compose --file "$compose" exec --no-tty --interactive target docker load
        host_public_key="$(docker compose --file "$compose" exec --no-tty target cat /etc/ssh/ralphus-host-keys/ssh_host_ed25519_key.pub)"
        read -r key_type key_data _ <<<"$host_public_key"
        printf '[127.0.0.1]:%s %s %s\n' "$port" "$key_type" "$key_data" >"$known_hosts"
        cat >"$ssh_config" <<EOC
Host ralphus-docker-docker
    HostName 127.0.0.1
    Port $port
    User ralphus
    IdentityFile $private_key
    IdentitiesOnly yes
    UserKnownHostsFile $known_hosts
    StrictHostKeyChecking yes
    BatchMode yes
EOC
        echo "Ralphus remote host with Docker engine is ready."
        echo "SSH: ssh -F $ssh_config ralphus-docker-docker"
        echo "Work image (inside the host): $work_image"
        echo "Register provider: ralphus machine register --scheme ssh-docker --program $root/target/debug/ralphus-ssh-provider --arg=--ssh-config=$ssh_config --arg=--container-image=$work_image --arg=--container-mount=/srv/ralphus-work:/home/ralphus/.ralphus/remote-work"
        echo "Task machine value: ssh-docker:ralphus-docker-docker"
        echo "Remote root (inside the work container): /home/ralphus/.ralphus/remote-work"
        ;;
    stop) docker compose --file "$compose" stop ;;
    down) docker compose --file "$compose" down ;;
    destroy) docker compose --file "$compose" down --volumes --remove-orphans ;;
    status) docker compose --file "$compose" ps ;;
    config) echo "$ssh_config" ;;
    *) echo "usage: $0 {up|stop|down|destroy|status|config}" >&2; exit 2 ;;
esac
