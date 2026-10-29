#!/bin/sh
# Native acceptance gate: all tests and mock providers run on this OS.
set -eu
. "${0%/*}/native.sh"
"$SHELL_PROGRAM" "$ROOT_DIR/tests/test-gears-http.sh" ${PROFILE_ARG:+"$PROFILE_ARG"}
"$SHELL_PROGRAM" "$ROOT_DIR/tests/test-gears-batteries.sh" ${PROFILE_ARG:+"$PROFILE_ARG"}
echo "gears full-test.sh ($PROFILE) ALL PASS"
