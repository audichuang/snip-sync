#!/bin/sh
# Root only long enough to hand the volumes to `ubuntu`, then the command runs
# unprivileged like CI's: root reads a file a test made unreadable. A volume is
# only walked when its root is not ubuntu's yet: new, or from a root-run image.
set -eu
for dir in /opt/cargo/registry /opt/cargo/git "$(pwd)/target"; do
	if [ -d "$dir" ] && [ "$(stat -c %u "$dir")" != 1000 ]; then chown -R ubuntu:ubuntu "$dir"; fi
done
exec setpriv --reuid=ubuntu --regid=ubuntu --init-groups env HOME=/home/ubuntu USER=ubuntu LOGNAME=ubuntu "$@"
