#!/usr/bin/env python3
"""Measure release ldu on one synthetic, warm-cache corpus under host-quiet."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import shutil
import statistics
import subprocess
import sys
import tempfile
import time


def positive_list(value):
    result = [int(item) for item in value.split(",")]
    if not result or any(item < 1 for item in result):
        raise argparse.ArgumentTypeError("expected comma-separated positive integers")
    return result


def local_executable(binary):
    value = str(binary)
    return "./" + value if not binary.is_absolute() and len(binary.parts) == 1 else value


def source_head():
    result = subprocess.run(["git", "rev-parse", "--verify", "HEAD"], text=True,
                            capture_output=True)
    if result.returncode != 0:
        return "uncommitted"
    return result.stdout.strip() or "uncommitted"


def create_corpus(root, files):
    flat = root / "flat"
    flat.mkdir()
    (flat / "navigation").mkdir()
    (flat / "navigation" / "leaf").write_bytes(b"navigation")
    wide = root / "wide"
    wide.mkdir()
    directories = []
    for n in range(128):
        directory = wide / f"d{n:04d}"
        directory.mkdir()
        directories.append(directory)
    for n in range(files):
        directory = flat if n % 2 else directories[(n // 2) % len(directories)]
        (directory / f"f{n:08d}").write_bytes(b"x" * (n % 9 * 128))
    deep = root / "deep"
    deep.mkdir()
    for n in range(96):
        deep = deep / "d"
        deep.mkdir()
        (deep / f"leaf{n:03d}").write_bytes(b"y" * 256)
    sparse = root / "sparse"
    with sparse.open("wb") as output:
        output.truncate(64 * 1024 * 1024)
    os.link(sparse, directories[0] / "sparse-alias")
    os.symlink("..", wide / "parent-link")


def run_json(command):
    begin = time.perf_counter()
    result = subprocess.run(command, text=True, capture_output=True, check=True)
    elapsed = time.perf_counter() - begin
    return elapsed, json.loads(result.stdout)


def reference_total(root, apparent):
    command = ["du", "--block-size=1", "--summarize"]
    if apparent:
        command.append("--apparent-size")
    command += ["--", str(root)]
    result = subprocess.run(command, text=True, capture_output=True, check=True)
    total = int(result.stdout.split()[0])
    if apparent:
        # GNU du omits directory metadata lengths in apparent-size mode.
        # LDU's export includes each entry's own length, including directories.
        def failed(error):
            raise error

        total += sum(Path(folder).lstat().st_size for folder, _, _ in
                     os.walk(root, onerror=failed, followlinks=False))
    return total


def binary_identity(label, binary):
    digest = hashlib.sha256()
    size = 0
    with binary.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
            size += len(block)
    return {"variant": label, "path": str(binary), "sha256": digest.hexdigest(), "bytes": size}


def reference_entries(root):
    # os.walk does not descend through symlinks, but counts each directory entry.
    def failed(error):
        raise error

    return sum(len(directories) + len(files)
               for _, directories, files in os.walk(root, onerror=failed, followlinks=False))


def validate_output(output, expected, entries):
    assert isinstance(output, list) and len(output) == 4 and output[:2] == [1, 2], (
        "Expected ncdu-compatible -o output; both benchmark binaries must support this format")
    assert output[2]["progname"] == "ldu", output[2]
    root = output[3]
    assert isinstance(root, list) and root and isinstance(root[0], dict), "Missing export root"
    totals = {"apparent": 0, "allocated": 0}
    count = 0
    pending = [iter([root])]
    while pending:
        item = next(pending[-1], None)
        if item is None:
            pending.pop()
            continue
        if isinstance(item, list):
            assert item and isinstance(item[0], dict), "Invalid exported directory"
            record = item[0]
            pending.append(iter(item[1:]))
        else:
            record = item
        assert isinstance(record, dict) and isinstance(record.get("name"), str), record
        assert not record.get("read_error") and not record.get("excluded"), record
        for field, total in [("asize", "apparent"), ("dsize", "allocated")]:
            value = record.get(field, 0)
            assert type(value) is int and value >= 0, record
            totals[total] += value
        count += 1
    assert totals == expected, (expected, totals)
    assert count - 1 == entries, (entries, count - 1)
    assert sorted(item[0]["name"] for item in root[1:] if isinstance(item, list)) == [
        "deep", "flat", "wide"]


def measure(args):
    if os.environ.get("LDU_BENCH_GUARDED") != "1":
        raise SystemExit("internal measurement mode requires the host-quiet/guarded-run launcher")
    root = args.measure
    expected = {"apparent": reference_total(root, True), "allocated": reference_total(root, False)}
    entries = reference_entries(root)
    variants = [("current", args.binary)]
    if args.baseline_binary is not None:
        variants.insert(0, ("baseline", args.baseline_binary))
    binaries = [binary_identity(label, binary) for label, binary in variants]
    reports = []
    backends = args.backends.split(",")
    for backend in backends:
        for threads in args.threads:
            # The standard-library backend has no configurable readdir buffer.
            buffers = args.buffers if backend != "std" else [262144]
            for buffer_size in buffers:
                commands = {
                    label: [local_executable(binary), "-o", "-", "--backend", backend,
                            "--threads", str(threads), "--buffer-size", str(buffer_size), str(root)]
                    for label, binary in variants
                }
                samples = {label: [] for label, _ in variants}
                # Warm each executable on the same corpus before collecting this case.
                for command in commands.values():
                    _, initial = run_json(command)
                    validate_output(initial, expected, entries)
                for repetition in range(args.repeats):
                    # Pair samples closely and alternate which executable runs first.
                    order = variants if repetition % 2 == 0 else list(reversed(variants))
                    for label, _ in order:
                        elapsed, output = run_json(commands[label])
                        validate_output(output, expected, entries)
                        samples[label].append(elapsed)
                for label, _ in variants:
                    reports.append({
                        "variant": label,
                        "backend": backend, "threads": threads, "buffer_bytes": buffer_size,
                        "wall_median_seconds": statistics.median(samples[label]),
                        "wall_samples_seconds": samples[label],
                    })
    references = []
    for name, command in (
        ("du allocated", ["du", "--block-size=1", "--summarize", "--", str(root)]),
        ("ncdu export", ["ncdu", "-0", "-o", str(root.parent / "ncdu-export.json"), str(root)]),
    ):
        if name.startswith("ncdu") and not args.ncdu:
            continue
        if shutil.which(command[0]) is None:
            continue
        samples = []
        for _ in range(args.repeats):
            begin = time.perf_counter()
            subprocess.run(command, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, check=True)
            samples.append(time.perf_counter() - begin)
        references.append({"tool": name, "wall_median_seconds": statistics.median(samples)})
    latencies = None
    if args.pty:
        command = ["python3", "scripts/check_pty.py", "--binary", str(args.binary),
                   "--root", str(root / "flat"), "--report-latencies",
                   "--max-input-ms", str(args.max_input_ms)]
        result = subprocess.run(command, text=True, capture_output=True, check=True)
        latencies = json.loads(result.stdout)
    assert binaries == [binary_identity(label, binary) for label, binary in variants], (
        "Executable changed during measurement; discard all samples")
    print(json.dumps({
        "qualification": "Synthetic corpus; warm metadata cache; samples accepted only when guarded-run exits zero.",
        "platform": platform.platform(), "logical_cpus": os.cpu_count(),
        "source_head": source_head(), "binaries": binaries,
        "timing": "Process wall time, including ncdu-compatible JSON export",
        "sample_order": "baseline/current then current/baseline on alternating repetitions"
                        if args.baseline_binary is not None else "current only",
        "primary_files": args.files, "flat_rows": args.files // 2 + 1, "entries": entries,
        "apparent": expected["apparent"], "allocated": expected["allocated"],
        "repeats": args.repeats, "cases": reports, "references": references,
        "interactive_latencies": latencies,
    }, indent=2))


def main():
    if sys.flags.optimize:
        raise SystemExit("Benchmark qualification requires assertions; rerun without -O or PYTHONOPTIMIZE")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/release/ldu"))
    parser.add_argument("--baseline-binary", type=Path,
                        help="interleave baseline and current samples on the same corpus")
    parser.add_argument("--quiet-timeout", type=float, default=45,
                        help="maximum seconds waiting for a quiet host (default: 45)")
    parser.add_argument("--files", type=int, default=25000)
    parser.add_argument("--repeats", type=int, default=5)
    parser.add_argument("--threads", type=positive_list, default=[1, 4, 16])
    parser.add_argument("--buffers", type=positive_list, default=[65536, 262144, 1048576])
    parser.add_argument("--backends", default="std,native")
    parser.add_argument("--ncdu", action="store_true", help="include installed ncdu export wall time")
    parser.add_argument("--pty", action="store_true", help="include key response, first-frame and quit latency samples")
    parser.add_argument("--max-input-ms", type=float, default=1000, help="maximum accepted PTY key response")
    parser.add_argument("--measure", type=Path, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if not math.isfinite(args.max_input_ms) or args.max_input_ms <= 0:
        parser.error("max-input-ms must be a finite positive number")
    if not math.isfinite(args.quiet_timeout) or args.quiet_timeout <= 0:
        parser.error("quiet-timeout must be a finite positive number")
    for binary in (args.binary, args.baseline_binary):
        if binary is not None and not binary.is_file():
            parser.error(f"release binary does not exist: {binary}")
    if args.repeats < 1 or args.files < 1:
        parser.error("files and repeats must be positive")
    if any(n > 128 for n in args.threads) or any(not 4096 <= n <= 4194304 for n in args.buffers):
        parser.error("threads must be <=128 and buffers must be 4096..4194304")
    if any(b not in ("auto", "native", "std") for b in args.backends.split(",")):
        parser.error("backends must be comma-separated auto,native,std")
    if args.measure:
        measure(args)
        return
    for tool in ("host-quiet", "guarded-run", "du"):
        if shutil.which(tool) is None:
            parser.error(f"{tool} is required; timings are never collected without the shared-host gate")
    Path(".tmp").mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="ldu-bench-", dir=".tmp") as work:
        root = Path(work) / "corpus"
        root.mkdir()
        create_corpus(root, args.files)
        print("Waiting for host-quiet before collecting timings...", file=sys.stderr, flush=True)
        try:
            quiet = subprocess.run(["host-quiet", "--wait"], stdout=sys.stderr,
                                   timeout=args.quiet_timeout)
        except subprocess.TimeoutExpired:
            print("No timings collected: the host did not become quiet before the timeout.",
                  file=sys.stderr)
            raise SystemExit(75)
        if quiet.returncode:
            raise SystemExit(quiet.returncode)
        child = ["python3", os.path.relpath(__file__), "--measure", str(root),
                 "--binary", str(args.binary), "--files", str(args.files),
                 "--repeats", str(args.repeats), "--threads", ",".join(map(str, args.threads)),
                 "--buffers", ",".join(map(str, args.buffers)), "--backends", args.backends,
                 "--max-input-ms", str(args.max_input_ms)]
        if args.baseline_binary is not None:
            child += ["--baseline-binary", str(args.baseline_binary)]
        if args.ncdu:
            child.append("--ncdu")
        if args.pty:
            child.append("--pty")
        # Every measured descendant includes this unique corpus path; foreign work is never signalled.
        guarded = subprocess.run(
            ["guarded-run", "--no-wait", "--abort-on-busy", "--ignore", str(root), "--"] + child,
            text=True, capture_output=True, env={**os.environ, "LDU_BENCH_GUARDED": "1"},
        )
        if guarded.returncode:
            print(guarded.stderr, file=sys.stderr, end="")
            if guarded.returncode == 75:
                print("Timings discarded: shared host became busy (exit 75).", file=sys.stderr)
            raise SystemExit(guarded.returncode)
        print(guarded.stdout, end="")


if __name__ == "__main__":
    main()
