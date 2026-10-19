#!/bin/sh
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
sys="$(uname -s)"
case "$sys" in
MINGW* | MSYS* | CYGWIN*) rel="x86_64-pc-windows-msvc/gray-account.exe" ;;
Darwin*)
    case "$(uname -m)" in
    arm64) rel="aarch64-apple-darwin/gray-account" ;;
    *) rel="x86_64-apple-darwin/gray-account" ;;
    esac ;;
*)
    case "$(uname -m)" in
    aarch64 | arm64) rel="aarch64-unknown-linux-musl/gray-account" ;;
    *) rel="x86_64-unknown-linux-musl/gray-account" ;;
    esac ;;
esac
if [ -x "$here/$rel" ]; then exec "$here/$rel" "$@"; fi
for cand in "$here/target/release/gray-account.exe" "$here/target/release/gray-account" "$here/target/debug/gray-account.exe" "$here/target/debug/gray-account"; do
    if [ -x "$cand" ]; then exec "$cand" "$@"; fi
done
echo "gray-account binary not found in $here" >&2
exit 1
