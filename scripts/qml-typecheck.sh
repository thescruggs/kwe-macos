#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# qml-typecheck.sh: the QML type gate for the manager.
#
# Two failures shipped in 0.1.0-alpha.1 because nothing here was enforced:
#
#   1. The five C++ clients carried QML_UNCREATABLE with no QML_ELEMENT, so
#      qmltyperegistrar registered NOTHING (the generated qmltypes was an
#      empty `Module {}`) and every `ApplyClient.Failed`-style read was a
#      runtime ReferenceError that silently killed the binding.
#   2. `qmllint` on PATH is qt5-declarative's qmllint 1.0 on this distro: it
#      cannot resolve a single Qt 6 type and exits 0 on everything, so the
#      old check.sh call was a no-op.
#
# So this script resolves the *Qt 6* qmllint explicitly, asserts the built
# module actually registers its C++ types, and fails on any unresolved TYPE
# (an unqualified identifier starting with an uppercase letter). Unqualified
# *instances* (applyClient, catalogClient, …) are by design — they are
# context properties set in main.cpp, which qmllint cannot see — so they are
# reported as a count and do not fail the gate.
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_root"

build_dir="${KWE_CMAKE_BUILD_DIR:-build/cmake}"
module_dir="$build_dir/apps/kwe-manager/org/kde/kwe"
qmltypes="$module_dir/kwe-manager.qmltypes"

# The registered C++ types, i.e. every class carrying QML_ELEMENT in the
# manager's QML module sources.
registered_types=(ApplyClient CatalogClient DaemonActivator PackageInstaller PermissionsClient)

resolve_qmllint() {
    # Never trust a bare `qmllint`: on this distro it is Qt 5's.
    local candidate
    if command -v qtpaths6 >/dev/null 2>&1; then
        candidate="$(qtpaths6 --query QT_HOST_BINS 2>/dev/null || true)/qmllint"
        [[ -x "$candidate" ]] && { echo "$candidate"; return 0; }
    fi
    for candidate in /usr/lib/qt6/bin/qmllint /usr/lib64/qt6/bin/qmllint \
        /usr/lib/qt6/libexec/qmllint /usr/bin/qmllint6; do
        [[ -x "$candidate" ]] && { echo "$candidate"; return 0; }
    done
    if command -v qmllint >/dev/null 2>&1 && qmllint --version 2>&1 | grep -q "qmllint 6"; then
        command -v qmllint
        return 0
    fi
    return 1
}

if [[ ! -f "$qmltypes" ]]; then
    echo "qml-typecheck: $qmltypes is missing; build the manager first" >&2
    exit 1
fi

# Gate 1: the module must register its C++ types.
missing=()
for type in "${registered_types[@]}"; do
    grep -q "name: \"$type\"" "$qmltypes" || missing+=("$type")
done
if (( ${#missing[@]} > 0 )); then
    echo "qml-typecheck: the QML module registers no type named: ${missing[*]}" >&2
    echo "  $qmltypes" >&2
    echo "  QML_UNCREATABLE alone registers nothing — each class needs QML_ELEMENT too." >&2
    exit 1
fi
echo "qml-typecheck: qmltypes registers ${#registered_types[@]} C++ types"

# Gate 2: no unresolved types in the QML.
if ! qmllint_bin="$(resolve_qmllint)"; then
    echo "qml-typecheck: no Qt 6 qmllint found; skipping the QML lint gate" >&2
    exit 0
fi
echo "qml-typecheck: using $qmllint_bin ($("$qmllint_bin" --version 2>&1 | head -1))"

lint_report="$(mktemp -t kwe-qmllint.XXXXXX)"
trap 'rm -f -- "$lint_report"' EXIT

# qmllint exits non-zero on warnings; the gate below decides what matters.
"$qmllint_bin" -I /usr/lib/qt6/qml -I "$build_dir/apps/kwe-manager" \
    apps/kwe-manager/qml/*.qml >"$lint_report" 2>&1 || true
"$qmllint_bin" -I /usr/lib/qt6/qml -I "$build_dir/apps/kwe-frame-preview" \
    apps/kwe-frame-preview/qml/Preview.qml >>"$lint_report" 2>&1 || true

python3 - "$lint_report" <<'PY'
import re
import sys

report = open(sys.argv[1], encoding="utf-8", errors="replace").read().splitlines()
unresolved_types = []
instances = 0
for index, line in enumerate(report):
    match = re.match(r"Warning: (.*?):(\d+):(\d+): Unqualified access \[unqualified\]", line)
    if not match or index + 2 >= len(report):
        continue
    source, carets = report[index + 1], report[index + 2]
    column = carets.find("^")
    token = re.match(r"[A-Za-z_][A-Za-z0-9_]*", source[column:]) if column >= 0 else None
    token = token.group(0) if token else ""
    # An uppercase identifier is a TYPE the file failed to resolve: either the
    # module does not register it or the file is missing its import.
    if token[:1].isupper():
        unresolved_types.append(f"{match.group(1)}:{match.group(2)}: {token}")
    else:
        instances += 1

if unresolved_types:
    print("qml-typecheck: unresolved QML types (every read of these throws at runtime):",
          file=sys.stderr)
    for entry in unresolved_types:
        print(f"  {entry}", file=sys.stderr)
    print("  Register the type (QML_ELEMENT) and import its module in the file.",
          file=sys.stderr)
    raise SystemExit(1)

print(f"qml-typecheck: no unresolved types "
      f"({instances} unqualified context-property reads, by design)")
PY
