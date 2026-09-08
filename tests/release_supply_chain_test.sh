#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
COMPOSE_FILE="${REPO_ROOT}/docker-compose.yml"
RELEASE_WORKFLOW="${REPO_ROOT}/.github/workflows/release.yml"
TUNNEL_RELEASE_WORKFLOW="${REPO_ROOT}/.github/workflows/build-tunnel.yml"
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
assert_line "${RELEASE_WORKFLOW}" "          push-to-registry: true"
assert_line "${RELEASE_WORKFLOW}" "          subject-path: |"
assert_line "${RELEASE_WORKFLOW}" "            release-assets/install.sh"
assert_line "${RELEASE_WORKFLOW}" "            release-assets/SHA256SUMS"
assert_line "${RELEASE_WORKFLOW}" "            release-assets/AETHER_RELEASE_PROVENANCE.sigstore.json"

assert_line "${TUNNEL_RELEASE_WORKFLOW}" "      attestations: write"
assert_line "${TUNNEL_RELEASE_WORKFLOW}" "      id-token: write"
assert_line "${TUNNEL_RELEASE_WORKFLOW}" \
    "        uses: actions/attest@1e69f48acb82d1966a394da916b4c1698aa569d6 # v4.2.2"
assert_line "${TUNNEL_RELEASE_WORKFLOW}" "            artifacts/SHA256SUMS.txt"
assert_line "${TUNNEL_RELEASE_WORKFLOW}" \
    "            artifacts/AETHER_TUNNEL_RELEASE_PROVENANCE.sigstore.json"
assert_line "${TUNNEL_RELEASE_WORKFLOW}" \
    "          tar czf ../../../aether-tunnel-\${{ matrix.name }}.tar.gz aether-tunnel.exe"

if grep -ERq '^[[:space:]]*(-[[:space:]]+)?uses:[[:space:]]+[^[:space:]#]+@[^0-9a-f[:space:]][^[:space:]]*([[:space:]#]|$)' \
    "${REPO_ROOT}/.github/workflows"; then
    fail_test "workflow contains a mutable third-party action reference"
fi

echo "PASS: release supply-chain pins and provenance workflow"
