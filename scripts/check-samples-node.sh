#!/usr/bin/env bash
# samples/node の独立性を static に見る（DESIGN-SAMPLES-NODE.md §5）。
#
# merge CI の共通 conformance 行列と一緒に必須。ビルドの成否には現れない
# 依存・import を入口で止める。
#
# 見るもの:
#   - package.json の production runtime dependency が空
#   - require() は node:* と相対 path だけ
#   - JS から Rust/core crate・生成 DTO を import しない
#
# 見ないもの:
#   - README の説明文
#   - Node の構文/unit（node --check と process conformance が別途見る）

set -euo pipefail
cd "$(dirname "$0")/.."

NODE=samples/node
fail=0

die() {
  echo "samples/node static audit FAIL: $*"
  fail=1
}

if [[ ! -f "$NODE/.node-version" ]]; then
  die "missing $NODE/.node-version (exact version pin)"
elif ! grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' "$NODE/.node-version"; then
  die "$NODE/.node-version must be an exact X.Y.Z pin"
fi

pkg="$NODE/web-gateway/package.json"
if [[ ! -f "$pkg" ]]; then
  die "missing $pkg"
elif ! python3 - "$pkg" <<'PY'
import json, sys
path = sys.argv[1]
data = json.loads(open(path, encoding="utf-8").read())
deps = data.get("dependencies")
if deps != {}:
    raise SystemExit(f"{path} dependencies must be {{}} (got {deps!r})")
PY
then
  die "runtime dependency 0"
fi

if ! python3 - "$NODE" <<'PY'
import re, sys
from pathlib import Path
root = Path(sys.argv[1])
req_re = re.compile(r"""require\((['"])([^'"]+)\1\)""")
failed = False
for path in sorted(root.rglob("*.js")):
    text = path.read_text(encoding="utf-8")
    for m in req_re.finditer(text):
        spec = m.group(2)
        if spec.startswith("node:") or spec.startswith("./") or spec.startswith("../"):
            continue
        print(f"{path}: require({spec!r}) must be node:* or relative")
        failed = True
if failed:
    raise SystemExit(1)
PY
then
  die "require() is node:* / relative only"
fi

if hits=$(grep -nE 'opencrab_|opencrab-core|opencrab-web-gateway|CARGO_MANIFEST|crates/' \
  --include='*.js' -R "$NODE" || true) && [[ -n "$hits" ]]; then
  die "Rust/core import 0"
  printf '%s\n' "$hits"
fi

if [[ "$fail" -ne 0 ]]; then
  exit 1
fi
echo "samples/node static audit OK"
