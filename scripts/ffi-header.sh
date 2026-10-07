#!/usr/bin/env bash
# The C header is GENERATED from crates/bugsee-ffi/src/lib.rs by cbindgen.
#
#   scripts/ffi-header.sh gen     regenerate crates/bugsee-ffi/include/bugsee.h
#   scripts/ffi-header.sh check   fail if the checked-in header is stale, if the
#                                 shared library exports anything the header does
#                                 not declare (or vice versa), or if the header
#                                 does not compile as C99 / C++11
#
# Why: the header is the ABI contract. Hand-maintained, it drifts from the
# Rust it describes; generated and diffed in CI, an accidental ABI change
# (a signature, a status value, a dropped function) shows up as a header diff
# in the very PR that made it, where a reviewer has to look at it.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
crate="$root/crates/bugsee-ffi"
header="$crate/include/bugsee.h"

generate() {
  (cd "$crate" && cbindgen --config cbindgen.toml --crate bugsee-ffi --output "$1" >/dev/null 2>&1)
}

case "${1:-}" in
  gen)
    generate "$header"
    echo "wrote $header"
    ;;
  check)
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT

    generate "$tmp/bugsee.h"
    if ! diff -u "$header" "$tmp/bugsee.h"; then
      echo "error: include/bugsee.h is out of date; run scripts/ffi-header.sh gen and commit it" >&2
      exit 1
    fi

    # Declared functions must be exactly the exported symbols.
    grep -oE '\bbugsee_[a-z0-9_]+\(' "$header" | tr -d '(' | sort -u >"$tmp/declared"
    (cd "$root" && cargo build -q -p bugsee-ffi)
    lib="$root/target/debug/libbugsee_ffi.so"
    if [ -f "$lib" ]; then
      nm -D --defined-only "$lib" | awk '$3 ~ /^bugsee_/ {print $3}' | sort -u >"$tmp/exported"
    else
      lib="$root/target/debug/libbugsee_ffi.dylib"
      nm -gU "$lib" | awk '$3 ~ /^_bugsee_/ {sub(/^_/, "", $3); print $3}' | sort -u >"$tmp/exported"
    fi
    if ! diff -u "$tmp/declared" "$tmp/exported"; then
      echo "error: the header and the library's exported bugsee_* symbols differ (< header, > library)" >&2
      exit 1
    fi

    # The header must be usable as plain C and as C++.
    echo '#include "bugsee.h"' >"$tmp/t.c"
    cc -std=c99 -Wall -Werror -pedantic -fsyntax-only -I"$crate/include" "$tmp/t.c"
    cp "$tmp/t.c" "$tmp/t.cc"
    c++ -std=c++11 -Wall -Werror -pedantic -fsyntax-only -I"$crate/include" "$tmp/t.cc"
    echo "ffi header ok ($(wc -l <"$tmp/declared") functions)"
    ;;
  *)
    echo "usage: $0 gen|check" >&2
    exit 2
    ;;
esac
