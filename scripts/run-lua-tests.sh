#!/usr/bin/env bash
# Official Lua 5.4.9 conformance suite for slew.
#
# Downloads the pinned upstream archive (checksum-verified) into
# target/lua-tests on first use, then runs the curated corpus in
# tests/conformance.rs. Results are compared against
# tests/lua-conformance.baseline: a file that fails earlier than the
# baseline is a regression; a Rust panic is always a failure.
#
# Usage:
#   scripts/run-lua-tests.sh              # run the corpus
#   SLEW_BLESS=1 scripts/run-lua-tests.sh # refresh the baseline
#   SLEW_LUA_TESTS_DIR=/path scripts/...  # use an existing extraction
#   SLEW_LUA_TESTS_CACHE=/dir scripts/... # where to download/cache the archive
#   SLEW_LUA_TESTS_TIMEOUT=60 scripts/... # per-file wall-clock budget (s)
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
version="5.4.9"
sha256="7d971845f545ffc09fbb3128a86b2c6524161c70d0fdf0154a16e8c00c343fca"
# Kept separate from `target/` so CI can cache the (pinned, checksum-verified)
# suite independently of build artifacts.
cache_dir="${SLEW_LUA_TESTS_CACHE:-$repo_root/target/lua-tests}"
suite_dir="${SLEW_LUA_TESTS_DIR:-$cache_dir/lua-$version-tests}"

check_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    echo "$sha256  $1" | sha256sum -c -
  else
    echo "$sha256  $1" | shasum -a 256 -c -
  fi
}

if [ ! -f "$suite_dir/vararg.lua" ]; then
  if [ -n "${SLEW_LUA_TESTS_DIR:-}" ]; then
    echo "error: SLEW_LUA_TESTS_DIR=$SLEW_LUA_TESTS_DIR is not an extracted suite" >&2
    exit 1
  fi
  mkdir -p "$cache_dir"
  archive="$cache_dir/lua-$version-tests.tar.gz"
  echo "fetching https://www.lua.org/tests/lua-$version-tests.tar.gz" >&2
  curl -fsSL -o "$archive" "https://www.lua.org/tests/lua-$version-tests.tar.gz"
  check_sha256 "$archive"
  tar -xzf "$archive" -C "$cache_dir"
  suite_dir="$cache_dir/lua-$version-tests"
fi

export SLEW_LUA_TESTS_DIR="$suite_dir"
cd "$repo_root"
exec cargo test --test conformance -- --ignored --nocapture "$@"
