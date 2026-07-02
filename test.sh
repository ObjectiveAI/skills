#!/usr/bin/env bash
# test.sh — run the integration suite against the local .objectiveai sandbox.
#
# Assumes build.sh has ALREADY staged everything into <repo>/.objectiveai: the
# objectiveai host binaries and the unpacked arcanum plugin. test.sh does NOT
# build — run build.sh first (or build-and-test.sh). It resets per-run state,
# applies the api config the run needs, then runs cargo-nextest. The `list_skills`
# e2e tests create laboratories that mount test-skills/ folders, so podman and a
# network (to pull the base image) are required.
#
# Requires cargo-nextest on PATH. Extra args forward to nextest
# (e.g. `bash test.sh <test-name-filter>`).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$REPO_ROOT"
OAI_DIR="$REPO_ROOT/.objectiveai"
export OBJECTIVEAI_DIR="$OAI_DIR"

case "$(uname -s)" in
  CYGWIN*|MINGW*|MSYS*) EXE=".exe" ;;
  *)                    EXE=""     ;;
esac
HOST="$OAI_DIR/bin/objectiveai$EXE"
[ -x "$HOST" ] \
  || { echo "test.sh: objectiveai host not found at $HOST — run build.sh first" >&2; exit 1; }

# 1. Stop any running host servers / daemons (they hold ports/files open).
echo "==> objectiveai kill-all"
"$HOST" kill-all || true

# 2. Fresh per-run state.
rm -rf "$OAI_DIR/state"

# 3. Global api config the run needs (mcp timeout covers laboratory startup +
#    the find; backoff is best-effort on older hosts).
echo "==> objectiveai api config (global)"
"$HOST" api config mcp-timeout-ms set --value 300000 --global
"$HOST" api config backoff-max-elapsed-time-ms set --value 0 --global || true

# 4. Run the suite, then stop the host's servers and exit on nextest's rc.
#    Serial (`--test-threads=1`): the e2e tests each spin up a real objectiveai
#    host + postgres + laboratory containers, so running them concurrently
#    exhausts shared resources (db connections / containers) and times out.
echo "==> cargo nextest run"
rc=0
cargo nextest run --test-threads=1 "$@" || rc=$?

echo "==> objectiveai kill-all"
"$HOST" kill-all || true

exit "$rc"
