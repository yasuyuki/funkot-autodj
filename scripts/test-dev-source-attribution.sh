#!/usr/bin/env bash
# Each ./dev.sh test run must execute a binary built from its own checkout.
# Two fixture sources share crate and test names; A passes, B fails.
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/dev-source-attribution.XXXXXXXX")
export DEV_TARGET_PREFIX=dev-source-attribution-$(python3 -c 'import uuid; print(uuid.uuid4().hex)')
unset WORKSPACE_LIFECYCLE_CONTEXT
docker volume create --label "org.funkot.attribution-fixture=$DEV_TARGET_PREFIX" "$DEV_TARGET_PREFIX" >/dev/null
cleanup() {
    test "$(docker volume inspect --format '{{index .Labels "org.funkot.attribution-fixture"}}' "$DEV_TARGET_PREFIX")" = "$DEV_TARGET_PREFIX" || return 1
    docker volume rm "$DEV_TARGET_PREFIX" >/dev/null
    rm -rf -- "$work"
}
trap cleanup EXIT
failures=0
dependency_built=0

fixture() { # dir value [mtime]
    mkdir -p "$1/fx/src" "$1/tools" "$1/tracked"
    cp "$root/dev.sh" "$root/Dockerfile" "$1/"
    cp "$root/tools/dev_source_guard.py" "$1/tools/"
    printf '[workspace]\nmembers = ["fx"]\nresolver = "2"\n' > "$1/Cargo.toml"
    printf '[package]\nname = "fx"\nversion = "0.1.0"\nedition = "2021"\n[dependencies]\nitoa="=1.0.15"\n' > "$1/fx/Cargo.toml"
    printf 'pub const VALUE: u32 = %s;\ninclude!(concat!(env!("OUT_DIR"),"/value.rs"));\n#[test]\nfn value() { println!("fx-value={} build-value={} quote-value={} itoa={}", VALUE, BUILD, quoted::value(), itoa::Buffer::new().format(VALUE)); assert_eq!((VALUE,BUILD,quoted::value()), (1,1,1)); }\n' "$2" > "$1/fx/src/lib.rs"
    cat >> "$1/fx/src/lib.rs" <<'RS'
mod quoted { include!("space 'quote'.rs"); }
RS
    printf 'pub fn value()->u32{1}\n' > "$1/fx/src/space 'quote'.rs"
    printf 'fn main(){println!("forwarded={}",std::env::args().skip(1).collect::<Vec<_>>().join("|"));}\n' > "$1/fx/src/main.rs"
    printf '1\n' > "$1/tracked/value"
    printf 'fn main(){println!("cargo:rerun-if-changed=../tracked");let v=std::fs::read_to_string("../tracked/value").unwrap(); std::fs::write(format!("{}/value.rs",std::env::var("OUT_DIR").unwrap()),format!("pub const BUILD:u32={};",v.trim())).unwrap();}\n' > "$1/fx/build.rs"
    if [[ -n ${3:-} ]]; then find "$1" -exec touch -h -d "$3" {} +; fi
}

expect() { # dir A|B label
    local out want=fx-value=1 status=0 input_before input_after
    [[ $2 == B ]] && want=fx-value=2
    input_before=$(sha256sum "$1/fx/src/lib.rs" "$1/fx/build.rs" "$1/tracked/value" "$1/fx/src/space 'quote'.rs")
    out=$("$1/dev.sh" cargo test -- --nocapture 2>&1) || status=$?
    input_after=$(sha256sum "$1/fx/src/lib.rs" "$1/fx/build.rs" "$1/tracked/value" "$1/fx/src/space 'quote'.rs")
    if [[ $input_before != "$input_after" ]]; then echo 'FAIL source changed during fixture'; failures=$((failures + 1)); fi
    if [[ $2 == A && $status == 0 || $2 == B && $status == 101 ]] && grep -q "$want" <<<"$out" && grep -q '1 passed\|1 failed' <<<"$out"; then
        echo "ok   $3 ($want)"
    else
        echo "FAIL $3: exit $status, expected source $2"; echo "$out" | tail -20; failures=$((failures + 1))
    fi
    if grep -q 'Compiling itoa ' <<<"$out"; then
        if ((dependency_built)); then echo "FAIL unnecessary registry dependency rebuild: $3"; failures=$((failures + 1)); fi
        dependency_built=1
    fi
    printf 'before\n%s\nafter\n%s\n' "$input_before" "$input_after"
    grep 'dev-source-guard:' <<<"$out" || true
    LAST_OUTPUT=$out
}

replace() { # dir from
    find "$1" -mindepth 1 -maxdepth 1 -exec rm -rf {} +
    (cd "$2" && tar cf - .) | (cd "$1" && tar xpf -)
}

old="2001-01-01 00:00:00"
for mtime in "$old" ""; do
    for order in ABA BAB; do
        case_dir=$work/$order-${mtime:+old}
        fixture "$case_dir/A" 1 "$mtime"; fixture "$case_dir/B" 2 "$mtime"
        for step in $(grep -o . <<<"$order"); do
            expect "$case_dir/$step" "$step" "separate checkouts $order ${mtime:+old mtime}: $step"
        done
    done
