#!/usr/bin/env bash
# Reproduce issue #9 and prove the fix, without a model key.
#
#   ./tests/anthropic-pairing/check.sh ./target/release/cua-hub.exe
#
# The stub answers 400 for any request that carries a tool_use without a
# matching tool_result — the rule the real API enforces. A binary that drops
# unparsable tool calls therefore dies at step 1; one that answers them finishes
# the task. Prints both the run outcome and what the stub saw per request.
set -uo pipefail

BIN="${1:?usage: check.sh <path to cua-hub>}"
PORT="${STUB_PORT:-8899}"
OUT="$(mktemp -d)"
LOG="$OUT/requests.jsonl"

cleanup() {
  [ -n "${STUB_PID:-}" ] && kill "$STUB_PID" 2>/dev/null
  return 0
}
trap cleanup EXIT

STUB_PORT="$PORT" STUB_LOG="$LOG" node "$(dirname "$0")/stub.cjs" >"$OUT/stub.log" 2>&1 &
STUB_PID=$!
sleep 2

ANTHROPIC_API_KEY=stub-key \
ANTHROPIC_BASE_URL="http://127.0.0.1:$PORT" \
CUA_HUB_MODEL=claude-test \
  "$BIN" run "press the mouse button" --engine vision --provider anthropic \
    --max-steps 4 --yes </dev/null >"$OUT/run.log" 2>&1
RUN_EXIT=$?

echo "--- run (exit $RUN_EXIT) ---"
grep -E "completed:|Error|HTTP 400|unparsable|answering the rejected" "$OUT/run.log" | tail -3
echo "--- what the stub saw ---"
node -e '
  const rows = require("fs").readFileSync(process.argv[1], "utf8").trim().split("\n").filter(Boolean).map(JSON.parse);
  for (const r of rows) {
    const paired = r.tool_uses.filter((id) => r.tool_results.includes(id));
    console.log(`  req#${r.step} tool_uses=${JSON.stringify(r.tool_uses)} paired=${JSON.stringify(paired)} unanswered=${JSON.stringify(r.unanswered)}`);
  }
  const last = rows[rows.length - 1];
  const died = rows.some((r) => r.unanswered.length > 0);
  console.log(died
    ? "  VERDICT: a tool_use was sent unanswered -> the API would 400 here (issue #9 present)"
    : "  VERDICT: every tool_use was answered before the next request (issue #9 fixed)");
' "$LOG"

rm -rf "$OUT"
exit $RUN_EXIT
