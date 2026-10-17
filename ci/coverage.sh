#!/usr/bin/env bash
# Test coverage of the workspace (source-based, `-C instrument-coverage` through cargo-llvm-cov): an HTML report to browse,
# an lcov file for other tools, and the table per file. A reference, not a gate: below COVERAGE_TARGET it only says
# so. Policy and critical zones: CLAUDE.md.
# usage: ci/coverage.sh [out-dir]          (default: target/coverage; HTML report in <out-dir>/html/index.html)
# env: COVERAGE_TARGET  line coverage the whole workspace aims at (default 80)
#      COVERAGE_BRANCH=1  also measure branches (needs a nightly toolchain: rustup toolchain install nightly)
#      COVERAGE_OPEN=1    open the report in the browser when done
# needs: cargo install cargo-llvm-cov --locked ; rustup component add llvm-tools
set -uo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
OUT="$(realpath -m "${1:-target/coverage}")"
TARGET="${COVERAGE_TARGET:-80}"
command -v cargo-llvm-cov >/dev/null || { echo "cargo-llvm-cov is missing: cargo install cargo-llvm-cov --locked" >&2; exit 2; }
# What is measured: the product code. Not the patched copies of IronRDP, the test tooling (xtask, bench, test
# client) nor the integration tests themselves.
IGNORE='(third_party|xtask|bins/bench|bins/rdp-test-client|/tests/)'
TOOLCHAIN=(); BRANCH=()
if [[ "${COVERAGE_BRANCH:-0}" == 1 ]]; then TOOLCHAIN=(+nightly); BRANCH=(--branch); fi
rm -rf "$OUT"; mkdir -p "$OUT"
cargo "${TOOLCHAIN[@]}" llvm-cov clean --workspace
cargo "${TOOLCHAIN[@]}" llvm-cov --no-report --workspace "${BRANCH[@]}" || { echo "the tests failed: no report" >&2; exit 1; }
R=(cargo "${TOOLCHAIN[@]}" llvm-cov report "${BRANCH[@]}" --ignore-filename-regex "$IGNORE")
"${R[@]}" --html --output-dir "$OUT" >/dev/null
"${R[@]}" --lcov --output-path "$OUT/lcov.info" >/dev/null
"${R[@]}" --json --summary-only --output-path "$OUT/summary.json" >/dev/null
"${R[@]}" --summary-only | tee "$OUT/summary.txt"
LINES=$(python3 -c "import json,sys; print('%.1f' % json.load(open(sys.argv[1]))['data'][0]['totals']['lines']['percent'])" "$OUT/summary.json")
echo
echo "line coverage: ${LINES}% (reference ${TARGET}%)   HTML report: $OUT/html/index.html   lcov: $OUT/lcov.info"
python3 -c 'import sys; sys.exit(0 if float(sys.argv[1]) >= float(sys.argv[2]) else 1)' "$LINES" "$TARGET" \
  || echo "below the reference: see CLAUDE.md for the zones that matter, no need to chase the last tenths of a point"
[[ "${COVERAGE_OPEN:-0}" == 1 ]] && xdg-open "$OUT/html/index.html" >/dev/null 2>&1
exit 0
