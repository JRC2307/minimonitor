#!/usr/bin/env bash
# Integration test for scripts/secret-scan.sh
# Verifies: (1) detects a real secret in a staged file; (2) exits 0 on a clean tree.
#
# MUST be run from the repo root (so git ls-files resolves correctly).
set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
SCAN="$REPO_ROOT/scripts/secret-scan.sh"
FAKE_SECRET_FILE="crates/fleet/tests/fixtures/fake_secret_DO_NOT_COMMIT.tmp"

cd "$REPO_ROOT"

cleanup() {
    git rm -f --cached "$FAKE_SECRET_FILE" 2>/dev/null || true
    rm -f "$FAKE_SECRET_FILE"
}
trap cleanup EXIT

# ── Test 1: secret detection ────────────────────────────────────────────────
echo "=== Test 1: scanner detects a staged secret ==="
echo 'KUMA_TOKEN=tk_aaaabbbbcccc1234' > "$FAKE_SECRET_FILE"
git add "$FAKE_SECRET_FILE"

if bash "$SCAN" 2>&1; then
    echo "FAIL: expected non-zero exit but got 0"
    exit 1
else
    echo "PASS: scanner correctly exited non-zero"
fi

# ── Test 2: clean tree exits 0 ──────────────────────────────────────────────
echo "=== Test 2: scanner exits 0 on clean tree ==="
git rm -f --cached "$FAKE_SECRET_FILE" 2>/dev/null
rm -f "$FAKE_SECRET_FILE"
trap - EXIT   # cleanup already done

if bash "$SCAN" 2>&1; then
    echo "PASS: scanner exits 0 on clean tree"
else
    echo "FAIL: unexpected non-zero exit on clean tree"
    exit 1
fi

echo "All secret-scan tests passed."
