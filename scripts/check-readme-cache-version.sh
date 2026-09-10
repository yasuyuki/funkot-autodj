#!/usr/bin/env sh
set -eu

version=$(sed -n 's/^pub const CACHE_VERSION: u32 = \([0-9][0-9]*\);$/\1/p' funkot-core/src/cache.rs)
test -n "$version"
grep -F "(currently v$version)" README.md >/dev/null
grep -F "（現行は v$version）" README_ja.md >/dev/null
grep -F "(saat ini v$version)" README_id.md >/dev/null
