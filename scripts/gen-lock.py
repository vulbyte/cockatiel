#!/usr/bin/env python3
"""Regenerate the repo-root ``cockatiel.lock`` and the ``release`` blocks in the
component manifests.

This is the Phase 2 release-data generator. It pins every shipped component
(``engine``, ``tui``, ``user-db``, ``test-runner`` and every ``modules/*``
module) to the gitlink SHA recorded by ``git submodule status`` together with
the ``name``/``version`` from that component's ``cockatiel_module_info.json``.
The result is fully reproducible: running the script twice is a no-op.

Sources of truth
----------------
* ``.gitmodules``               -> submodule path -> remote URL, normalised to
                                  ``https://github.com/<owner>/<name>`` (both
                                  ``https://github.com/...`` and
                                  ``git@github.com:...`` forms are accepted and
                                  a trailing ``.git`` is stripped).
* ``git submodule status``      -> the 40-hex gitlink SHA per submodule path.
* ``cockatiel_module_info.json`` -> ``name`` + ``version`` per component.

In-repo components (``cockatiel_user_database-rs`` and
``cockatiel_test_runner-rs``) have no gitlink, so their ``sha`` is ``null`` and
their releases live on the monorepo (``vulbyte/cockatiel``).

Each manifest also gets a ``release`` block::

    "release": {
      "repo": "<owner>/<name>",
      "base_url": "https://github.com/<owner>/<name>/releases/download",
      "tag": "v<version>"
    }

The block is appended when absent and updated in place when present, so key
order and every other field survive untouched. Re-serialisation mirrors the
existing 2-space pretty style and only changes the ``release`` key.

Usage
-----
    python3 scripts/gen-lock.py           # rewrite manifests + lock
    python3 scripts/gen-lock.py --check   # exit 1 if anything would change
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MANIFEST_NAME = "cockatiel_module_info.json"
MONOREPO = "vulbyte/cockatiel"
LOCK_PATH = ROOT / "cockatiel.lock"

# (lock key, component directory, kind, is_submodule)
CORE = (
    ("engine", "cockatiel_engine-rs", "engine", True),
    ("tui", "cockatiel_tui_v2-rs", "tui", True),
    ("user-db", "cockatiel_user_database-rs", "user-db", False),
    ("test-runner", "cockatiel_test_runner-rs", "test-runner", False),
)


def normalize_repo(url: str) -> str:
    """Return ``owner/name`` for a GitHub remote in https or ssh form."""
    url = url.strip()
    if url.startswith("git@"):
        # git@github.com:owner/name(.git)
        _, host_path = url.split("@", 1)
        _, path = host_path.split(":", 1)
    elif "://" in url:
        # https://github.com/owner/name(.git)
        _, host_path = url.split("://", 1)
        _, path = host_path.split("/", 1)
    else:
        path = url
    path = path.strip().rstrip("/")
    if path.endswith(".git"):
        path = path[:-4]
    return path


def read_gitmodules() -> dict[str, str]:
    """Map submodule path -> raw URL from ``.gitmodules``."""
    mapping: dict[str, str] = {}
    current: str | None = None
    for line in (ROOT / ".gitmodules").read_text(encoding="utf-8").splitlines():
        stripped = line.strip()
        if stripped.startswith("[submodule"):
            current = None
        elif stripped.startswith("path") and current is None and "=" in stripped:
            current = stripped.split("=", 1)[1].strip()
        elif stripped.startswith("url") and current is not None and "=" in stripped:
            mapping[current] = stripped.split("=", 1)[1].strip()
    return mapping


def read_submodule_shas() -> dict[str, str]:
    """Map submodule path -> gitlink SHA from ``git submodule status``."""
    out = subprocess.check_output(
        ["git", "submodule", "status"], cwd=ROOT, text=True
    )
    shas: dict[str, str] = {}
    for line in out.splitlines():
        line = line.rstrip()
        if not line:
            continue
        # "<status><sha> <path> (<describe>)" — status is ' ', '+', '-' or 'U'
        rest = line[1:]
        sha, _, tail = rest.partition(" ")
        path = tail.strip().split(" ")[0] if tail.strip() else ""
        if path:
            shas[path] = sha
    return shas


def release_block(repo: str, version: str) -> dict[str, str]:
    return {
        "repo": repo,
        "base_url": f"https://github.com/{repo}/releases/download",
        "tag": f"v{version}",
    }


def ensure_release(obj: dict, repo: str, version: str) -> dict:
    """Insert or update the ``release`` key without disturbing other keys.

    Preserves an existing ``assets`` map (written by the release packaging
    step) so regenerating the lock never drops published checksums."""
    existing = obj.get("release")
    merged = dict(existing) if isinstance(existing, dict) else {}
    merged.setdefault("repo", repo)
    merged["base_url"] = f"https://github.com/{repo}/releases/download"
    merged["tag"] = f"v{version}"
    obj["release"] = merged
    return obj


def dumps_preserving(obj: dict, original: str) -> str:
    """Pretty-print like ``serde_json::to_string_pretty`` (2-space indent).

    Unicode is left literal unless the original file was pure ASCII (in which
    case the existing ``\\uXXXX`` escapes are reproduced), and the original
    trailing-newline choice is preserved so re-serialisation is byte-stable.
    """
    text = json.dumps(obj, indent=2, ensure_ascii=original.isascii())
    if original.endswith("\n"):
        text += "\n"
    return text


def load(path: Path) -> tuple[str, dict]:
    raw = path.read_text(encoding="utf-8")
    return raw, json.loads(raw)


def main(argv: list[str]) -> int:
    check = "--check" in argv[1:]
    # Refresh only cockatiel.lock; leave the manifests alone. Useful after
    # submodule bumps when the manifests are already committed and carry
    # release `assets` the lock generator must not touch.
    lock_only = "--lock-only" in argv[1:]
    gitmodules = read_gitmodules()
    shas = read_submodule_shas()
    changes: list[str] = []
    components: dict[str, dict] = {}

    def register(key, rel, kind, repo, version, sha):
        components[key] = {
            "path": rel,
            "sha": sha,
            "version": version,
            "repo": repo,
            "kind": kind,
        }

    # --- core components -------------------------------------------------
    for key, rel, kind, is_sub in CORE:
        manifest = ROOT / rel / MANIFEST_NAME
        raw, obj = load(manifest)
        version = obj["version"]
        if is_sub:
            if rel not in gitmodules:
                raise SystemExit(f"error: {rel} missing from .gitmodules")
            repo = normalize_repo(gitmodules[rel])
            sha = shas.get(rel)
            if sha is None:
                raise SystemExit(f"error: {rel} missing from git submodule status")
        else:
            repo, sha = MONOREPO, None
        # Core components carry a `kind` so the TUI's plugin discovery does not
        # mistake the engine/TUI/user-db/test-runner manifests for chat modules.
        if not lock_only:
            obj = ensure_release(obj, repo, version)
            obj["kind"] = kind
            new_text = dumps_preserving(obj, raw)
            if new_text != raw:
                changes.append(str(manifest.relative_to(ROOT)))
                if not check:
                    manifest.write_text(new_text, encoding="utf-8")
        register(key, rel, kind, repo, version, sha)

    # --- modules ---------------------------------------------------------
    for manifest in sorted((ROOT / "modules").glob(f"*/{MANIFEST_NAME}")):
        rel = manifest.parent.relative_to(ROOT).as_posix()
        raw, obj = load(manifest)
        name, version = obj["name"], obj["version"]
        if rel not in gitmodules:
            raise SystemExit(f"error: {rel} missing from .gitmodules")
        repo = normalize_repo(gitmodules[rel])
        sha = shas.get(rel)
        if sha is None:
            raise SystemExit(f"error: {rel} missing from git submodule status")
        if not lock_only:
            new_text = dumps_preserving(ensure_release(obj, repo, version), raw)
            if new_text != raw:
                changes.append(str(manifest.relative_to(ROOT)))
                if not check:
                    manifest.write_text(new_text, encoding="utf-8")
        register(name, rel, "module", repo, version, sha)

    # --- lock ------------------------------------------------------------
    lock = {
        "lock_version": 1,
        "components": {k: components[k] for k in sorted(components)},
    }
    lock_text = json.dumps(lock, indent=2, ensure_ascii=False) + "\n"
    old_lock = LOCK_PATH.read_text(encoding="utf-8") if LOCK_PATH.exists() else None
    if old_lock != lock_text:
        changes.append("cockatiel.lock")
        if not check:
            LOCK_PATH.write_text(lock_text, encoding="utf-8")

    if check:
        if changes:
            print("out of date:")
            for path in changes:
                print(f"  {path}")
            return 1
        print("cockatiel.lock and manifests are up to date")
        return 0

    if changes:
        print("updated:")
        for path in changes:
            print(f"  {path}")
    else:
        print("nothing to do (already up to date)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
