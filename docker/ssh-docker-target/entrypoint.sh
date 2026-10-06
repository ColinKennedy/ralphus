#!/bin/sh
set -eu

authorized_keys=/bootstrap/authorized_keys
host_key_dir=/etc/ssh/ralphus-host-keys
# Host directory the work container bind-mounts as its remote root; owned by
# the work image's uid so the runner can write to it.
work_root=/srv/ralphus-work

if [ ! -s "$authorized_keys" ]; then
    echo "missing or empty $authorized_keys; run scripts/ssh-docker-target.sh up first" >&2
    exit 1
fi

install -d -m 0700 -o ralphus -g ralphus /home/ralphus/.ssh
install -m 0600 -o ralphus -g ralphus "$authorized_keys" /home/ralphus/.ssh/authorized_keys

install -d -m 0700 "$host_key_dir"
if [ ! -s "$host_key_dir/ssh_host_ed25519_key" ]; then
    ssh-keygen -q -t ed25519 -N '' -f "$host_key_dir/ssh_host_ed25519_key"
fi
chmod 0600 "$host_key_dir/ssh_host_ed25519_key"
chmod 0644 "$host_key_dir/ssh_host_ed25519_key.pub"

install -d -m 0750 -o 10001 -g 10001 "$work_root"

# Start the inner Docker engine (TLS off: it is only reachable through the
# unix socket, by members of the `docker` group).
DOCKER_TLS_CERTDIR= dockerd-entrypoint.sh >/var/log/dockerd.log 2>&1 &
i=0
until docker info >/dev/null 2>&1; do
    i=$((i + 1))
    if [ "$i" -gt 60 ]; then
        echo "inner dockerd did not become ready; log follows" >&2
        cat /var/log/dockerd.log >&2
        exit 1
    fi
    sleep 1
done

/usr/sbin/sshd -t
exec /usr/sbin/sshd -D -e
