#!/bin/sh
# Unit, integration and shipped-tool checks against local loopback providers.
set -eu
. "${0%/*}/native.sh"
command -v "$SEARCH_PROGRAM"
build build --bins --examples
build test
build test --example mock-provider
OPENROUTER_API_KEY=fixture-child-key "$BIN_DIR/examples/tool-smoke" "$ROOT_DIR" "$BIN_DIR/gears"
echo "test-gears-http.sh ALL PASS"
