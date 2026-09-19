#!/usr/bin/env bash
# Keep upstream-only paths out of this fork.
#
#   upstream_excluded_paths.sh check   # fail when any excluded path is tracked or present (CI)
#   upstream_excluded_paths.sh prune   # `git rm` every excluded path, e.g. after `git merge --no-commit`
#
# The list lives in .github/upstream-excluded-paths.txt. Git has no attribute
# that keeps a file deleted across merges (merge=ours only resolves content
# conflicts), so modify/delete conflicts on these paths are resolved here.
set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)"
LIST_FILE="${REPO_ROOT}/.github/upstream-excluded-paths.txt"

usage() {
    echo "usage: $0 {check|prune}" >&2
    exit 2
}

[ "$#" -eq 1 ] || usage
mode="$1"
case "${mode}" in
    check|prune) ;;
    *) usage ;;
esac

paths=()
while IFS= read -r line; do
    line="${line%%#*}"
    line="${line#"${line%%[![:space:]]*}"}"
    line="${line%"${line##*[![:space:]]}"}"
    [ -n "${line}" ] && paths+=("${line}")
done < "${LIST_FILE}"

if [ "${#paths[@]}" -eq 0 ]; then
    echo "FAIL: ${LIST_FILE} lists no paths" >&2
    exit 1
fi

cd "${REPO_ROOT}"

if [ "${mode}" = "prune" ]; then
    for path in "${paths[@]}"; do
        if [ -n "$(git ls-files -- "${path}")" ]; then
            git rm -r -q --ignore-unmatch -- "${path}"
            echo "removed: ${path}"
        fi
    done
fi

status=0
for path in "${paths[@]}"; do
    if [ -n "$(git ls-files -- "${path}")" ]; then
        echo "FAIL: upstream-only path is tracked: ${path}" >&2
        status=1
    elif [ -e "${path}" ]; then
        echo "FAIL: upstream-only path exists untracked, remove it: ${path}" >&2
        status=1
    fi
done

if [ "${status}" -eq 0 ]; then
    echo "PASS: upstream-only paths are excluded"
fi
exit "${status}"
