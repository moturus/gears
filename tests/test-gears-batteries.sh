#!/bin/sh
# Hook-protocol and real-tool workflow checks, executed locally.
set -eu
. "${0%/*}/native.sh"
command -v "$SEARCH_PROGRAM"
build build --bins --example workflow-smoke
BATTERIES_BIN="$BIN_DIR/gears-batteries"
WORK="${TMPDIR:-/tmp}/gears-batteries-$$"
if [ "$BUILD_TOOL" = lorry ]; then WORK="/user/tmp/gears-batteries-$$"; fi
mkdir "$WORK"
trap 'rm -r "$WORK"' EXIT
export GEARS_DIRECTORY="$WORK/state"
mkdir "$GEARS_DIRECTORY"
cp -r "$ROOT_DIR/tools" "$GEARS_DIRECTORY/tools"
EVENT='{"protocol_version":1,"event":"tool_execute","workspace":"'"$WORK"'","session":{"id":"fixture"}}'
if printf '%s' "$EVENT" | "$BATTERIES_BIN" hook; then
  echo "batteries accepted a tool_execute without a tool call" >&2
  exit 1
fi
EVENT='{"protocol_version":1,"event":"tool_execute","workspace":"'"$WORK"'","session":{"id":"fixture"},"payload":{"tool_call":{"id":"c1","name":"todo","arguments":"{}"}}}'
OUTPUT="$(printf '%s' "$EVENT" | "$BATTERIES_BIN" hook)"
case "$OUTPUT" in
  *'"tool_result"'*) ;;
  *) echo "unexpected todo reply: $OUTPUT" >&2; exit 1 ;;
esac
if printf '{}' | "$BATTERIES_BIN" hook; then
  echo "batteries accepted malformed hook input" >&2
  exit 1
fi
"$BIN_DIR/examples/workflow-smoke" "$BATTERIES_BIN" "$(command -v rustc)"
OUTPUT="$(printf '/tools\n/help\n/quit\n' | OPENROUTER_API_KEY=fixture-local \
  "$BIN_DIR/gears" --ui line --ephemeral --config "$ROOT_DIR/config/gears-batteries.toml" \
  --provider local --model fixture --workspace "$WORK" 2>&1)"
case "$OUTPUT" in
  *'read_file'*': enabled (config)'*'check'*'/remember - '*) ;;
  *) echo "missing tools or hook command: $OUTPUT" >&2; exit 1 ;;
esac
echo "test-gears-batteries.sh ALL PASS"
