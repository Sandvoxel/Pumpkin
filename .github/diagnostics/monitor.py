#!/usr/bin/env python3
"""Bounded Linux build telemetry; never serialize process arguments or environments."""
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import threading
import time

OUT = Path("diagnostic-output")
OUT.mkdir(exist_ok=True)
CLOCK_TICKS = os.sysconf("SC_CLK_TCK")
PAGE_BYTES = os.sysconf("SC_PAGE_SIZE")


def read(path):
    try:
        return Path(path).read_text().strip()
    except OSError:
        return None


def cgroup_paths():
    membership = read("/proc/self/cgroup") or ""
    mounts = read("/proc/self/mountinfo") or ""
    resolved = set()
    for line in membership.splitlines():
        _, controllers, member = line.split(":", 2)
        for mount in mounts.splitlines():
            left, right = mount.split(" - ", 1)
            fields, fs = left.split(), right.split()
            if not (fs[0] == "cgroup2" and not controllers or
                    fs[0] == "cgroup" and "memory" in controllers.split(",")
                    and "memory" in fs[2].split(",")):
                continue
            root, mountpoint = fields[3], fields[4]
            root = re.sub(r"\\([0-7]{3})", lambda m: chr(int(m[1], 8)), root)
            mountpoint = re.sub(r"\\([0-7]{3})", lambda m: chr(int(m[1], 8)), mountpoint)
            if root != "/" and member != root and not member.startswith(root + "/"):
                continue
            relative = member[len(root):] if root != "/" else member
            path, stop = Path(mountpoint) / relative.lstrip("/"), Path(mountpoint)
            while path == stop or stop in path.parents:
                resolved.add(path)
                if path == stop:
                    break
                path = path.parent
    return sorted(resolved)


CGROUPS = cgroup_paths()
CGROUP_FILES = ("memory.current", "memory.peak", "memory.max", "memory.high",
                "memory.events", "memory.events.local", "memory.swap.current",
                "memory.swap.peak", "memory.swap.max", "cpu.max", "cpu.stat",
                "cpuset.cpus.effective", "memory.usage_in_bytes",
                "memory.max_usage_in_bytes", "memory.limit_in_bytes",
                "memory.failcnt", "memory.oom_control")


def system_sample():
    memory = {}
    for line in (read("/proc/meminfo") or "").splitlines():
        name, value = line.split(":", 1)
        if name in {"MemTotal", "MemAvailable", "MemFree", "SwapTotal", "SwapFree", "Cached", "Dirty"}:
            memory[name + "_kib"] = int(value.split()[0])
    disk = shutil.disk_usage(".")
    return {"memory": memory, "cpu_stat": (read("/proc/stat") or "").splitlines()[:1],
            "pressure_memory": read("/proc/pressure/memory"),
            "disk_free_bytes": disk.free, "disk_total_bytes": disk.total,
            "cgroups": {str(p): {f: read(p / f) for f in CGROUP_FILES} for p in CGROUPS}}


def processes(root_pid):
    rows = {}
    for p in Path("/proc").iterdir():
        if not p.name.isdigit():
            continue
        stat = read(p / "stat")
        if not stat:
            continue
        try:
            end = stat.rindex(")")
            fields = stat[end + 2:].split()
            row = {"pid": int(p.name), "comm": stat[stat.index("(") + 1:end],
                   "ppid": int(fields[1]), "state": fields[0],
                   "cpu_seconds": (int(fields[11]) + int(fields[12])) / CLOCK_TICKS,
                   "start_ticks": int(fields[19]), "rss_bytes": int(fields[21]) * PAGE_BYTES}
            rows[row["pid"]] = row
        except (ValueError, IndexError):
            continue
    selected = {root_pid}
    while True:
        new = {pid for pid, row in rows.items() if row["ppid"] in selected}
        if new <= selected:
            break
        selected |= new
    result = []
    for pid in sorted(selected):
        if pid not in rows:
            continue
        row = rows[pid]
        if row["comm"] in {"rustc", "clippy-driver"}:
            # Read only to extract a validated crate identifier; never emit argv.
            try:
                argv = (Path("/proc") / str(pid) / "cmdline").read_bytes().split(b"\0")
                i = argv.index(b"--crate-name")
                crate = argv[i + 1].decode("ascii")
                if re.fullmatch(r"[A-Za-z0-9_]+", crate):
                    row["crate"] = crate
            except (OSError, ValueError, IndexError, UnicodeError):
                pass
        result.append(row)
    return result


def main():
    started = time.monotonic()
    samples = (OUT / "telemetry.jsonl").open("w", buffering=1)
    peaks = {}
    def emit(kind, payload, live=True):
        record = {"kind": kind, "utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                  "elapsed_seconds": round(time.monotonic() - started, 3), **payload}
        text = json.dumps(record, sort_keys=True)
        samples.write(text + "\n")
        if live:
            print("DIAGNOSTIC " + text, flush=True)
    emit("before", system_sample())
    command = ["/usr/bin/time", "-v", "-o", str(OUT / "time.txt"),
               "cargo", "build", "--verbose", "--release", "--target", "aarch64-unknown-linux-musl"]
    child = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                             start_new_session=True)
    def copy_output():
        with (OUT / "build.log").open("wb") as log:
            for line in iter(child.stdout.readline, b""):
                log.write(line)
                log.flush()
                sys.stdout.buffer.write(line)
                sys.stdout.buffer.flush()
    copier = threading.Thread(target=copy_output, daemon=True)
    copier.start()
    def interrupted(signum, _frame):
        emit("received_signal", {"signal": signum, **system_sample()})
        try:
            os.killpg(child.pid, signum)
        except ProcessLookupError:
            pass
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    seen = set()
    maximum_aggregate = 0
    tick = 0
    while True:
        rows = processes(child.pid)
        current = {(r["pid"], r["start_ticks"]) for r in rows}
        aggregate = sum(r["rss_bytes"] for r in rows)
        maximum_aggregate = max(maximum_aggregate, aggregate)
        for row in rows:
            key = f'{row["pid"]}:{row["start_ticks"]}'
            previous = peaks.get(key)
            peaks[key] = {**row, "peak_rss_bytes": max(row["rss_bytes"], previous["peak_rss_bytes"] if previous else 0),
                          "first_seen_seconds": previous["first_seen_seconds"] if previous else round(time.monotonic() - started, 3),
                          "last_seen_seconds": round(time.monotonic() - started, 3)}
        emit("sample", {"processes": rows, "aggregate_rss_bytes": aggregate,
                        "processes_ended": sorted(seen - current), **system_sample()},
             live=tick % 15 == 0 or current != seen)
        seen = current
        tick += 1
        if child.poll() is not None:
            break
        time.sleep(1)
    copier.join(timeout=10)
    result = {"exit_code": child.returncode, "maximum_sampled_aggregate_rss_bytes": maximum_aggregate,
              "process_peaks": list(peaks.values()), "sample_interval_seconds": 1,
              "elapsed_seconds": round(time.monotonic() - started, 3)}
    (OUT / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    emit("after", {"exit_code": child.returncode, **system_sample()})
    print(read(OUT / "time.txt"), flush=True)
    samples.close()
    return child.returncode if child.returncode >= 0 else 128 - child.returncode


if __name__ == "__main__":
    sys.exit(main())
