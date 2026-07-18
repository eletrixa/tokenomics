#!/usr/bin/env bash
# check.sh — the gate. Must be green before any wave is "done".
# fmt (no drift) + clippy (pedantic, -D warnings) + tests.
set -euo pipefail
cd "$(dirname "$0")"
# shellcheck disable=SC1090
. "$HOME/.cargo/env" 2>/dev/null || true

echo "▶ cargo fmt --check"
cargo fmt --check
echo "▶ cargo clippy --all-targets --all-features -- -D warnings"
cargo clippy --all-targets --all-features -- -D warnings
echo "▶ cargo test"
cargo test

echo "▶ PII gate (email addresses over plans/ specs/ src/ docs/ tests/ fixtures/)"
ALLOWLIST=".pii-allowlist"
EMAIL_RE='[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}'
matches="$(grep -rnoE "$EMAIL_RE" plans/ specs/ src/ docs/ tests/ fixtures/ 2>/dev/null || true)"
pii_fail=0
if [ -n "$matches" ]; then
  while IFS= read -r hit; do
    [ -z "$hit" ] && continue
    email="${hit##*:}"
    if ! grep -qxF "$email" "$ALLOWLIST" 2>/dev/null; then
      echo "✗ possible email outside .pii-allowlist: $hit"
      pii_fail=1
    fi
  done <<<"$matches"
fi
if [ "$pii_fail" -ne 0 ]; then
  echo "PII gate failed — allowlist only known-safe synthetic/noreply addresses in $ALLOWLIST," \
       "never a real personal email."
  exit 1
fi
echo "✓ PII gate clean"

echo "✓ check green"
