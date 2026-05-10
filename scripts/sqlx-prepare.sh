#!/usr/bin/env bash
# Regenerate the sqlx offline cache (`.sqlx/`) for crates/anatta-store
# after schema or query changes. Commit the resulting `.sqlx/` directory
# alongside the change.
#
# Requires sqlx-cli (one-time install):
#     cargo install sqlx-cli --no-default-features --features sqlite,rustls
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CRATE_DIR="$REPO_ROOT/crates/anatta-store"

if ! command -v sqlx >/dev/null 2>&1; then
    echo "error: sqlx-cli not found. install with:"
    echo "  cargo install sqlx-cli --no-default-features --features sqlite,rustls"
    exit 1
fi

# Ephemeral DB just for introspection; deleted at exit.
TMPDIR=$(mktemp -d)
trap "rm -rf '$TMPDIR'" EXIT
TMPDB="$TMPDIR/prepare.db"

cd "$CRATE_DIR"

DATABASE_URL="sqlite://$TMPDB" sqlx db create
DATABASE_URL="sqlite://$TMPDB" sqlx migrate run --source migrations
DATABASE_URL="sqlite://$TMPDB" SQLX_OFFLINE=false cargo sqlx prepare

echo
echo "regenerated $CRATE_DIR/.sqlx/ — commit changes."