done

fixture "$work/src/A" 1 "$old"; fixture "$work/src/B" 2 "2001-06-01 00:00:00"
mkdir "$work/inplace"
for step in A B A; do
    replace "$work/inplace" "$work/src/$step"
    expect "$work/inplace" "$step" "one path replaced in place by older files: $step"
done

fixture "$work/mutation" 1
cp -p "$work/mutation/fx/src/lib.rs" "$work/lib.rs"
expect "$work/mutation" A "mutation: original"
python3 - "$work/mutation/fx/src/lib.rs" <<'PY'
import os
import sys
path = sys.argv[1]
before = os.stat(path)
with open(path, 'r+', encoding='utf-8') as source:
    contents = source.read().replace('VALUE: u32 = 1', 'VALUE: u32 = 2')
    source.seek(0)
    source.write(contents)
    source.truncate()
os.utime(path, ns=(before.st_atime_ns, before.st_mtime_ns))
after = os.stat(path)
assert (after.st_ino, after.st_size, after.st_mtime_ns) == (before.st_ino, before.st_size, before.st_mtime_ns)
PY
expect "$work/mutation" B "mutation: same inode, size, and mtime"
cp -p "$work/lib.rs" "$work/mutation/fx/src/lib.rs"
expect "$work/mutation" A "mutation: restored with its old mtime"
for iteration in 1 2 3 4 5; do
    expect "$work/mutation" A "warm unchanged $iteration"
    if grep -q 'Compiling ' <<<"$LAST_OUTPUT"; then echo 'FAIL warm rebuild'; failures=$((failures + 1)); fi
done
python3 - "$work/mutation/tracked/value" <<'PY'
import os
import sys
path = sys.argv[1]
before = os.stat(path)
with open(path, 'r+b') as output:
    output.write(b'2\n')
os.utime(path, ns=(before.st_atime_ns, before.st_mtime_ns))
after = os.stat(path)
assert (after.st_ino, after.st_size, after.st_mtime_ns) == (before.st_ino, before.st_size, before.st_mtime_ns)
PY
out=$("$work/mutation/dev.sh" cargo test -- --nocapture 2>&1) && status=0 || status=$?
if [[ $status != 101 ]] || ! grep -q 'fx-value=1 build-value=2' <<<"$out"; then
    echo "FAIL tracked build-script input"; echo "$out"; failures=$((failures + 1))
else
    echo 'ok   build-script tracked directory input with unchanged metadata'
fi
printf '1\n' > "$work/mutation/tracked/value"
expect "$work/mutation" A "before quoted source mutation"
python3 - "$work/mutation/fx/src/space 'quote'.rs" <<'PY'
import os
import sys
path = sys.argv[1]
before = os.stat(path)
with open(path, 'r+b') as output:
    output.write(b'pub fn value()->u32{2}\n')
os.utime(path, ns=(before.st_atime_ns, before.st_mtime_ns))
PY
out=$("$work/mutation/dev.sh" cargo test -- --nocapture 2>&1) && status=0 || status=$?
if [[ $status != 101 ]] || ! grep -q 'quote-value=2' <<<"$out"; then
    echo "FAIL quoted source with unchanged metadata"; echo "$out"; failures=$((failures + 1))
else
    echo 'ok   source filename with spaces and quotes'
fi

out=$("$work/mutation/dev.sh" cargo run -p fx -- --target-dir /forwarded-argument 2>&1) && status=0 || status=$?
if [[ $status != 0 ]] || ! grep -q 'forwarded=--target-dir|/forwarded-argument' <<<"$out"; then
    echo "FAIL Cargo program argument forwarding"; echo "$out"; failures=$((failures + 1))
fi
out=$("$work/mutation/dev.sh" cargo test --target-dir /unguarded-output 2>&1) && status=0 || status=$?
if [[ $status != 2 ]] || ! grep -q 'Cargo --target-dir must match' <<<"$out"; then
    echo "FAIL alternate Cargo target was not refused"; echo "$out"; failures=$((failures + 1))
fi

fixture "$work/default-build" 1
printf '1\n' > "$work/default-build/fx/default-value"
printf 'fn main(){let v=std::fs::read_to_string("default-value").unwrap(); std::fs::write(format!("{}/value.rs",std::env::var("OUT_DIR").unwrap()),format!("pub const BUILD:u32={};",v.trim())).unwrap();}\n' > "$work/default-build/fx/build.rs"
expect "$work/default-build" A "build-script default package inputs"
python3 - "$work/default-build/fx/default-value" <<'PY'
import os
import sys
path = sys.argv[1]
before = os.stat(path)
with open(path, 'r+b') as output:
    output.write(b'2\n')
os.utime(path, ns=(before.st_atime_ns, before.st_mtime_ns))
PY
out=$("$work/default-build/dev.sh" cargo test -- --nocapture 2>&1) && status=0 || status=$?
if [[ $status != 101 ]] || ! grep -q 'fx-value=1 build-value=2' <<<"$out"; then
    echo "FAIL default build-script input"; echo "$out"; failures=$((failures + 1))
else
    echo 'ok   default build-script input with unchanged metadata'
fi

exit $((failures > 0))
