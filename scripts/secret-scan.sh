#!/usr/bin/env bash
# Scan tracked files for leaked secrets using value-shaped regex patterns.
#
# Exclusions:
#   - this script itself (contains patterns as strings)
#   - scripts/secret-scan.test.sh (contains patterns as strings)
#   - docs/ (spec files legitimately use pattern examples)
#
# Bypass: add  # pragma: allowlist secret  to any line to skip it.
#
# Exit 0 = clean, exit 1 = secrets found.
# Compatible with bash 3.2+ (macOS system bash).

set -uo pipefail

FOUND=0

# ── Helper: scan one file for one pattern, print matching lines ───────────────
# Usage: scan_file FILE RULE PATTERN
scan_file() {
    local file="$1" rule="$2" pattern="$3"
    [[ -f "$file" ]] || return 0
    local hits
    hits=$(grep -nE "$pattern" "$file" 2>/dev/null \
           | grep -v '# pragma: allowlist secret' \
           || true)
    if [[ -n "$hits" ]]; then
        while IFS= read -r hit; do
            echo "SECRET [$rule] in $file: line ${hit%%:*}"
            FOUND=1
        done <<< "$hits"
    fi
}

# ── Helper: assigned-secret rule with env-var-name exclusion ─────────────────
# Matches:  (secret|token|password|api_key) = "VALUE24+"
# Excludes: lines where the value after the delimiter is ALL-CAPS/DIGITS/UNDERSCORES
#           (i.e., an env-var name like FLEET_BESZEL_PASSWORD).
scan_assigned_secrets() {
    local file="$1"
    [[ -f "$file" ]] || return 0
    local hits
    # Step 1: match the pattern (key + 24+ char value)
    # Step 2: strip pragma lines
    # Step 3: exclude lines whose post-delimiter value looks like an env-var name
    hits=$(grep -nE \
        '(secret|token|password|api[_-]?key)["'"'"'[:space:]:=]+[A-Za-z0-9+/_-]{24,}' \
        "$file" 2>/dev/null \
        | grep -v '# pragma: allowlist secret' \
        | grep -vE \
            '["'"'"'[:space:]:=][A-Z0-9_]{24,}[^A-Za-z0-9+/_-]?[[:space:]]*["'"'"']?[[:space:]]*$' \
        || true)
    if [[ -n "$hits" ]]; then
        while IFS= read -r hit; do
            echo "SECRET [assigned-secret] in $file: line ${hit%%:*}"
            FOUND=1
        done <<< "$hits"
    fi
}

# ── Build file list ───────────────────────────────────────────────────────────
while IFS= read -r file; do
    # Standard rules (name:pattern pairs iterated manually — bash 3.2 has no assoc arrays)
    scan_file "$file" "github-token"   'gh[oprsu]_[A-Za-z0-9]{36,}'
    scan_file "$file" "github-pat"     'github_pat_[A-Za-z0-9_]{40,}'
    scan_file "$file" "aws-access-key" 'AKIA[0-9A-Z]{16}'
    scan_file "$file" "private-key"    '-----BEGIN [A-Z ]*PRIVATE KEY-----'
    scan_file "$file" "stripe-kuma"    '(sk|tk|rk)_(live|test)_[A-Za-z0-9]{16,}'
    scan_file "$file" "slack"          'xox[baprs]-[A-Za-z0-9-]{10,}'
    scan_file "$file" "bearer-token"   'Bearer[[:space:]]+[A-Za-z0-9._-]{20,}'
    scan_assigned_secrets "$file"
done < <(git ls-files -- ':!:scripts/secret-scan.sh' ':!:scripts/secret-scan.test.sh' ':!:docs/')

# ── Result ────────────────────────────────────────────────────────────────────
if [[ "$FOUND" -eq 1 ]]; then
    echo "Secret scan FAILED"
    exit 1
fi

echo "Secret scan passed"
