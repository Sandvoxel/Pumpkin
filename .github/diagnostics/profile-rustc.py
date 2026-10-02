#!/usr/bin/env python3
"""Diagnostic-only phase capture; never serialize inherited environments or argv."""
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time
import threading

ROOT = Path.cwd() / 'diagnostic-output'
DEEP = {'pumpkin', 'pumpkin_wasm_host_v0_1', 'pumpkin_wasm_host_v0_2'}


def flags(directory, deep):
    result = ['-Ztime-passes', '-Ztime-passes-format=json']
    if deep:
        (directory / 'mono').mkdir(parents=True, exist_ok=True)
        result += [f'-Zself-profile={directory}', '-Zself-profile-events=default,query-keys,llvm',
                   '-Ztime-llvm-passes', f'-Zdump-mono-stats={directory / "mono"}',
                   '-Zdump-mono-stats-format=json']
    return result


def probe():
    out = ROOT / 'compiler-probe'
    out.mkdir(parents=True, exist_ok=True)
    source = out / 'probe.rs'
    source.write_text('#[inline(never)] fn work(x: u64) -> u64 { (0..x).sum() }\n'
                      'fn main() { std::hint::black_box(work(10000)); }\n')
    env = dict(os.environ, RUSTC_BOOTSTRAP='profile_probe')
    args = ['rustc', str(source), '--crate-name', 'profile_probe', '-Copt-level=3',
            '-Clto=thin', '-Ccodegen-units=1', '--target', 'aarch64-unknown-linux-musl',
            '--out-dir', str(out), *flags(out, True)]
    with (out / 'compiler.log').open('w') as log:
        result = subprocess.run(args, env=env, stdout=log, stderr=log, timeout=90)
    phases = [json.loads(line[6:]) for line in (out / 'compiler.log').read_text().splitlines()
              if line.startswith('time: {')]
    traces = list(out.glob('*.mm_profdata'))
    assert result.returncode == 0 and len(traces) == 1 and traces[0].stat().st_size > 1000
    assert any(p['pass'] == 'run_linker' for p in phases)
    assert any(p['pass'] == 'type_check_crate' for p in phases)
    (out / 'validation.json').write_text(json.dumps({'exit_code': result.returncode,
        'trace_bytes': traces[0].stat().st_size, 'phase_count': len(phases),
        'flags': flags(out, True)}, indent=2) + '\n')
    (ROOT / 'profiles').mkdir(exist_ok=True)
    print('COMPILER_PROFILE_PROBE_OK: phase JSON, LLVM report, raw self-profile and native link completed')


def main():
    if sys.argv[1:] == ['--probe']:
        probe()
        return 0
    rustc, *args = sys.argv[1:]
    if '--crate-name' not in args:
        os.execv(rustc, [rustc, *args])
    crate = args[args.index('--crate-name') + 1]
    if not re.fullmatch(r'[A-Za-z0-9_]+', crate):
        raise SystemExit('Invalid crate name')
    phase_dir = ROOT / 'phases'
    phase_dir.mkdir(parents=True, exist_ok=True)
    unit = f'{crate}-{os.getpid()}'
    deep = crate in DEEP
    directory = ROOT / 'profiles' / unit
    if deep:
        directory.mkdir(parents=True, exist_ok=True)
    extra = flags(directory, deep)
    env = dict(os.environ, RUSTC_BOOTSTRAP=crate)
    start = time.time_ns()
    monotonic = time.monotonic()
    child = subprocess.Popen([rustc, *args, *extra], env=env, stderr=subprocess.PIPE,
                             stdout=subprocess.PIPE if deep else None, close_fds=False)
    def copy_stdout():
        with (directory / 'llvm-stdout.log').open('wb') as log:
            for line in iter(child.stdout.readline, b''):
                log.write(line)
                log.flush()
                sys.stdout.buffer.write(line)
                sys.stdout.buffer.flush()
    copier = threading.Thread(target=copy_stdout) if deep else None
    if copier:
        copier.start()
    identity = {'crate': crate, 'wrapper_pid': os.getpid(), 'rustc_pid': child.pid,
                'start_unix_ns': start, 'deep_profile': deep, 'diagnostic_flags': extra}
    (phase_dir / f'{unit}.json').write_text(json.dumps(identity) + '\n')
    with (phase_dir / f'{unit}.log').open('wb') as log, \
            (phase_dir / f'{unit}.jsonl').open('w', buffering=1) as phases:
        for line in iter(child.stderr.readline, b''):
            log.write(line)
            log.flush()
            if line.startswith(b'time: {'):
                record = json.loads(line[6:])
                record.update(observed_unix_ns=time.time_ns(), rustc_pid=child.pid, crate=crate)
                phases.write(json.dumps(record) + '\n')
            else:
                sys.stderr.buffer.write(line)
                sys.stderr.buffer.flush()
    code = child.wait()
    if copier:
        copier.join()
    result = {**identity, 'end_unix_ns': time.time_ns(),
              'elapsed_seconds': time.monotonic() - monotonic, 'exit_code': code}
    (phase_dir / f'{unit}-result.json').write_text(json.dumps(result) + '\n')
    if deep:
        (directory / 'compiler.log').write_bytes((phase_dir / f'{unit}.log').read_bytes())
        (directory / 'result.json').write_text(json.dumps(result) + '\n')
    return code if code >= 0 else 128 - code


if __name__ == '__main__':
    sys.exit(main())
