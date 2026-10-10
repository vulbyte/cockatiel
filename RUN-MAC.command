#!/bin/bash
# RUN-MAC.command — double-click this in Finder to build + launch Cockatiel.
# It is a thin wrapper around the shared unix launcher next to it.
DIR="$(cd "$(dirname "$0")" && pwd)"
exec "$DIR/RUN-UNIX.sh" "$@"
