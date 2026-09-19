"""Builds the OTA package and its manifest from dist/Spoty.

    python tools/make_update.py <dist/Spoty> <out dir> <version> [notes or notes-file]

Writes <out>/spoty-update.tar.gz and <out>/update.json. Upload both to the same
place (for example one GitHub release) and point `update_url` at update.json.
"""
import hashlib
import json
import os
import sys
import tarfile

EXECUTABLE = {"spoty", "launch.sh"}
SKIP = {"settings.json"}


def main() -> None:
    src, out, version = sys.argv[1], sys.argv[2], sys.argv[3]
    notes = sys.argv[4] if len(sys.argv) > 4 else ""
    # Notes may be a UTF-8 text file (safer than passing Vietnamese on a command line).
    if notes and os.path.isfile(notes):
        with open(notes, encoding="utf-8-sig") as f:
            notes = f.read().strip()
    os.makedirs(out, exist_ok=True)
    pkg = os.path.join(out, "spoty-update.tar.gz")
    with tarfile.open(pkg, "w:gz", format=tarfile.USTAR_FORMAT) as tar:
        for root, dirs, files in os.walk(src):
            dirs[:] = [d for d in dirs if d != "data"]
            for name in sorted(files):
                full = os.path.join(root, name)
                rel = os.path.relpath(full, src).replace(os.sep, "/")
                if rel in SKIP:
                    continue
                info = tar.gettarinfo(full, arcname=rel)
                info.mode = 0o755 if rel in EXECUTABLE else 0o644
                info.uid = info.gid = 0
                info.uname = info.gname = ""
                with open(full, "rb") as f:
                    tar.addfile(info, f)
    data = open(pkg, "rb").read()
    manifest = {
        "version": version,
        "notes": notes,
        "file": "spoty-update.tar.gz",
        "sha256": hashlib.sha256(data).hexdigest(),
        "size": len(data),
    }
    with open(os.path.join(out, "update.json"), "w", encoding="utf-8") as f:
        json.dump(manifest, f, ensure_ascii=False, indent=2)
    print(f"OTA package: {pkg} ({len(data)} bytes)")


if __name__ == "__main__":
    main()
