#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
# shellcheck source=../install.sh
source "${REPO_ROOT}/install.sh"

fail_test() {
    echo "FAIL: $*" >&2
    exit 1
}

assert_generated_key_output() {
    local label="$1"
    local output="$2"
    local key value
    local -a values=()

    for key in \
        JWT_SECRET_KEY ENCRYPTION_KEY DB_PASSWORD REDIS_PASSWORD \
        MYSQL_PASSWORD MYSQL_ROOT_PASSWORD; do
        value="$(printf '%s\n' "${output}" | awk -F= -v key="${key}" '
            $1 == key { print substr($0, length(key) + 2); exit }
        ')"
        [[ "${value}" =~ ^[A-Za-z0-9_-]{40,}$ ]] \
            || fail_test "${label} did not generate a strong URL-safe ${key}"
        values+=("${value}")
    done
    [[ "$(printf '%s\n' "${values[@]}" | sort -u | wc -l | tr -d '[:space:]')" == "6" ]] \
        || fail_test "${label} reused a generated secret"
}

assert_line() {
    local file="$1"
    local expected="$2"
    grep -Fqx -- "${expected}" "${file}" \
        || fail_test "missing expected line in ${file}: ${expected}"
}

APP_DOCKERFILE="${REPO_ROOT}/Dockerfile.app"
COMPOSE_FILES=(
    "${REPO_ROOT}/docker-compose.yml"
    "${REPO_ROOT}/docker-compose.single-node.yml"
)

assert_line "${APP_DOCKERFILE}" "USER 65532:65532"
assert_line "${APP_DOCKERFILE}" "    HOME=/tmp/aether-home \\"
if grep -Eq '^USER[[:space:]]+(root|0)(:0)?[[:space:]]*$' "${APP_DOCKERFILE}"; then
    fail_test "production image still selects a root runtime identity"
fi

for compose_file in "${COMPOSE_FILES[@]}"; do
    assert_line "${compose_file}" \
        '    user: "${AETHER_CONTAINER_UID:-65532}:${AETHER_CONTAINER_GID:-65532}"'
    assert_line "${compose_file}" "    read_only: true"
    assert_line "${compose_file}" "    cap_drop:"
    assert_line "${compose_file}" "      - ALL"
    assert_line "${compose_file}" "    security_opt:"
    assert_line "${compose_file}" "      - no-new-privileges:true"
    assert_line "${compose_file}" "    tmpfs:"
    assert_line "${compose_file}" "      - /tmp:rw,nosuid,nodev,noexec,mode=1777"
    if grep -Eq '^[[:space:]]+user:[[:space:]]+"?(root|0)(:0)?"?[[:space:]]*$' "${compose_file}"; then
        fail_test "production Compose file still selects a root runtime identity: ${compose_file}"
    fi
done

standard_install_body="$(declare -f install_compose_mode)"
single_node_install_body="$(declare -f install_compose_single_node_mode)"
if grep -Fq "prepare_compose_single_node_data_directory" <<<"${standard_install_body}"; then
    fail_test "standard Postgres Compose install unexpectedly prepares a SQLite data directory"
fi
grep -Fq "prepare_compose_single_node_data_directory" <<<"${single_node_install_body}" \
    || fail_test "single-node Compose install does not prepare its SQLite data directory"

assert_line "${REPO_ROOT}/.env.example" "DB_PASSWORD="
assert_line "${REPO_ROOT}/.env.example" "REDIS_PASSWORD="
assert_line "${REPO_ROOT}/.env.example" "MYSQL_PASSWORD="
assert_line "${REPO_ROOT}/.env.example" "MYSQL_ROOT_PASSWORD="
assert_line "${REPO_ROOT}/README.md" "chmod 600 .env"
assert_line "${REPO_ROOT}/README.md" \
    "docker compose -f docker-compose.single-node.yml stop app"
assert_line "${REPO_ROOT}/README.md" \
    "sudo chown -R -P 65532:65532 ./data"
grep -Fq '${DB_PASSWORD:?set DB_PASSWORD in .env}' "${REPO_ROOT}/docker-compose.yml" \
    || fail_test "Postgres password is not required by Compose"
grep -Fq '${REDIS_PASSWORD:?set REDIS_PASSWORD in .env}' "${REPO_ROOT}/docker-compose.yml" \
    || fail_test "Redis password is not required by Compose"
grep -Fq '${MYSQL_PASSWORD:?set MYSQL_PASSWORD in .env}' "${REPO_ROOT}/docker-compose.yml" \
    || fail_test "MySQL application password is not required by Compose"
grep -Fq '${MYSQL_ROOT_PASSWORD:?set MYSQL_ROOT_PASSWORD in .env}' "${REPO_ROOT}/docker-compose.yml" \
    || fail_test "MySQL root password is not required by Compose"
if grep -Eq '(DB_PASSWORD|REDIS_PASSWORD|MYSQL_PASSWORD|MYSQL_ROOT_PASSWORD):?-?=?aether(_root)?([}"[:space:]]|$)' \
    "${REPO_ROOT}/docker-compose.yml" "${REPO_ROOT}/.env.example"; then
    fail_test "production Compose configuration contains a weak infrastructure password default"
fi

TEST_ROOT="$(mktemp -d)"
trap 'chmod -R u+rwX "${TEST_ROOT}" 2>/dev/null || true; rm -rf "${TEST_ROOT}"' EXIT
COMPOSE_DIR="${TEST_ROOT}/compose"
mkdir -p "${COMPOSE_DIR}/data"
fake_bin="${TEST_ROOT}/fake-bin"
mkdir -p "${fake_bin}"
printf '#!/usr/bin/env bash\nprintf "false\\n"\n' >"${fake_bin}/docker"
chmod 0755 "${fake_bin}/docker"
PATH="${fake_bin}:${PATH}"
fixture_uid="$(id -u)"
fixture_gid="$(id -g)"
[[ "${fixture_uid}" != "0" ]] || fixture_uid="65532"
[[ "${fixture_gid}" != "0" ]] || fixture_gid="65532"
printf 'AETHER_CONTAINER_UID=%s\nAETHER_CONTAINER_GID=%s\n' \
    "${fixture_uid}" "${fixture_gid}" >"${COMPOSE_DIR}/.env"
