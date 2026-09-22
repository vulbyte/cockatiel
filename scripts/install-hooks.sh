#!/usr/bin/env bash
#
# install-hooks.sh — install a pre-commit hook that runs check_git_secrets.sh
# in this repo and every submodule, so .env / database / secret config files
# can never be committed again.
#
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CHECK="$ROOT/scripts/check_git_secrets.sh"

[[ -f "$CHECK" ]] || { printf 'error: %s not found\n' "$CHECK" >&2; exit 1; }

HOOK_BODY="$(cat <<'EOF'
#!/usr/bin/env bash
# Installed by scripts/install-hooks.sh — checks staged files for secrets.
set -Eeuo pipefail
HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$HOOK_DIR"
while [[ "$ROOT" != "/" && ! -f "$ROOT/scripts/check_git_secrets.sh" ]]; do
  ROOT="$(dirname "$ROOT")"
done
exec "$ROOT/scripts/check_git_secrets.sh" --staged
EOF
)"

# shellcheck disable=SC2207
REPOS=("$ROOT")
for sm in "$ROOT"/cockatiel_engine-rs "$ROOT"/cockatiel_lib "$ROOT"/modules/*/; do
  [[ -d "$sm/.git" || -f "$sm/.git" ]] && REPOS+=("$sm")
done

for repo in "${REPOS[@]}"; do
  # Resolve the real hooks dir (works for submodules, whose `.git` is a file
  # pointing into the superproject's `.git/modules/…`).
  local_hooks="$(git -C "$repo" rev-parse --git-path hooks 2>/dev/null || true)"
  [[ -n "$local_hooks" && -d "$local_hooks" ]] || {
    printf 'skip %s (no hooks dir)\n' "$repo" >&2
    continue
  }
  printf '%s\n' "$HOOK_BODY" > "$local_hooks/pre-commit"
  chmod +x "$local_hooks/pre-commit"
  printf 'installed pre-commit hook in %s\n' "$repo"
done

printf 'done — staged .env/db/config.json files are now blocked in every repo.\n'