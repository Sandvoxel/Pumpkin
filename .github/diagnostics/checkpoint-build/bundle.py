#!/usr/bin/env python3
"""Archive only allowlisted diagnostic data; never credentials, caches or binaries."""
import gzip
import re
import io
import json
from pathlib import Path
import sys
import tarfile

out = Path("diagnostic-output")
kind, destination, offset_string = sys.argv[1:]
offset = int(offset_string)
identity = ["source-and-machine.txt", "rustc.txt", "cargo.txt", "active-toolchain.txt",
            "installed-targets.txt", "packages.txt", "checkpoint-state.json"]
members = {}
for name in identity:
    p = out / name
    if p.is_file():
        members[name] = p.read_bytes()
if kind.startswith("profile:"):
    name = kind.split(":", 1)[1]
    if not re.fullmatch(r"(pumpkin|pumpkin_wasm_host_v0_[12])-\d+", name):
        raise SystemExit("Invalid profile name")
    files = sorted((out / "profiles" / name).rglob("*"))
    files = [p for p in files if p.is_file() and p.suffix in {".json", ".mm_profdata", ".log"}]
    if sum(p.stat().st_size for p in files) > 8 * 1024**3:
        raise SystemExit("Raw profile exceeds 8 GiB budget")
    with tarfile.open(destination, "w:gz", compresslevel=3) as tar:
        for p in files:
            tar.add(p, arcname=str(p.relative_to(out)), recursive=False)
    size = Path(destination).stat().st_size
    if size > 1024**3:
        raise SystemExit("Compressed profile exceeds 1 GiB budget")
    print(json.dumps({"telemetry_end": offset, "archive_bytes": size, "profile": name}))
    raise SystemExit(0)
if kind == "preflight":
    for p in sorted((out / "probe").glob("*")):
        if p.is_file() and p.suffix in {".txt", ".json", ".jsonl", ".log"}:
            members["probe/" + p.name] = p.read_bytes()
    for p in sorted((out / "compiler-probe").rglob("*")):
        if p.is_file() and p.suffix in {".json", ".mm_profdata", ".log"}:
            members[str(p.relative_to(out))] = p.read_bytes()
    end = 0
else:
    telemetry = (out / "telemetry.jsonl").read_bytes()
    # Exclude a possibly unfinished line while the sampler is writing.
    end = telemetry.rfind(b"\n") + 1
    members["telemetry.jsonl"] = telemetry[offset:end] if kind == "periodic" else telemetry[:end]
    members["telemetry-range.json"] = json.dumps({"byte_start": offset if kind == "periodic" else 0,
                                                "byte_end": end}).encode()
    for name in ["snapshot.json", "result.json", "time.txt", "final-footprint.txt"]:
        p = out / name
        if p.is_file():
            members[name] = p.read_bytes()
    for p in sorted((out / "phases").glob("*")):
        if p.is_file() and p.suffix in {".json", ".jsonl", ".log"}:
            members["phases/" + p.name] = p.read_bytes()
    if kind == "final":
        for p in sorted(Path("target/cargo-timings").glob("*.html")):
            members["cargo-timings/" + p.name] = p.read_bytes()
        members["build.log"] = (out / "build.log").read_bytes()
with open(destination, "wb") as raw:
    with gzip.GzipFile(filename="", fileobj=raw, mode="wb", compresslevel=6, mtime=0) as gz:
        with tarfile.open(fileobj=gz, mode="w|") as tar:
            for name, data in sorted(members.items()):
                info = tarfile.TarInfo(name)
                info.size = len(data)
                info.mode = 0o644
                tar.addfile(info, io.BytesIO(data))
maximum = 32 * 1024 * 1024
size = Path(destination).stat().st_size
if size > maximum:
    raise SystemExit(f"Diagnostic archive {size} bytes exceeds {maximum}-byte limit")
print(json.dumps({"telemetry_end": end, "archive_bytes": size}))
