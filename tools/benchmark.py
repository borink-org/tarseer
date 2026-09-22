#!/usr/bin/env python3
"""Interleave Linux benchmark commands; retain raw samples and provenance.

Run on the benchmark machine. See tools/README.md for the experiment document.
"""

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time


def read_experiment(path):
    experiment = json.loads(path.read_text())
    if not isinstance(experiment, dict) or experiment.get("version") != 1:
        raise ValueError("experiment must be an object with version 1")
    if set(experiment) - {"version", "passes", "cpus", "timeout_seconds", "cold", "groups"}:
        raise ValueError("unknown experiment field")
    for field in ("passes", "timeout_seconds"):
        if type(experiment.get(field)) is not int or experiment[field] <= 0:
            raise ValueError(f"{field} must be a positive integer")
    if not isinstance(experiment.get("cpus"), str) or not re.fullmatch(r"[0-9,-]+", experiment["cpus"]):
        raise ValueError("cpus must be a taskset CPU list, such as 0-7")
    if type(experiment.get("cold", False)) is not bool:
        raise ValueError("cold must be a boolean")
    groups = experiment.get("groups")
    if not isinstance(groups, list) or not groups:
        raise ValueError("groups must be a nonempty array")
    names = set()
    for group in groups:
        if not isinstance(group, dict) or set(group) != {"name", "cases"}:
            raise ValueError("each group needs name and cases")
        name = group["name"]
        if not isinstance(name, str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", name) or name in names:
            raise ValueError("group names must be distinct filename components")
        names.add(name)
        if not isinstance(group["cases"], list) or not group["cases"]:
            raise ValueError("cases must be a nonempty array")
        labels = set()
        for case in group["cases"]:
            if not isinstance(case, dict) or set(case) - {"label", "command", "expected_entries"}:
                raise ValueError("case fields are label, command and optional expected_entries")
            label = case.get("label")
            if not isinstance(label, str) or not label or label in labels:
                raise ValueError("case labels must be nonempty and distinct within a group")
            labels.add(label)
            command = case.get("command")
            if not isinstance(command, list) or not command or any(not isinstance(value, str) or not value or "\0" in value for value in command):
                raise ValueError("command must be a nonempty argv array")
            if shutil.which(command[0]) is None:
                raise ValueError(f"executable not found: {command[0]}")
            if "expected_entries" in case and (type(case["expected_entries"]) is not int or case["expected_entries"] < 0):
                raise ValueError("expected_entries must be a nonnegative integer")
    return experiment


def steal_ticks():
    return int(Path("/proc/stat").read_text().splitlines()[0].split()[8])


def capture(command):
    result = subprocess.run(command, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, check=False)
    return {"command": command, "exit_code": result.returncode, "output": result.stdout}


def sample(case, experiment, time_program, scratch):
    if experiment.get("cold", False):
        os.sync()
        subprocess.run(["sudo", "-n", "sh", "-c", "echo 3 > /proc/sys/vm/drop_caches"], check=True)
    command = ["timeout", "--kill-after=1s", str(experiment["timeout_seconds"]),
               "taskset", "-c", experiment["cpus"], time_program,
               "-f", "%U %S %M %w %c %F %R %I %O", "-o", str(scratch / "usage"),
               "--", *case["command"]]
    load = os.getloadavg()
    stolen = steal_ticks()
    started = time.perf_counter_ns()
    # A separate process group lets a timeout stop wrappers and their children.
    with (scratch / "stderr").open("w+") as errors:
        process = subprocess.Popen(command, stdout=subprocess.DEVNULL, stderr=errors, start_new_session=True)
        try:
            # wait(timeout=...) polls and quantizes timings of short commands.
            # GNU timeout enforces the deadline while waitpid blocks directly.
            exit_code = process.wait()
        except KeyboardInterrupt:
            import signal
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
            raise
        elapsed = (time.perf_counter_ns() - started) / 1e9
        errors.seek(0)
        stderr = errors.read()
    row = dict(command=case["command"], wall_s=elapsed, exit_code=exit_code,
               steal_ticks=steal_ticks() - stolen, load=load, stderr=stderr)
    if exit_code:
        row["error"] = f"command exited with status {exit_code}"
        return row
    values = (scratch / "usage").read_text().split()
    fields = ["user_s", "sys_s", "peak_kib", "voluntary_switches", "involuntary_switches",
              "major_faults", "minor_faults", "input_blocks", "output_blocks"]
    row.update({key: float(value) if key.endswith("_s") else int(value) for key, value in zip(fields, values, strict=True)})
    if "expected_entries" in case:
        matches = re.findall(r"\b(\d+) entries\b", stderr)
        if len(matches) != 1 or int(matches[0]) != case["expected_entries"]:
            row["error"] = f"expected one report of {case['expected_entries']} entries; got {matches}"
    return row


def run_experiment(experiment, output):
    time_program = shutil.which("time")
    if time_program is None or any(shutil.which(command) is None for command in ("taskset", "timeout", "lscpu", "git")):
        raise ValueError("GNU time, timeout, taskset, lscpu and git must be on PATH")
    if "GNU" not in capture([time_program, "--version"])["output"]:
        raise ValueError("time must be GNU time")
    subprocess.run(["taskset", "-c", experiment["cpus"], "true"], check=True)
    with open("/var/tmp/tarseer-benchmark.lock", "a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise ValueError("another benchmark.py run holds /var/tmp/tarseer-benchmark.lock") from None
        output.mkdir(parents=True, exist_ok=False)
        (output / "experiment.json").write_text(json.dumps(experiment, indent=2) + "\n")
        binaries = {}
        for group in experiment["groups"]:
            for case in group["cases"]:
                binary = Path(shutil.which(case["command"][0])).resolve()
                if str(binary) not in binaries:
                    with binary.open("rb") as stream:
                        binaries[str(binary)] = hashlib.file_digest(stream, "sha256").hexdigest()
        metadata = dict(started_utc=time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                        uname=list(os.uname()), cpus=experiment["cpus"], cwd=str(Path.cwd()),
                        cpuinfo=Path("/proc/cpuinfo").read_text(),
                        memory=Path("/proc/meminfo").read_text(), binaries=binaries,
                        topology=capture(["lscpu"]), revision=capture(["git", "rev-parse", "HEAD"]),
                        dirty=capture(["git", "status", "--short"]))
        (output / "machine.json").write_text(json.dumps(metadata, indent=2) + "\n")
        with tempfile.TemporaryDirectory(prefix="tarseer-benchmark-") as temporary:
            for group in experiment["groups"]:
                rows = []
                cases = group["cases"]
                with (output / (group["name"] + ".jsonl")).open("w") as stream:
                    for pass_number in range(-1, experiment["passes"]):
                        offset = max(pass_number, 0) % len(cases)
                        for case in cases[offset:] + cases[:offset]:
                            row = sample(case, experiment, time_program, Path(temporary))
                            row.update(label=case["label"], pass_number=pass_number,
                                       phase="warmup" if pass_number == -1 else "timed")
                            stream.write(json.dumps(row) + "\n")
                            stream.flush()
                            if "error" in row:
                                raise ValueError(f"{group['name']}/{case['label']}: {row['error']}; see {stream.name}")
                            if pass_number >= 0:
                                rows.append(row)
                summaries = []
                for case in cases:
                    selected = [row for row in rows if row["label"] == case["label"]]
                    summary = dict(label=case["label"], samples=len(selected),
                                   wall_median=statistics.median(row["wall_s"] for row in selected),
                                   wall_min=min(row["wall_s"] for row in selected),
                                   wall_max=max(row["wall_s"] for row in selected),
                                   peak_mib=max(row["peak_kib"] for row in selected) / 1024)
                    summaries.append(summary)
                    print(f"{group['name']}/{case['label']}: {summary['wall_median']:.6f}s, {summary['peak_mib']:.1f} MiB", flush=True)
                (output / (group["name"] + ".summary.json")).write_text(json.dumps(summaries, indent=2) + "\n")
        (output / "complete").touch()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("experiment", type=Path)
    parser.add_argument("output", type=Path, help="new output directory; existing results are never overwritten")
    arguments = parser.parse_args()
    try:
        run_experiment(read_experiment(arguments.experiment), arguments.output)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"benchmark: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
