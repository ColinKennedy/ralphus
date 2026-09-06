#!/bin/sh
set -eu

authorized_keys=/bootstrap/authorized_keys
host_key_dir=/etc/ssh/ralphus-host-keys
remote_root=/home/ralphus/.ralphus/remote-work

if [ ! -s "$authorized_keys" ]; then
    echo "missing or empty $authorized_keys; run scripts/ssh-target.ps1 up first" >&2
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

install -d -m 0750 -o ralphus -g ralphus "$remote_root"
install -d -m 0755 -o ralphus -g ralphus /srv/git

# Seeds the origin only if it's missing (RAL-355: reset-origin.sh --force
# reseeds it on demand between test suites; see docs/remote-docker-target.md).
/usr/local/bin/reset-origin.sh

chown -R ralphus:ralphus /srv/git "$remote_root"
/usr/sbin/sshd -t
exec /usr/sbin/sshd -D -e
