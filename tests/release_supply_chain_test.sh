#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
COMPOSE_FILE="${REPO_ROOT}/docker-compose.yml"
RELEASE_WORKFLOW="${REPO_ROOT}/.github/workflows/publish-docker.yml"
APP_DOCKERFILE="${REPO_ROOT}/Dockerfile.app"

fail_test() {
    echo "FAIL: $*" >&2
    exit 1
}

assert_line() {
    local file="$1"
    local expected="$2"
    grep -Fqx -- "${expected}" "${file}" \
        || fail_test "missing expected line in ${file}: ${expected}"
}

assert_line "${COMPOSE_FILE}" \
    "    image: postgres:15.19@sha256:5f72c7b5bd616308ccfd2e74d6be16fb06364e5eecbb815fe9dc6ab9761d2111"
assert_line "${COMPOSE_FILE}" \
    "    image: redis:7.4.11-alpine@sha256:ff02b58f971e7d7d156a1267e283fcbbeee91773b6aa36c49dac28ecfe28eadf"
assert_line "${COMPOSE_FILE}" \
    "    image: mysql:8.0.46@sha256:7dcddc01f13bab2f15cde676d44d01f61fc9f99fe7785e86196dfc07d358ae2b"

if grep -Eq '^[[:space:]]+image:[[:space:]]+(postgres|redis|mysql):[^@[:space:]]+[[:space:]]*$' "${COMPOSE_FILE}"; then
    fail_test "compose contains a mutable third-party image tag"
fi

assert_line "${APP_DOCKERFILE}" \
    "FROM busybox:1.37.0-musl@sha256:fc6dddc4c44b1bfe37f41cae8e67d1693828e8f42a91862816d7953e2c9d3f23 AS layout"
assert_line "${APP_DOCKERFILE}" \
    "FROM gcr.io/distroless/static-debian12@sha256:6447365a6337c3732f412d1b74357b30a633831955b2bc45552b0086be907687"

if grep -Eq '^FROM[[:space:]]+[^[:space:]@]+(:[^[:space:]@]+)?([[:space:]]+AS[[:space:]]+[^[:space:]]+)?$' "${APP_DOCKERFILE}"; then
    fail_test "production Dockerfile contains an unpinned base image"
fi

assert_line "${RELEASE_WORKFLOW}" "      attestations: write"
assert_line "${RELEASE_WORKFLOW}" "      id-token: write"
assert_line "${RELEASE_WORKFLOW}" \
    "        uses: actions/attest@1e69f48acb82d1966a394da916b4c1698aa569d6 # v4.2.2"
assert_line "${RELEASE_WORKFLOW}" '          subject-digest: ${{ steps.push.outputs.digest }}'
assert_line "${RELEASE_WORKFLOW}" "          push-to-registry: false"
assert_line "${RELEASE_WORKFLOW}" '          subject-name: ${{ steps.image.outputs.name }}'
assert_line "${RELEASE_WORKFLOW}" "        id: push"

python3 - "${REPO_ROOT}" <<'PY'
import pathlib
import re
import sys

root = pathlib.Path(sys.argv[1])
for workflow in (root / ".github/workflows").glob("*.yml"):
    for reference in re.findall(r"^\s*(?:-\s+)?uses:\s*(\S+)", workflow.read_text(), re.M):
        if reference.startswith("./"):
            assert (root / reference).is_file(), f"{workflow}: missing local workflow {reference}"
        else:
            assert re.fullmatch(r"[^@]+@[0-9a-f]{40}", reference), (
                f"{workflow}: action must use a full commit SHA: {reference}"
            )
PY

echo "PASS: release supply-chain pins and provenance workflow"
