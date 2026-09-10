import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

repo_root = Path(__file__).resolve().parent.parent
environment = {key: os.environ[key] for key in ("HOME", "PATH") if key in os.environ}


def assert_app_security(app, *, user, capabilities):
    assert app["user"] == user
    assert app["read_only"] is True
    assert app["cap_drop"] == ["ALL"]
    assert set(app.get("cap_add", [])) == set(capabilities)
    assert app["security_opt"] == ["no-new-privileges:true"]
    assert not app.get("privileged", False)
    assert app["environment"]["AETHER_LOG_DESTINATION"] == "stdout"
    assert all(
        volume["target"] not in ("/app/logs", "/opt/aether/logs")
        for volume in app.get("volumes", [])
    )
    assert app["logging"]["driver"] == "json-file"
    assert app["logging"]["options"] == {"max-size": "100m", "max-file": "10"}


with tempfile.TemporaryDirectory(prefix="aether-compose-databases-") as directory:
    fixture = Path(directory)
    for filename in (
        "docker-compose.yml",
        "docker-compose.local.yml",
        "docker-compose.single-node.yml",
        "docker-compose.release-local.yml",
    ):
        shutil.copyfile(repo_root / filename, fixture / filename)
    env_file = fixture / ".env"

    def compose_config(files, *, overrides=None, profiles=()):
        command = [
            "docker", "compose", "--project-name", "aether-database-fixture",
            "--project-directory", str(fixture), "--env-file", str(env_file),
        ]
        for profile in profiles:
            command.extend(["--profile", profile])
        for filename in files:
            command.extend(["-f", str(fixture / filename)])
        return subprocess.run(
            command + ["config", "--format", "json"],
            env={**environment, **(overrides or {})},
            capture_output=True, text=True, check=False,
        )

    def checked_config(files, **kwargs):
        result = compose_config(files, **kwargs)
        assert result.returncode == 0, result.stderr
        return json.loads(result.stdout)

    database_environment = "DB_PASSWORD=fixture-postgres\nREDIS_PASSWORD=fixture-redis\n"
    standard_compose_files = (
        ["docker-compose.yml"],
        ["docker-compose.yml", "docker-compose.local.yml"],
    )
    env_file.write_text(database_environment)
    for files in standard_compose_files:
        config = checked_config(files)
        assert set(config["services"]) == {"app", "postgres", "redis"}
        assert set(config["volumes"]) == {"postgres_data"}
        app = config["services"]["app"]
        assert_app_security(app, user="0:0", capabilities={"DAC_OVERRIDE", "FOWNER"})
        app_env = app["environment"]
        assert app_env.get("AETHER_DATABASE_DRIVER", "postgres") == "postgres"
        assert app_env["DATABASE_URL"] == "postgresql://postgres:fixture-postgres@postgres:5432/aether"
        assert app_env["REDIS_URL"] == "redis://:fixture-redis@redis:6379/0"
        for key in ("DB_PASSWORD", "REDIS_PASSWORD"):
            result = compose_config(files, overrides={key: ""})
            assert result.returncode != 0, f"empty {key} was accepted"
            assert f"set {key} in .env" in result.stderr, result.stderr

        # The optional MySQL profile remains compatible with existing DB_PASSWORD setups.
        config = checked_config(files, profiles=["mysql"])
        assert set(config["services"]) == {"app", "postgres", "redis", "mysql"}
        assert set(config["volumes"]) == {"postgres_data", "mysql_data"}
        mysql_env = config["services"]["mysql"]["environment"]
        assert mysql_env["MYSQL_PASSWORD"] == "fixture-postgres"
        assert mysql_env["MYSQL_ROOT_PASSWORD"] == "fixture-postgres"

    mysql_url = "mysql://custom:fixture-mysql@mysql:3306/custom_database"
    env_file.write_text(
        database_environment
        + "MYSQL_USER=custom\nMYSQL_DATABASE=custom_database\n"
        + "MYSQL_PASSWORD=fixture-mysql\nMYSQL_ROOT_PASSWORD=fixture-root\n"
        + f"AETHER_DATABASE_DRIVER=mysql\nAETHER_DATABASE_URL={mysql_url}\n"
    )
    for files in standard_compose_files:
        config = checked_config(files, profiles=["mysql"])
        app_env = config["services"]["app"]["environment"]
        assert app_env["AETHER_DATABASE_DRIVER"] == "mysql"
        assert app_env["AETHER_DATABASE_URL"] == mysql_url
        mysql_env = config["services"]["mysql"]["environment"]
        assert mysql_env["MYSQL_USER"] == "custom"
        assert mysql_env["MYSQL_DATABASE"] == "custom_database"
        assert mysql_env["MYSQL_PASSWORD"] == "fixture-mysql"
        assert mysql_env["MYSQL_ROOT_PASSWORD"] == "fixture-root"

    # SQLite deployment must not depend on PostgreSQL, MySQL or Redis credentials.
    env_file.write_text("")
    config = checked_config(["docker-compose.single-node.yml"])
    assert set(config["services"]) == {"app"}
    assert not config.get("volumes")
    app = config["services"]["app"]
    assert_app_security(app, user="65532:65532", capabilities=set())
    app_env = app["environment"]
    assert app_env["AETHER_DATABASE_DRIVER"] == "sqlite"
    assert app_env["AETHER_DATABASE_URL"] == "sqlite:///opt/aether/data/aether.db"
    assert app_env["AETHER_RUNTIME_BACKEND"] == "memory"
    assert app_env["AETHER_GATEWAY_DEPLOYMENT_TOPOLOGY"] == "single-node"
    assert any(volume["target"] == "/opt/aether/data" for volume in app["volumes"])

    config = checked_config(["docker-compose.release-local.yml"])
    assert set(config["services"]) == {"release-local-app"}
    assert set(config["volumes"]) == {"aether_release_local_root"}
    app_env = config["services"]["release-local-app"]["environment"]
    assert app_env["AETHER_DATABASE_DRIVER"] == "sqlite"
    assert app_env["AETHER_DATABASE_URL"] == "sqlite://./data/aether.db"
    assert app_env["AETHER_RUNTIME_BACKEND"] == "memory"

    for log_destination in ("file", "both"):
        env_file.write_text(
            database_environment
            + f"AETHER_LOG_DESTINATION={log_destination}\nAETHER_LOG_DIR=/app/logs\n"
            + "AETHER_CONTAINER_UID=12345\nAETHER_CONTAINER_GID=23456\n"
        )
        for files in standard_compose_files:
            app = checked_config(files)["services"]["app"]
            assert_app_security(app, user="0:0", capabilities={"DAC_OVERRIDE", "FOWNER"})
        app = checked_config(["docker-compose.single-node.yml"])["services"]["app"]
        assert_app_security(app, user="12345:23456", capabilities=set())

print("PASS: PostgreSQL, optional MySQL and SQLite Compose configurations")
