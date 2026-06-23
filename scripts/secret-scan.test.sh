#!/usr/bin/env bash
# Integration test for scripts/secret-scan.sh
# Verifies:
#   1. Detects a realistic GitHub token planted in a temp file under deploy/
#   2. Exits 0 on a clean tree
#   3. Does NOT flag crates/fleet/src/secrets.rs (short fake tokens, no live/test infix)
#   4. Does NOT flag crates/fleet/tests/config_secrets_doctor_test.rs (same reason)
#
# MUST be run from the repo root (so git ls-files resolves correctly).
set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
SCAN="$REPO_ROOT/scripts/secret-scan.sh"

# Realistic GitHub token: gho_ + 40 alphanumeric chars (constructed at runtime, not stored as literal)
# pragma: allowlist secret
GH_PREFIX="gho_"
GH_BODY="A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8S9t0"  # 40 alphanum chars # pragma: allowlist secret
FAKE_TOKEN="${GH_PREFIX}${GH_BODY}"  # pragma: allowlist secret

FAKE_SECRET_FILE="deploy/fake_secret_DO_NOT_COMMIT.tmp"

cd "$REPO_ROOT"

cleanup() {
    git rm -f --cached "$FAKE_SECRET_FILE" 2>/dev/null || true
    rm -f "$FAKE_SECRET_FILE"
}
trap cleanup EXIT

# ── Test 1: detect a realistic GitHub token ──────────────────────────────────
echo "=== Test 1: scanner detects a staged GitHub token ==="
mkdir -p "$(dirname "$FAKE_SECRET_FILE")"
printf 'GITHUB_TOKEN=%s\n' "$FAKE_TOKEN" > "$FAKE_SECRET_FILE"
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

# ── Test 3: secrets.rs is NOT flagged ────────────────────────────────────────
echo "=== Test 3: crates/fleet/src/secrets.rs not flagged ==="
SECRETS_RS="crates/fleet/src/secrets.rs"
if [[ -f "$SECRETS_RS" ]]; then
    # Run scan and look for any hit on secrets.rs specifically
    if bash "$SCAN" 2>&1 | grep -q "in $SECRETS_RS"; then
        echo "FAIL: scanner flagged $SECRETS_RS — short fake tokens should not trip value-shaped rules"
        exit 1
    else
        echo "PASS: $SECRETS_RS not flagged"
    fi
else
    echo "SKIP: $SECRETS_RS not found"
fi

# ── Test 4: config_secrets_doctor_test.rs is NOT flagged ────────────────────
echo "=== Test 4: crates/fleet/tests/config_secrets_doctor_test.rs not flagged ==="
DOCTOR_TEST="crates/fleet/tests/config_secrets_doctor_test.rs"
if [[ -f "$DOCTOR_TEST" ]]; then
    if bash "$SCAN" 2>&1 | grep -q "in $DOCTOR_TEST"; then
        echo "FAIL: scanner flagged $DOCTOR_TEST"
        exit 1
    else
        echo "PASS: $DOCTOR_TEST not flagged"
    fi
else
    echo "SKIP: $DOCTOR_TEST not found"
fi

echo "All secret-scan tests passed."
