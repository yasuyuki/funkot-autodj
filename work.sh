#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
TESTDATA="$ROOT/testdata"
WSL=0

is_supported() {
  case "$1" in
    mp3|m4a|aac|flac|ogg|oga|wav) return 0 ;;
    *) return 1 ;;
  esac
}

is_audio() {
  case "$1" in
    mp3|m4a|aac|flac|ogg|oga|wav|wma|aiff|aif|aifc|opus|ape|wv|mpc|caf|ac3|dts|tak|tta)
      return 0 ;;
    *) return 1 ;;
  esac
}

win_to_wsl() {
  local p="$1"
  if command -v wslpath >/dev/null 2>&1; then
    wslpath -u "$p"
    return
  fi
  if [[ "$p" =~ ^[A-Za-z]: ]]; then
    local drive rest
    drive=$(printf '%s' "${p:0:1}" | tr 'A-Z' 'a-z')
    rest="${p:2}"
    rest="${rest//\\//}"
    printf '/mnt/%s%s\n' "$drive" "$rest"
  else
    printf '%s\n' "$p"
  fi
}

_win_to_wsl_bash() {
  local p="$1" drive rest
  [[ "$p" =~ ^[A-Za-z]: ]] || { printf '%s\n' "$p"; return; }
  drive=$(printf '%s' "${p:0:1}" | tr 'A-Z' 'a-z')
  rest="${p:2}"
  rest="${rest//\\//}"
  printf '/mnt/%s%s\n' "$drive" "$rest"
}

has_list_arg() {
  local a
  for a in "$@"; do
    case "$a" in
      -l|--list|--list=*) return 0 ;;
    esac
  done
  return 1
}

if [[ "${1:-}" == --self-check ]]; then
  got="$(_win_to_wsl_bash 'C:\Users\foo\bar')"
  [[ "$got" == '/mnt/c/Users/foo/bar' ]] || { echo "fail: $got"; exit 1; }
  got="$(_win_to_wsl_bash 'D:/Music/x.wma')"
  [[ "$got" == '/mnt/d/Music/x.wma' ]] || { echo "fail: $got"; exit 1; }
  is_supported flac && ! is_supported wma && is_audio wma
  has_list_arg --render out.wav && { echo "fail: false positive list"; exit 1; }
  has_list_arg -l x.txt --render out.wav || { echo "fail: missed -l"; exit 1; }
  has_list_arg --list=x.txt || { echo "fail: missed --list="; exit 1; }
  _p="$ROOT/testdata/x.txt"
  [[ "${_p#"$ROOT"/}" == "testdata/x.txt" ]] || { echo "fail: root strip"; exit 1; }
  echo ok
  exit 0
fi

while [[ $# -gt 0 ]]; do
  case "$1" in
    -w|--wsl) WSL=1; shift ;;
    -*) echo "unknown option: $1" >&2; exit 2 ;;
    *) break ;;
  esac
done

[[ $# -ge 1 ]] || { echo "usage: $0 [-w|--wsl] <music-dir> [args...]" >&2; exit 2; }

DIR="$1"
shift
if [[ "$WSL" -eq 1 ]]; then
  DIR="$(win_to_wsl "$DIR")"
fi
[[ -d "$DIR" ]] || { echo "not a directory: $DIR" >&2; exit 1; }
DIR="$(cd "$DIR" && pwd)"

mkdir -p "$TESTDATA"

TRACKS=()
shopt -s nullglob
for f in "$DIR"/*; do
  [[ -f "$f" ]] || continue
  ext="${f##*.}"
  ext="$(printf '%s' "$ext" | tr 'A-Z' 'a-z')"
  is_audio "$ext" || continue
  if is_supported "$ext"; then
    TRACKS+=("$f")
    continue
  fi
  base="$(basename "$f")"
  base="${base%.*}"
  out="$TESTDATA/${base}.flac"
  if [[ -e "$out" ]]; then
    echo "skip (exists): $out" >&2
  else
    echo "convert: $f -> $out" >&2
    ffmpeg -nostdin -hide_banner -loglevel error -n -i "$f" "$out"
  fi
  TRACKS+=("$out")
done

to_cli_path() {
  local p="$1"
  if [[ "$p" == "$ROOT"/* ]]; then
    printf '%s\n' "${p#"$ROOT"/}"
  else
    printf '%s\n' "$p"
  fi
}

ARGS=()
for a in "$@"; do
  if [[ "$WSL" -eq 1 && "$a" =~ ^[A-Za-z]: ]]; then
    a="$(win_to_wsl "$a")"
  fi
  ARGS+=("$(to_cli_path "$a")")
done

if ! has_list_arg "${ARGS[@]}"; then
  PL_REL="testdata/work_playlist.txt"
  PL="$ROOT/$PL_REL"
  : > "$PL"
  if [[ ${#TRACKS[@]} -gt 0 ]]; then
    for t in "${TRACKS[@]}"; do
      printf '%s\t%s\n' "$(basename "$t")" "$(to_cli_path "$t")"
    done | LC_ALL=C sort | cut -f2- > "$PL"
  fi
  echo "auto playlist: $PL_REL (${#TRACKS[@]} tracks)" >&2
  ARGS=("-l" "$PL_REL" "${ARGS[@]}")
fi

export DEV_BIND_SRC="$DIR"
export DEV_BIND_DST="$DIR"
exec "$ROOT/dev.sh" cargo run -p funkot-cli --release -- "${ARGS[@]}"