printf 'sqlite fixture\n' >"${COMPOSE_DIR}/data/aether.db"
chmod 0755 "${COMPOSE_DIR}/data/aether.db"
chmod 4755 "${COMPOSE_DIR}/data/aether.db" 2>/dev/null || true

prepare_compose_single_node_data_directory
[[ "$(stat_file_mode "${COMPOSE_DIR}/data")" == "700" ]] \
    || fail_test "single-node data directory was not restricted to mode 0700"
[[ "$(stat_file_mode "${COMPOSE_DIR}/data/aether.db")" == "600" ]] \
    || fail_test "single-node SQLite file was not restricted to mode 0600"

printf 'AETHER_CONTAINER_UID=0\nAETHER_CONTAINER_GID=%s\n' \
    "${fixture_gid}" >"${COMPOSE_DIR}/.env"
if (prepare_compose_single_node_data_directory) >/dev/null 2>&1; then
    fail_test "root container uid was accepted"
fi

printf 'AETHER_CONTAINER_UID=%s\nAETHER_CONTAINER_GID=%s\n' \
    "${fixture_uid}" "${fixture_gid}" >"${COMPOSE_DIR}/.env"
ln -s "${TEST_ROOT}" "${COMPOSE_DIR}/data/unsafe-link"
if (prepare_compose_single_node_data_directory) >/dev/null 2>&1; then
    fail_test "symbolic link inside the managed SQLite directory was accepted"
fi

rm -f "${COMPOSE_DIR}/data/unsafe-link"
mkfifo "${COMPOSE_DIR}/data/unsafe-fifo"
if (prepare_compose_single_node_data_directory) >/dev/null 2>&1; then
    fail_test "FIFO inside the managed SQLite directory was accepted"
fi
rm -f "${COMPOSE_DIR}/data/unsafe-fifo"

printf '#!/usr/bin/env bash\nprintf "true\\n"\n' >"${fake_bin}/docker"
chmod 0755 "${fake_bin}/docker"
if (prepare_compose_single_node_data_directory) >/dev/null 2>&1; then
    fail_test "running app container did not block SQLite permission migration"
fi
printf '#!/usr/bin/env bash\nprintf "false\\n"\n' >"${fake_bin}/docker"
chmod 0755 "${fake_bin}/docker"

ln "${COMPOSE_DIR}/data/aether.db" "${COMPOSE_DIR}/data/unsafe-hardlink"
if (prepare_compose_single_node_data_directory) >/dev/null 2>&1; then
    fail_test "hard link inside the managed SQLite directory was accepted"
fi
rm -f "${COMPOSE_DIR}/data/unsafe-hardlink"

cp "${REPO_ROOT}/.env.example" "${COMPOSE_DIR}/.env.example"
ADMIN_PASSWORD="test-admin-password"
APP_IMAGE="example.invalid/aether:test"
AETHER_CONTAINER_UID="${fixture_uid}"
AETHER_CONTAINER_GID="${fixture_gid}"
JWT_SECRET_KEY=""
ENCRYPTION_KEY=""
generated_env="${TEST_ROOT}/generated.env"
generate_compose_env "${generated_env}"

generated_secrets=()
for key in \
    JWT_SECRET_KEY ENCRYPTION_KEY DB_PASSWORD REDIS_PASSWORD \
    MYSQL_PASSWORD MYSQL_ROOT_PASSWORD; do
    value="$(env_file_value "${generated_env}" "${key}")"
    [[ "${value}" =~ ^[A-Za-z0-9_-]{40,}$ ]] \
        || fail_test "installer did not generate a strong URL-safe ${key}"
    generated_secrets+=("${value}")
done
[[ "$(printf '%s\n' "${generated_secrets[@]}" | sort -u | wc -l | tr -d '[:space:]')" == "6" ]] \
    || fail_test "installer reused a generated secret"

assert_generated_key_output \
    "repository generate_keys.sh" "$("${REPO_ROOT}/generate_keys.sh")"
generated_key_script="${TEST_ROOT}/generated-keys.sh"
write_generate_keys_script "${generated_key_script}"
assert_generated_key_output \
    "installer-generated key script" "$("${generated_key_script}")"

injection_env="${TEST_ROOT}/injection.env"
printf 'SAFE=value\n' >"${injection_env}"
if (replace_or_append_env "${injection_env}" "SAFE" $'value\nINJECTED=true') >/dev/null 2>&1; then
    fail_test "dotenv CR/LF injection was accepted"
fi
grep -Fqx "SAFE=value" "${injection_env}" \
    || fail_test "rejected dotenv injection still modified the target"
if grep -Fq "INJECTED" "${injection_env}"; then
    fail_test "rejected dotenv injection added another variable"
fi

literal_backslash_value='literal\nINJECTED=true'
replace_or_append_env "${injection_env}" "SAFE" "${literal_backslash_value}"
grep -Fqx "SAFE=${literal_backslash_value}" "${injection_env}" \
    || fail_test "dotenv replacement interpreted a literal backslash escape"
[[ "$(wc -l <"${injection_env}" | tr -d '[:space:]')" == "1" ]] \
    || fail_test "literal backslash escape injected another dotenv line"

echo "PASS: production container runtime hardening fixtures"
