#!/usr/bin/env python3
"""Prepare and smoke-test Linux release binaries (Python 3.11+, Git, Rust 1.99.0)."""

import argparse
import datetime
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import struct
import subprocess
import tempfile
import tomllib
import zipfile

TARGETS = {
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "armv7-unknown-linux-musleabihf",
    "riscv64gc-unknown-linux-gnu",
}
TOOLCHAIN = "1.99.0"
ROOT = Path(__file__).resolve().parent.parent


def run(command, timeout=120):
    result = subprocess.run(
        [str(part) for part in command], cwd=ROOT, check=False, timeout=timeout,
        stdout=subprocess.PIPE, text=True, encoding="utf-8",
    )
    if result.returncode:
        print(result.stdout, flush=True)
        result.check_returncode()
    return result.stdout.strip()


def digest(data):
    return hashlib.sha256(data).hexdigest()


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def binary_for(target):
    return ROOT / "target" / target / "release" / "ldu"


def check_architecture(data, target):
    expected = {"x86_64": 62, "aarch64": 183, "armv7": 40, "riscv64gc": 243}
    require(len(data) >= 20 and data[:4] == b"\x7fELF" and data[5] == 1,
            "Expected little-endian ELF")
    actual = struct.unpack_from("<H", data, 18)[0]
    require(actual == expected[target.split("-")[0]],
            f"Wrong executable architecture for {target}")


def totals(paths):
    stats = [path.stat() for path in paths]
    return {
        "apparent": sum(item.st_size for item in stats),
        "allocated": sum(
            item.st_blocks * 512 if hasattr(item, "st_blocks") else item.st_size
            for item in stats
        ),
    }


def smoke(args, version, binary):
    data = binary.read_bytes()
    check_architecture(data, args.target)
    runner = json.loads(args.runner_json)
    require(isinstance(runner, list) and all(isinstance(arg, str) for arg in runner),
            "--runner-json must be an array of command arguments")
    command = runner + [str(binary)]
    require(run(command + ["--version"]) == f"ldu {version}", "Wrong binary version")
    help_text = run(command + ["--help"])
    require(all(text in help_text for text in ["Usage:", "--json", "--backend"]),
            "Incomplete --help output")
    (ROOT / ".tmp").mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="release-smoke-", dir=ROOT / ".tmp") as work:
        root = Path(work).resolve()
        nested = root / "nested"
        empty = nested / "empty"
        empty.mkdir(parents=True)
        (root / "top.txt").write_bytes(b"release fixture\n")
        (nested / "payload.bin").write_bytes(bytes(range(256)) * 17)
        (root / "zero").touch()
        paths = [root, *root.rglob("*")]
        expected = totals(paths)
        nested_expected = totals([nested, *nested.rglob("*")])
        backends = ["auto", "native", "std"]
        for backend in backends:
            scan_command = command + [
                "--json", "--backend", backend, "--threads", "2", str(root),
            ]
            try:
                report = json.loads(run(scan_command))
            except subprocess.CalledProcessError:
                if runner and Path(runner[0]).name.startswith("qemu-"):
                    trace = subprocess.run(
                        [runner[0], "-strace", *scan_command[1:]], cwd=ROOT,
                        stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                        text=True, encoding="utf-8", errors="replace", timeout=30,
                    )
                    print(trace.stdout[-30000:], flush=True)
                raise
            require(report["root"] == str(root), f"{backend}: wrong root")
            require(report["entries"] == len(paths) - 1, f"{backend}: wrong entry count")
            require(report["errors"] == 0 and report["cancelled"] is False,
                    f"{backend}: scan failed")
            require(all(report[key] == value for key, value in expected.items()),
                    f"{backend}: wrong root totals: {report}")
            require(math.isfinite(report["seconds"]) and report["seconds"] >= 0,
                    f"{backend}: invalid elapsed time")
            require(len(report["directories"]) == 1, f"{backend}: wrong directories")
            directory = report["directories"][0]
            require(directory["name"] == "nested" and directory["path"] == str(nested),
                    f"{backend}: wrong directory identity")
            require(directory["complete"] is True and directory["errors"] == 0,
                    f"{backend}: incomplete directory")
            require(all(directory[key] == value for key, value in nested_expected.items()),
                    f"{backend}: wrong directory totals")
    linkage = {}
    if "linux" in args.target:
        headers = run(["readelf", "--program-headers", binary])
        dynamic = run(["readelf", "--dynamic", binary])
        if "musl" in args.target:
            require("INTERP" not in headers and "(NEEDED)" not in dynamic,
                    "musl release must be statically linked")
            linkage["static"] = True
        else:
            versions = run(["readelf", "--version-info", binary])
            glibc = set(re.findall(r"Name: GLIBC_([0-9.]+)", versions))
            require(glibc, "No GLIBC version requirements found")
            linkage["needed_libraries"] = re.findall(r"\(NEEDED\).*?\[(.*?)\]", dynamic)
            linkage["glibc_symbol_floor"] = max(
                glibc, key=lambda version: tuple(map(int, version.split(".")))
            )
    result = {
        "version": version, "target": args.target, "binary_sha256": digest(data),
        "runner": runner, "host_system": platform.system(), "host_machine": platform.machine(),
        "checks": ["architecture", "version", "help", *[f"json-{backend}" for backend in backends]],
        "linkage": linkage,
    }
    report_path = ROOT / ".tmp" / f"release-smoke-{args.target}.json"
    report_path.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print("SMOKE_METADATA " + json.dumps(result, sort_keys=True), flush=True)
    print(f"Smoke passed: {args.target} ({' '.join(runner) if runner else 'native'})")


