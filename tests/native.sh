# Shared setup for gates that execute entirely on the current OS.
ROOT_DIR="$(cd "${0%/*}/.." && pwd)"
cd "$ROOT_DIR"
PROFILE=debug
PROFILE_ARG=
case "${1:-}" in
  '') ;;
  --release) PROFILE=release; PROFILE_ARG=--release ;;
  *) echo "usage: $0 [--release]" >&2; exit 2 ;;
esac
if [ -x /system/bin/rush ]; then
  SEARCH_PROGRAM=/system/bin/rg
  BUILD_TOOL=lorry
  SHELL_PROGRAM=/system/bin/rush
  BIN_DIR="$ROOT_DIR/target/lorry/$PROFILE"
else
  SEARCH_PROGRAM=grep
  BUILD_TOOL=cargo
  SHELL_PROGRAM=/bin/sh
  BIN_DIR="$ROOT_DIR/target/$PROFILE"
fi
build() {
  if [ "$BUILD_TOOL" = cargo ]; then
    cargo "$@" --locked --offline ${PROFILE_ARG:+"$PROFILE_ARG"}
  else
    lorry "$@" ${PROFILE_ARG:+"$PROFILE_ARG"}
  fi
}
