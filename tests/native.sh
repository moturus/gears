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
SEARCH_PROGRAM=/system/bin/rg
SHELL_PROGRAM=/system/bin/rush
BIN_DIR="$ROOT_DIR/target/lorry/$PROFILE"
build() {
  lorry "$@" ${PROFILE_ARG:+"$PROFILE_ARG"}
}