def dependency_licenses(target):
    metadata = json.loads(run([
        "cargo", f"+{TOOLCHAIN}", "metadata", "--locked", "--format-version=1",
        "--filter-platform", target,
    ], timeout=300))
    files = {}
    notices = ["# Dependency licenses", "", "License files distributed with these packages:", ""]
    for package in sorted(metadata["packages"], key=lambda item: (item["name"], item["version"])):
        if package["name"] == "ldu":
            continue
        identifier = f"{package['name']}-{package['version']}"
        require(re.fullmatch(r"[A-Za-z0-9_.+-]+", identifier), "Invalid package identifier")
        base = Path(package["manifest_path"]).parent
        candidates = set()
        for path in base.iterdir():
            if path.name.upper().startswith(("LICENSE", "LICENCE", "COPYING", "NOTICE", "COPYRIGHT")):
                if path.is_file():
                    candidates.add(path)
                elif path.is_dir():
                    candidates.update(child for child in path.rglob("*") if child.is_file())
        if package.get("license_file"):
            declared = Path(package["license_file"])
            candidates.add(declared if declared.is_absolute() else base / declared)
        require(candidates, f"No license text found for {identifier}")
        source = package.get("source") or "workspace"
        if source.startswith("registry+"):
            source = f"https://crates.io/api/v1/crates/{package['name']}/{package['version']}/download"
        elif source == "workspace":
            source = "https://github.com/SmallThingz/ldu/tree/" + run(["git", "rev-parse", "HEAD"]) + "/vendor"
        notices.extend([f"## {identifier}", "", f"License: {package.get('license') or 'see files'}",
                        f"Source: {source}", ""])
        for path in sorted(candidates):
            require(path.is_file(), f"Missing declared license: {identifier}/{path.name}")
            relative = path.relative_to(base).as_posix()
            name = f"licenses/{identifier}/{relative}"
            files[name] = path.read_bytes()
            notices.append(f"- [{relative}]({identifier}/{relative})")
        notices.append("")
    files["licenses/README.md"] = ("\n".join(notices) + "\n").encode()
    return files


