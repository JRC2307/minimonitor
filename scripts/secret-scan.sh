#!/usr/bin/env bash
set -euo pipefail

# Scan tracked files for leaked secrets.
# Excludes this script itself (contains the patterns as strings) and docs/ (spec
# files legitimately use pattern examples like tk_/Bearer /client_secret).
PATTERNS=("tk_" "Bearer " "client_secret")
FOUND=0

while IFS= read -r file; do
    for pat in "${PATTERNS[@]}"; do
        if grep -qF "$pat" "$file" 2>/dev/null; then
            echo "SECRET FOUND in $file: $pat"
            FOUND=1
        fi
    done
done < <(git ls-files -- ':!:scripts/secret-scan.sh' ':!:docs/')

if [ "$FOUND" -eq 1 ]; then
    echo "Secret scan FAILED"
    exit 1
fi

echo "Secret scan passed"
