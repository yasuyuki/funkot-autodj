#!/usr/bin/env bash
# Each ./dev.sh test run must execute a binary built from its own checkout.
# Two fixture sources share crate and test names; A passes, B fails.
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/dev-source-attribution.XXXXXXXX")
export DEV_TARGET_PREFIX=dev-source-attribution-$$
cleanup() {
    docker volume ls -q --filter "name=^$DEV_TARGET_PREFIX-" | xargs -r docker volume rm >/dev/null
    rm -rf -- "$work"
}
trap cleanup EXIT
failures=0

fixture() { # dir value [mtime]
    mkdir -p "$1/fx/src" "$1/tools"
    cp "$root/dev.sh" "$root/Dockerfile" "$1/"
    cp "$root/tools/dev_source_guard.py" "$1/tools/"
    printf '[workspace]\nmembers = ["fx"]\nresolver = "2"\n' > "$1/Cargo.toml"
    printf '[package]\nname = "fx"\nversion = "0.1.0"\nedition = "2021"\n' > "$1/fx/Cargo.toml"
    printf 'pub const VALUE: u32 = %s;\n#[test]\nfn value() { println!("fx-value={}", VALUE); assert_eq!(VALUE, 1); }\n' "$2" > "$1/fx/src/lib.rs"
    if [[ -n ${3:-} ]]; then find "$1" -exec touch -h -d "$3" {} +; fi
}

expect() { # dir A|B label
    local out want=fx-value=1 status=0
    [[ $2 == B ]] && want=fx-value=2
    out=$("$1/dev.sh" cargo test -- --nocapture 2>&1) || status=$?
    if [[ $2 == A && $status == 0 || $2 == B && $status != 0 ]] && grep -q "$want" <<<"$out"; then
        echo "ok   $3"
    else
        echo "FAIL $3: exit $status, expected source $2"; echo "$out" | tail -20; failures=$((failures + 1))
    fi
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
sed -i 's/VALUE: u32 = 1/VALUE: u32 = 2/' "$work/mutation/fx/src/lib.rs"
expect "$work/mutation" B "mutation: mutant"
cp -p "$work/lib.rs" "$work/mutation/fx/src/lib.rs"
expect "$work/mutation" A "mutation: restored with its old mtime"

exit $((failures > 0))
