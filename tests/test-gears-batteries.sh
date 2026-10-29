#!/bin/sh
# Existing policy-input and real-tool workflow checks, executed locally.
set -eu
. "${0%/*}/native.sh"
command -v "$SEARCH_PROGRAM"
build build --bins --example workflow-smoke
BATTERIES_BIN="$BIN_DIR/gears-batteries"
WORK="${TMPDIR:-/tmp}/gears-policy-$$"
if [ "$BUILD_TOOL" = lorry ]; then WORK="/user/tmp/gears-policy-$$"; fi
mkdir "$WORK"
trap 'rm -r "$WORK"' EXIT
export GEARS_DIRECTORY="$WORK/state"
mkdir "$GEARS_DIRECTORY"
cp -r "$ROOT_DIR/tools" "$GEARS_DIRECTORY/tools"
EVENT='{"protocol_version":1,"event":"permission","workspace":"'"$WORK"'","session":{"id":"fixture"}}'
OUTPUT="$(printf '%s' "$EVENT" | "$BATTERIES_BIN" hook)"
case "$OUTPUT" in
  '{"decision":"deny",'*'invalid permission call'*) ;;
  *) echo "unexpected malformed-call reply: $OUTPUT" >&2; exit 1 ;;
esac
EVENT='{"protocol_version":1,"event":"permission","workspace":"'"$WORK"'","session":{"id":"fixture"},"payload":{"tool_call":{"id":"c1","name":"todo","arguments":"{}"}}}'
OUTPUT="$(printf '%s' "$EVENT" | "$BATTERIES_BIN" hook)"
[ "$OUTPUT" = '{"decision":"allow"}' ]
EVENT='{"protocol_version":1,"event":"permission","workspace":"'"$WORK"'","session":{"id":"fixture"},"payload":{"tool_call":{"id":"c1","name":"sh","arguments":"{\"command\":\"rm -rf '"$GEARS_DIRECTORY"'\"}"}}}'
OUTPUT="$(printf '%s' "$EVENT" | "$BATTERIES_BIN" hook)"
[ "$OUTPUT" = '{"decision":"deny"}' ]
if printf '{}' | "$BATTERIES_BIN" hook; then
  echo "batteries accepted malformed hook input" >&2
  exit 1
fi
"$BIN_DIR/examples/workflow-smoke" "$BATTERIES_BIN" "$(command -v rustc)"
OUTPUT="$(printf '/tools\n/help\n/quit\n' | OPENROUTER_API_KEY=fixture-local \
  "$BIN_DIR/gears" --ui line --ephemeral --config "$ROOT_DIR/config/gears-batteries.toml" \
  --provider local --model fixture --workspace "$WORK" 2>&1)"
case "$OUTPUT" in
  *'read_file: enabled (config)'*'check: enabled (config)'*'/remember - '*) ;;
  *) echo "missing tools or hook command: $OUTPUT" >&2; exit 1 ;;
esac
echo "test-gears-batteries.sh ALL PASS"
