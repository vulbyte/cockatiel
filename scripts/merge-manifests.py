#!/usr/bin/env python3
"""Merge per-platform release assets into the checked-in component manifests.

`scripts/package-release.sh` runs on each release runner and, for that one
platform, writes an `assets.<os>.<arch>` entry into the component's manifest and
copies the patched manifest to `<dist>/<component-dir>.manifest.json`. This
script unions those per-platform manifests back into the working-tree manifests
so the checked-in files carry every platform's asset + checksum.

Usage:
    scripts/merge-manifests.py <dist-dir> [--check]

`<dist-dir>` contains one or more `*.manifest.json` files. For each, the script
locates the component's real manifest in the monorepo (by matching the manifest
`name` against the component dirs) and merges `release.assets` (existing entries
are preserved; per-platform entries are added/overwritten). `--check` reports
whether anything would change and exits non-zero if so.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MANIFEST_NAME = "cockatiel_module_info.json"

# component directory -> its checked-in manifest. Modules are discovered below.
CORE_DIRS = [
    "cockatiel_engine-rs",
    "cockatiel_engine-rs/modules/cockatiel_user_database-rs",
    "modules/cockatiel_module-tui_v2-rs",
    "modules/cockatiel_module-test_runner-rs",
]


def component_manifests() -> dict[str, Path]:
    """Map component `name` -> manifest path for the core + every module."""
    out: dict[str, Path] = {}
    for rel in CORE_DIRS:
        path = ROOT / rel / MANIFEST_NAME
        if path.is_file():
            out[json.loads(path.read_text())["name"]] = path
    for path in sorted((ROOT / "modules").glob(f"*/{MANIFEST_NAME}")):
        out[json.loads(path.read_text())["name"]] = path
    return out


def merge_assets(dest: dict, src: dict) -> None:
    """Union src's release.assets into dest (in place), preserving the rest."""
    src_release = src.get("release") or {}
    if not src_release:
        return
    dest_release = dest.setdefault("release", {})
    for key in ("repo", "base_url", "tag"):
        if key not in dest_release and key in src_release:
            dest_release[key] = src_release[key]
    dest_assets = dest_release.setdefault("assets", {})
    for os_name, by_arch in (src_release.get("assets") or {}).items():
        dest_assets.setdefault(os_name, {})
        for arch, entry in by_arch.items():
            dest_assets[os_name][arch] = entry


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        print(__doc__)
        return 2
    dist = Path(argv[1])
    check = "--check" in argv[2:]
    if not dist.is_dir():
        print(f"error: {dist} is not a directory", file=sys.stderr)
        return 1

    known = component_manifests()
    changed: list[str] = []
    for manifest_copy in sorted(dist.glob("*.manifest.json")):
        patch = json.loads(manifest_copy.read_text())
        name = patch.get("name")
        target = known.get(name)
        if target is None:
            print(f"warning: no checked-in manifest for component {name!r}", file=sys.stderr)
            continue
        original = target.read_text()
        obj = json.loads(original)
        before = json.dumps(obj.get("release", {}), sort_keys=True)
        merge_assets(obj, patch)
        after = json.dumps(obj.get("release", {}), sort_keys=True)
        if before == after:
            continue
        new_text = json.dumps(obj, indent=2, ensure_ascii=original.isascii())
        if original.endswith("\n"):
            new_text += "\n"
        changed.append(str(target.relative_to(ROOT)))
        if not check:
            target.write_text(new_text, encoding="utf-8")

    if check:
        if changed:
            print("out of date:")
            for path in changed:
                print(f"  {path}")
            return 1
        print("manifests up to date")
        return 0

    if changed:
        print("updated:")
        for path in changed:
            print(f"  {path}")
    else:
        print("nothing to do")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
