#!/usr/bin/env bash
# Build the ORIGINAL Fress frontend into app/frontend-dist/.
#
# The GUI is shipped UNCHANGED: this script checks out WasewaseX/Fress at
# the pinned commit into a scratch directory (never modifying the upstream
# repo or any local clone of it), runs its own vite build there, and copies
# the dist/ output into this repo. Every frontend byte the app serves
# comes from that pinned upstream build.
set -euo pipefail

FRESS_REPO="${FRESS_REPO:-https://github.com/WasewaseX/Fress}"
FRESS_REF="${FRESS_REF:-d0909125dfbdcd5c50d25cf966a7aaf22414a0ab}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SCRATCH="${FRESS_BUILD_DIR:-$ROOT/target/fress-frontend-src}"

echo "==> checkout $FRESS_REPO @ ${FRESS_REF:0:12} (scratch: $SCRATCH)"
rm -rf "$SCRATCH"
mkdir -p "$SCRATCH"
git init -q "$SCRATCH"
git -C "$SCRATCH" remote add origin "$FRESS_REPO"
git -C "$SCRATCH" fetch -q --depth 1 origin "$FRESS_REF"
git -C "$SCRATCH" checkout -q FETCH_HEAD

echo "==> npm ci + vite build (upstream scripts, unmodified)"
(cd "$SCRATCH" && npm ci --no-audit --no-fund && npm run build)

echo "==> copy dist/ -> app/frontend-dist/"
rm -rf "$ROOT/app/frontend-dist"
mkdir -p "$ROOT/app/frontend-dist"
cp -r "$SCRATCH/dist/." "$ROOT/app/frontend-dist/"

echo "==> done: $(find "$ROOT/app/frontend-dist" -type f | wc -l) files"
