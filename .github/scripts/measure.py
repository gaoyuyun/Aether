"""Run a CI command with wall time and sampled process-tree RSS in the summary."""

import os
from pathlib import Path
import shlex
import subprocess
import sys
import time


def build_rss(root_pid: int) -> int:
    rows = subprocess.check_output(
        ["ps", "-eo", "pid=,ppid=,rss=,comm="], text=True,
    ).splitlines()
    processes = [row.split(maxsplit=3) for row in rows]
    # sccache workers and cross's containerized Cargo can live outside the
    # measured command's process tree. Runners execute one build job at a time.
    selected = {root_pid}
    selected.update(int(pid) for pid, _, _, command in processes if command in {"cargo", "sccache"})
    while True:
        descendants = {int(pid) for pid, parent, _, _ in processes if int(parent) in selected}
        expanded = selected | descendants
        if expanded == selected:
            break
        selected = expanded
    return sum(int(rss) for pid, _, rss, _ in processes if int(pid) in selected)


def main() -> int:
    command = sys.argv[1:]
    if not command:
        raise SystemExit("usage: measure.py COMMAND [ARG ...]")
    started = time.monotonic()
    peak_kib = 0
    with subprocess.Popen(command) as process:
        while process.poll() is None:
            try:
                peak_kib = max(peak_kib, build_rss(process.pid))
            except (OSError, subprocess.CalledProcessError, ValueError):
                # Missing telemetry must not interrupt or mask the build result.
                pass
            try:
                process.wait(timeout=1)
            except subprocess.TimeoutExpired:
                pass
    elapsed = time.monotonic() - started
    report = (
        f"Command: `{shlex.join(command)}`\n\n"
        f"Wall time: **{elapsed:.1f}s**; sampled peak combined RSS: "
        f"**{peak_kib / 1024:.0f} MiB**; exit status: **{process.returncode}**.\n\n"
    )
    print(report, flush=True)
    if summary := os.environ.get("GITHUB_STEP_SUMMARY"):
        try:
            with Path(summary).open("a") as output:
                output.write(report)
        except OSError as error:
            print(f"Cannot write CI timing summary: {error}", file=sys.stderr)
    return process.returncode


if __name__ == "__main__":
    sys.exit(main())
