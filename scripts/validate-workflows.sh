#!/usr/bin/env bash
# Validate GitHub Actions workflow pinning.
#
# Usage:
#   validate-workflows.sh                       # validate .github/workflows + policy files
#   validate-workflows.sh FILE_OR_DIR [ ... ]   # validate explicit files/directories
#
# Remote actions must be pinned to exactly 40 lowercase hex characters:
#   uses: owner/repo[/path]@<40 lowercase hex> # v1.2.3
# A trailing version comment is optional, but the ref itself is not.
# docker:// actions are accepted only when pinned with a 64 hex sha256 digest.
# Local ./ actions are accepted: they are pinned by this repository's commit.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fail=0
explicit_paths=0

trim() {
  local value="$1"
  value="${value#"${value%%[![:space:]]*}"}"
  value="${value%"${value##*[![:space:]]}"}"
  printf '%s' "$value"
}

# Returns 0 when the uses: value is an accepted pin.
check_uses_value() {
  local raw="$1" ref comment=""

  if [[ "$raw" == *"#"* ]]; then
    ref="$(trim "${raw%%#*}")"
    comment="$(trim "${raw#*#}")"
    if [[ -z "$comment" || "$comment" == *"#"* ]]; then
      return 1
    fi
  else
    ref="$(trim "$raw")"
  fi

  # Local actions are checked in here, so the repository commit is the pin.
  if [[ "$ref" == ./* ]]; then
    [[ -n "${ref#./}" ]]
    return $?
  fi

  # Container images must be pinned by digest, not by tag (or unpinned).
  if [[ "$ref" == docker://* ]]; then
    [[ "$ref" =~ ^docker://[^[:space:]]+@sha256:[0-9a-f]{64}$ ]]
    return $?
  fi

  # owner/repo[/path]@<exactly 40 lowercase hex>. Uppercase, branches, tags,
  # short SHAs and long SHAs are all rejected.
  [[ "$ref" =~ ^[A-Za-z0-9][A-Za-z0-9_.-]*/[A-Za-z0-9_.-]+(/[A-Za-z0-9_.-]+)*@[0-9a-f]{40}$ ]]
}

check_file() {
  local f="$1" line_no=0 line value
  if [ ! -f "$f" ]; then
    echo "missing: $f" >&2
    fail=1
    return
  fi
  if ! grep -q '^permissions: {}' "$f"; then
    echo "missing top-level permissions: {} in $f" >&2
    fail=1
  fi
  while IFS= read -r line || [ -n "$line" ]; do
    line_no=$((line_no + 1))
    case "$line" in
      [[:space:]]*uses:*) ;;
      *) continue ;;
    esac
    value="${line#*uses:}"
    if ! check_uses_value "$value"; then
      echo "invalid uses: pin in $f:$line_no: $(trim "$line")" >&2
      echo "  expected owner/repo[/path]@<40 lowercase hex> [ # version], a digest-pinned docker:// image, or a local ./ action" >&2
      fail=1
    fi
  done < "$f"
}

check_path() {
  local p="$1"
  if [ -d "$p" ]; then
    local f
    for f in "$p"/*.yml "$p"/*.yaml; do
      [ -e "$f" ] || continue
      check_file "$f"
    done
  else
    check_file "$p"
  fi
}

paths=()
while [ "$#" -gt 0 ]; do
  case "$1" in
    --workflows-dir|--dir|--file)
      flag="$1"
      shift
      [ "$#" -gt 0 ] || { echo "missing argument to $flag" >&2; exit 2; }
      paths+=("$1")
      ;;
    -h|--help)
      echo "usage: ${BASH_SOURCE[0]} [FILE_OR_DIR ...] [--workflows-dir DIR] [--file FILE]"
      exit 0
      ;;
    *)
      paths+=("$1")
      ;;
  esac
  shift
done

if [ "${#paths[@]}" -eq 0 ] && [ -n "${TUISHOU_WORKFLOWS_DIR:-}" ]; then
  paths=("$TUISHOU_WORKFLOWS_DIR")
fi

if [ "${#paths[@]}" -gt 0 ]; then
  explicit_paths=1
  for p in "${paths[@]}"; do
    check_path "$p"
  done
else
  check_path "$root/.github/workflows"
fi

if [ "$explicit_paths" -eq 0 ]; then
  # dependency-review allow-licenses should include deny.toml allows
  if ! grep -q 'NCSA' "$root/.github/workflows/dependency-review.yml"; then
    echo "dependency-review missing NCSA" >&2
    fail=1
  fi
fi

if [ "$fail" -ne 0 ]; then
  echo "workflow validation failed" >&2
  exit 1
fi
echo "workflow validation ok"
