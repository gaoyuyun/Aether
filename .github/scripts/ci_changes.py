"""Select CI jobs conservatively, including source files read by Rust tests."""

import json
import os
from pathlib import Path
import re
import subprocess


def select_jobs(paths: list[str] | None) -> dict[str, bool]:
    # Dispatches, reusable workflows, new branches, and unavailable diffs get
    # full coverage. Unknown paths also select both jobs.
    jobs = {"rust": False, "frontend": False}
    if paths is None:
        return dict.fromkeys(jobs, True)

    for path in paths:
        if path.startswith(("apps/", "crates/", ".cargo/")) or path in {
            "Cargo.toml", "Cargo.lock", "rust-toolchain", "rust-toolchain.toml",
        }:
            jobs["rust"] = True
        elif path.startswith("frontend/"):
            jobs["frontend"] = True
            # Gateway architecture tests scan frontend sources for retired API
            # aliases and check the Vite build-version contract.
            if path.startswith("frontend/src/") or path == "frontend/vite.config.ts":
                jobs["rust"] = True
        elif path.startswith("aether-vscodex/"):
            jobs["frontend"] = True
        elif path.startswith("docs/api/"):
            # aether-ai-formats includes API contract documents in its tests.
            jobs["rust"] = True
        elif path == "README.md" or (path.startswith("docs/") and path.endswith(".md")):
            continue
        else:
            return dict.fromkeys(jobs, True)
    return jobs


def base_revision(event_name: str, event: dict) -> str:
    if event_name == "push":
        return event.get("before", "")
    if event_name == "pull_request":
        # Checkout supplies the tested merge commit. Diff it against its base,
        # so unrelated changes already on the base branch are not included.
        return event.get("pull_request", {}).get("base", {}).get("sha", "")
    return ""


def changed_paths(event_name: str, event: dict) -> list[str] | None:
    if event_name not in {"push", "pull_request"}:
        return None

    base = base_revision(event_name, event)
    if not re.fullmatch(r"[0-9a-f]{40}", base) or base == "0" * 40:
        return None

    try:
        available = subprocess.run(
            ["git", "cat-file", "-e", f"{base}^{{commit}}"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        if available.returncode:
            subprocess.run(
                ["git", "fetch", "--no-tags", "--depth=1", "origin", base],
                check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
            )
        diff = subprocess.check_output([
            "git", "diff", "--name-only", "--no-renames", "-z", base, "HEAD", "--",
        ])
    except (OSError, subprocess.CalledProcessError) as error:
        print(f"Cannot determine changed files; running all jobs: {error}")
        return None
    # Disable rename detection so a move out of a component still checks it.
    return [os.fsdecode(path) for path in diff.split(b"\0") if path]


def has_successful_baseline(event_name: str, event: dict) -> bool:
    # A newer push can cancel an unfinished backend run. Comparing only its
    # files would otherwise let a docs/asset push turn untested code green.
    try:
        repository = os.environ["GITHUB_REPOSITORY"]
        response = subprocess.check_output([
            "gh", "api", "--method", "GET",
            f"repos/{repository}/actions/workflows/ci.yml/runs",
            "-f", f"head_sha={base_revision(event_name, event)}",
            "-f", "status=success", "-f", "per_page=1",
        ], stderr=subprocess.PIPE, text=True)
        return json.loads(response).get("total_count", 0) > 0
    except (OSError, KeyError, ValueError, subprocess.CalledProcessError):
        return False


def main() -> None:
    try:
        if os.environ.get("CI_FORCE_FULL") == "true":
            paths = None
        else:
            event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text())
            paths = changed_paths(os.environ["GITHUB_EVENT_NAME"], event)
    except (OSError, KeyError, ValueError) as error:
        print(f"Cannot read CI event; running all jobs: {error}")
        paths = None

    jobs = select_jobs(paths)
    if paths is not None and not all(jobs.values()):
        if not has_successful_baseline(os.environ["GITHUB_EVENT_NAME"], event):
            print("No successful CI baseline; running all jobs.")
            jobs = select_jobs(None)
    output = "".join(f"{name}={str(enabled).lower()}\n" for name, enabled in jobs.items())
    print(output, end="")
    with Path(os.environ["GITHUB_OUTPUT"]).open("a") as destination:
        destination.write(output)


if __name__ == "__main__":
    main()
