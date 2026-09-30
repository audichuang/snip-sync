#!/bin/sh
# Root only long enough to hand the volumes to `ubuntu`, then the command runs
# unprivileged like CI's: root reads a file a test made unreadable. A volume is
# only walked when its root is not ubuntu's yet: new, or from a root-run image.
set -eu
for dir in /opt/cargo/registry /opt/cargo/git "$(pwd)/target"; do
	if [ -d "$dir" ] && [ "$(stat -c %u "$dir")" != 1000 ]; then chown -R ubuntu:ubuntu "$dir"; fi
done
rc=0
setpriv --reuid=ubuntu --regid=ubuntu --init-groups env HOME=/home/ubuntu USER=ubuntu LOGNAME=ubuntu "$@" || rc=$?
# Acceptance writes its evidence under /tmp, which leaves with the container; keep it,
# pass or fail, when the host mounted somewhere for it.
if [ -d /evidence ]; then
	for run in /tmp/snip-native-acceptance-*; do
		if [ -d "$run" ]; then cp -a "$run" /evidence/; fi
	done
fi
exit "$rc"
