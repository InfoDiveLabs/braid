#!/bin/sh
set -eu

# Every container starts as root unless told otherwise, and PUID/PGID/UMASK
# are the variables a compose file for a Deluge or qBittorrent container
# already sets for exactly that reason. Deleting this section would make the
# script shorter and turn this into the one container in the stack whose
# files land root-owned in a media library shared with everything else
# there, and the one that fails outright on its first run with a permission
# error on /downloads instead of just working the way what it replaced did.

if [ -z "${PUID:-}" ]; then
    echo "PUID is not set. Refusing to start as root: set PUID (and PGID) to" >&2
    echo "the numeric ids that should own the files under /config and" >&2
    echo "/downloads, the same way you already do for any other torrent or" >&2
    echo "download container." >&2
    exit 1
fi

PGID="${PGID:-$PUID}"
UMASK="${UMASK:-022}"

case "$PUID" in
    ''|*[!0-9]*) echo "PUID must be numeric, got '$PUID'." >&2; exit 1 ;;
esac
case "$PGID" in
    ''|*[!0-9]*) echo "PGID must be numeric, got '$PGID'." >&2; exit 1 ;;
esac

umask "$UMASK"

# setpriv rather than a user baked into the image at build time: PUID and
# PGID are only known once the container starts, so a user created when the
# image was built can never be the one that should own these files. setpriv
# changes to a numeric uid/gid without needing an /etc/passwd entry for it,
# which a synthetic user created here on every start would otherwise need.
#
# `exec` here, and setpriv's own exec of the command that follows it,
# replace this shell twice over, so braid-server ends up running as the
# container's PID 1 instead of a child of it. Skip either replacement and
# SIGTERM from `docker stop` hits a shell that is not listening for it: the
# runtime then waits out the whole stop grace period before it gives up and
# sends SIGKILL, throwing away the graceful shutdown the server already does
# for itself when it gets the signal directly.
exec setpriv --reuid "$PUID" --regid "$PGID" --clear-groups --inh-caps=-all "$@"
