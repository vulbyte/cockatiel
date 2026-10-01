#!/bin/bash
# Launch the Cockatiel TUI against the engine.
# Optionally pass --ip/--port/--pin to override auto-detection.
cd "$(dirname "$0")"
exec cargo run --release -- "$@"