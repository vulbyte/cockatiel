#!/bin/bash
# RUN-LINUX.sh — build + launch Cockatiel on Linux.
# A thin wrapper around the shared unix launcher next to it.
DIR="$(cd "$(dirname "$0")" && pwd)"
exec "$DIR/RUN-UNIX.sh" "$@"