def runtime_licenses():
    sysroot = Path(run(["rustc", f"+{TOOLCHAIN}", "--print", "sysroot"]))
    base = sysroot / "share" / "doc" / "rust"
    copyright_file = base / "COPYRIGHT-library.html"
    require(copyright_file.is_file() and (base / "licenses").is_dir(),
            "Pinned Rust toolchain is missing its library license notices")
    files = {"licenses/rust/COPYRIGHT-library.html": copyright_file.read_bytes()}
    for path in sorted((base / "licenses").rglob("*")):
        if path.is_file():
            files["licenses/rust/" + path.relative_to(base).as_posix()] = path.read_bytes()
    bundled = ROOT / "licenses" / "runtime"
    sources = json.loads((bundled / "sources.json").read_text(encoding="utf-8"))
    for name, source in sources.items():
        require(Path(name).name == name and digest((bundled / name).read_bytes()) == source["sha256"],
                f"Runtime notice checksum mismatch: {name}")
    for path in sorted(bundled.iterdir()):
        if path.is_file():
            files["licenses/runtime/" + path.name] = path.read_bytes()
    return files


def package(args, version, binary):
    data = binary.read_bytes()
    check_architecture(data, args.target)
    commit = run(["git", "rev-parse", "HEAD"])
    epoch = int(run(["git", "show", "-s", "--format=%ct", "HEAD"]))
    notices = dependency_licenses(args.target)
    notices.update(runtime_licenses())
    notices["LICENSE"] = (ROOT / "LICENSE").read_bytes()
    output = Path(args.output).resolve()
    output.mkdir(parents=True, exist_ok=True)
    released = output / f"ldu-v{version}-{args.target}"
    require(released != binary, "Release destination must differ from the build output")
    released.write_bytes(data)
    # ELF loaders ignore trailing data. Keep required license texts compressed in
    # the executable itself so downloads consist solely of runnable binaries.
    # ZIP readers can inspect these notices without running the program.
    timestamp = datetime.datetime.fromtimestamp(max(epoch, 315532800), datetime.timezone.utc)
    stamp = (timestamp.year, timestamp.month, timestamp.day,
             timestamp.hour, timestamp.minute, timestamp.second // 2 * 2)
    with zipfile.ZipFile(released, "a", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as handle:
        for name, content in sorted(notices.items()):
            entry = zipfile.ZipInfo(name, date_time=stamp)
            entry.create_system = 3
            entry.external_attr = 0o100644 << 16
            entry.compress_type = zipfile.ZIP_DEFLATED
            handle.writestr(entry, content, compresslevel=9)
    released.chmod(0o755)
    final = released.read_bytes()
    manifest = {
        "version": version, "target": args.target, "source_commit": commit,
        "source_date_epoch": epoch, "rustc": run(["rustc", f"+{TOOLCHAIN}", "-Vv"]),
        "release_profile": tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["profile"]["release"],
        "target_rustflags": os.environ.get("CARGO_TARGET_" + args.target.upper().replace("-", "_") + "_RUSTFLAGS", ""),
        "build_binary_sha256": digest(data),
        "binary": {"name": released.name, "bytes": len(final), "sha256": digest(final)},
    }
    (ROOT / ".tmp").mkdir(exist_ok=True)
    (ROOT / ".tmp" / f"release-manifest-{args.target}.json").write_text(
        json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    print("BUILD_METADATA " + json.dumps(manifest, sort_keys=True), flush=True)
    print(f"{released.name}: {len(final):,} bytes")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["smoke", "package"])
    parser.add_argument("--target", required=True, choices=sorted(TARGETS))
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--runner-json", default="[]")
    parser.add_argument("--output", default=".tmp/release")
    args = parser.parse_args()
    version = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["package"]["version"]
    binary = (args.binary or binary_for(args.target)).resolve()
    if args.action == "smoke":
        smoke(args, version, binary)
    else:
        package(args, version, binary)


if __name__ == "__main__":
    main()
