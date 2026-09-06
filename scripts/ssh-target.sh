#!/usr/bin/env bash
set -euo pipefail

action="${1:-up}"
port="${RALPHUS_SSH_TEST_PORT:-2222}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
state="$root/.docker-ssh-target"
private_key="$state/id_ed25519"
authorized_keys="$state/authorized_keys"
known_hosts="$state/known_hosts"
ssh_config="$state/ssh_config"
compose="$root/docker/ssh-target-compose.yml"

mkdir -p "$state"
if [[ ! -f "$private_key" ]]; then
    ssh-keygen -q -t ed25519 -N '' -C ralphus-docker-test -f "$private_key"
fi
cp "$private_key.pub" "$authorized_keys"

export RALPHUS_SSH_TEST_PORT="$port"
export RALPHUS_SSH_TEST_AUTHORIZED_KEYS="$authorized_keys"

case "$action" in
    up)
        docker compose --file "$compose" up --build --detach --wait
        host_public_key="$(docker compose --file "$compose" exec --no-tty target cat /etc/ssh/ralphus-host-keys/ssh_host_ed25519_key.pub)"
        read -r key_type key_data _ <<<"$host_public_key"
        printf '[127.0.0.1]:%s %s %s\n' "$port" "$key_type" "$key_data" >"$known_hosts"
        cat >"$ssh_config" <<EOF
Host ralphus-docker
    HostName 127.0.0.1
    Port $port
    User ralphus
    IdentityFile $private_key
    IdentitiesOnly yes
    UserKnownHostsFile $known_hosts
    StrictHostKeyChecking yes
    BatchMode yes
EOF
        echo "Ralphus Docker SSH target is ready."
        echo "SSH: ssh -F $ssh_config ralphus-docker"
        echo "Provider environment: RALPHUS_SSH_CONFIG_FILE=$ssh_config"
        echo "Build provider: cargo build --package ralphus-ssh-provider"
        echo "Register provider: ralphus machine register --scheme ssh --program $root/target/debug/ralphus-ssh-provider --arg=--ssh-config --arg $ssh_config --description 'Docker SSH target fixture'"
        echo "Task machine value: ssh:ralphus-docker"
        echo "Fixture Git URL: file:///srv/git/ralphus-test.git"
        echo "Remote root: /home/ralphus/.ralphus/remote-work"
        ;;
    stop) docker compose --file "$compose" stop ;;
    down) docker compose --file "$compose" down ;;
    destroy) docker compose --file "$compose" down --volumes --remove-orphans ;;
    reset-origin)
        # RAL-355: reseeds only the bare Git origin to its pristine state --
        # unlike `destroy`, this leaves the remote root and pinned host key
        # untouched, so a test suite can get a clean origin between runs
        # without forcing a full re-`up` (and re-pin) afterward.
        docker compose --file "$compose" exec --no-tty target /usr/local/bin/reset-origin.sh --force
        echo "Fixture Git origin reset to its pristine seeded state."
        ;;
    status) docker compose --file "$compose" ps ;;
    config) echo "$ssh_config" ;;
    *) echo "usage: $0 {up|stop|down|destroy|reset-origin|status|config}" >&2; exit 2 ;;
esac
