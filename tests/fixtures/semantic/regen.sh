#!/usr/bin/env bash
# Regenerates tiny_semantic.rustdoc.json from tiny_semantic/ with a pinned nightly.
# The only post-processing is replacing the local rustup toolchain root in
# external_crates[*].path with a stable placeholder so the fixture is machine-independent.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
TOOLCHAIN=${TOOLCHAIN:-nightly-2026-09-14}
TARGET=$(mktemp -d)
trap 'rm -rf "$TARGET"' EXIT
(cd "$HERE/tiny_semantic" && CARGO_TARGET_DIR="$TARGET" cargo +"$TOOLCHAIN" rustdoc -q -- --output-format json -Z unstable-options)
python3 - "$TARGET/doc/tiny_semantic.json" "$HERE/tiny_semantic.rustdoc.json" "$(rustup +"$TOOLCHAIN" show home)" <<'PY'
import json, sys
src, dst, home = sys.argv[1:4]
doc = json.load(open(src))
for crate in doc.get("external_crates", {}).values():
    if isinstance(crate.get("path"), str) and crate["path"].startswith(home):
        crate["path"] = "<rustup-home>" + crate["path"][len(home):]
with open(dst, "w") as out:
    json.dump(doc, out, sort_keys=True, separators=(",", ":"))
    out.write("\n")
PY
