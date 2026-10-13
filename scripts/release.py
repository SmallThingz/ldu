#!/usr/bin/env python3
"""Prepare and smoke-test Linux release binaries (Python 3.11+, Git, Rust 1.99.0)."""

import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import platform
import stat
import struct
import subprocess
import tempfile
import tomllib
import zipfile

from licenses import collect_licenses, collect_upx_licenses

TARGETS = {
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "armv7-unknown-linux-musleabihf",
    "riscv64gc-unknown-linux-musl",
}
TOOLCHAIN = "1.99.0"
UPX_VERSION = "5.2.1"
# Official unmodified Linux executables from github.com/upx/upx/releases/tag/v5.2.1.
# Pinning the tool also preserves UPX's compressed-program licensing exception.
UPX_SHA256 = {
    "287b3dffe9dcafd8e366e162ac4ab41e5cf45a3c6768970256af0869288d84a1",  # amd64
    "871d65c543cfda923d8502b92a58d4934c434aead8746d7b4a7f6dad66eb3dad",  # arm64
}
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
    expected_class = 1 if target.startswith("armv7-") else 2
    require(data[4] == expected_class, f"Wrong ELF class for {target}")
    actual = struct.unpack_from("<H", data, 18)[0]
    require(actual == expected[target.split("-")[0]],
            f"Wrong executable architecture for {target}")


def check_ncdu_export(report, root, paths, version, backend):
    require(isinstance(report, list) and len(report) == 4 and report[:2] == [1, 2],
            f"{backend}: expected ncdu 1.2 export")
    metadata = report[2]
    require(isinstance(metadata, dict)
            and set(metadata) == {"progname", "progver", "timestamp"},
            f"{backend}: invalid export metadata")
    require(metadata["progname"] == "ldu" and metadata["progver"] == version,
            f"{backend}: wrong export producer")
    require(type(metadata["timestamp"]) is int and metadata["timestamp"] > 0,
            f"{backend}: invalid export timestamp")
    expected = {path.relative_to(root): path.lstat() for path in paths}
    records = {}
    pending = [(Path(), report[3])]
    while pending:
        relative, entry = pending.pop()
        directory = isinstance(entry, list)
        require(not directory or bool(entry), f"{backend}: empty directory record")
        record = entry[0] if directory else entry
        require(isinstance(record, dict), f"{backend}: invalid entry record")
        require(relative in expected and relative not in records,
                f"{backend}: unexpected or duplicate path {relative}")
        info = expected[relative]
        require(directory == stat.S_ISDIR(info.st_mode),
                f"{backend}: wrong entry type for {relative}")
        wanted = {
            "name": str(root) if relative == Path() else relative.name,
            "asize": info.st_size,
            "dsize": info.st_blocks * 512,
        }
        if not (directory or stat.S_ISREG(info.st_mode)):
            wanted["notreg"] = True
        require(record == wanted, f"{backend}: wrong metadata for {relative}: {record}")
        require(type(record["asize"]) is int and type(record["dsize"]) is int,
                f"{backend}: non-integer size for {relative}")
        records[relative] = record
        if directory:
            for child in entry[1:]:
                child_record = child[0] if isinstance(child, list) and child else child
                require(isinstance(child_record, dict), f"{backend}: invalid child record")
                name = child_record.get("name")
                require(isinstance(name, str) and name not in ("", ".", "..")
                        and "/" not in name, f"{backend}: invalid child name")
                pending.append((relative / name, child))
    require(records.keys() == expected.keys(), f"{backend}: missing entries in export")
    return {key: value for key, value in metadata.items() if key != "timestamp"}, records


def smoke(args, version, binary):
    data = binary.read_bytes()
    check_architecture(data, args.target)
    runner = json.loads(args.runner_json)
    require(isinstance(runner, list) and all(isinstance(arg, str) for arg in runner),
            "--runner-json must be an array of command arguments")
    command = runner + [str(binary)]
    require(run(command + ["--version"]) == f"ldu {version}", "Wrong binary version")
    help_text = run(command + ["--help"])
    require(all(text in help_text for text in ["Usage:", "--output", "--backend"]),
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
        (root / 'quote-"-\\-é.txt').write_bytes(b"escaped filename\n")
        (root / "link-file").symlink_to("top.txt")
        (root / "link-directory").symlink_to("nested", target_is_directory=True)
        paths = [root, *root.rglob("*")]
        backends = ["auto", "native", "std"]
        reference = None
        for backend in backends:
            scan_command = command + [
                "-o", "-", "--backend", backend, "--threads", "2", str(root),
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
            exported = check_ncdu_export(report, root, paths, version, backend)
            if reference is None:
                reference = exported
            else:
                require(exported == reference, f"{backend}: export differs between backends")
    check_static_linkage(binary)
    linkage = {"static": True}
    result = {
        "version": version, "target": args.target, "binary_sha256": digest(data),
        "runner": runner, "host_system": platform.system(), "host_machine": platform.machine(),
        "checks": ["architecture", "version", "help", *[f"ncdu-export-{backend}" for backend in backends]],
        "linkage": linkage,
    }
    report_path = ROOT / ".tmp" / f"release-smoke-{args.target}.json"
    report_path.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print("SMOKE_METADATA " + json.dumps(result, sort_keys=True), flush=True)
    print(f"Smoke passed: {args.target} ({' '.join(runner) if runner else 'native'})")


def check_static_linkage(binary):
    headers = run(["readelf", "--program-headers", binary])
    dynamic = run(["readelf", "--dynamic", binary])
    require("INTERP" not in headers and "(NEEDED)" not in dynamic,
            "musl release must be statically linked")


def pack_upx(binary, original, upx):
    upx = upx.resolve(strict=True)
    tool_hash = digest(upx.read_bytes())
    require(tool_hash in UPX_SHA256,
            f"Use an official unmodified UPX {UPX_VERSION} Linux amd64 or arm64 executable")
    command = [upx, "--no-env", "--no-progress", "--no-color"]
    version = run(command + ["--version"]).splitlines()[0]
    require(version == f"upx {UPX_VERSION}", "Unexpected UPX version")
    # NRV avoids LZMA's slower launch-time decompression. Keep an exact, reversible
    # executable and reject pre-existing overlays; legal notices are appended later.
    flags = ["-9", "--nrv2e", "--exact", "--overlay=skip"]
    run(command + flags + [binary], timeout=600)
    run(command + ["--test", binary])
    restored = binary.with_name(binary.name + ".unpacked")
    try:
        run(command + ["--decompress", "--output=" + str(restored), binary])
        require(restored.read_bytes() == original, "UPX round trip changed the executable")
    finally:
        restored.unlink(missing_ok=True)
    packed = binary.read_bytes()
    require(len(packed) < len(original), "UPX did not reduce executable size")
    return {
        "name": "upx", "version": UPX_VERSION, "tool_sha256": tool_hash,
        "flags": flags, "bytes": len(packed), "sha256": digest(packed),
        "round_trip_sha256": digest(original),
    }


def package(args, version, binary):
    data = binary.read_bytes()
    check_architecture(data, args.target)
    commit = run(["git", "rev-parse", "HEAD"])
    source_dirty = bool(run(["git", "status", "--porcelain", "--untracked-files=all"]))
    epoch = int(run(["git", "show", "-s", "--format=%ct", "HEAD"]))
    check_static_linkage(binary)
    notices = collect_licenses(ROOT, TOOLCHAIN, args.target, run)
    notices["LICENSE"] = (ROOT / "LICENSE").read_bytes()
    output = Path(args.output).resolve()
    output.mkdir(parents=True, exist_ok=True)
    released = output / f"ldu-v{version}-{args.target}"
    require(released != binary, "Release destination must differ from the build output")
    compression = None
    # Assemble next to the destination and rename only after all checks pass.
    with tempfile.TemporaryDirectory(prefix=".package-", dir=output) as work:
        prepared = Path(work) / released.name
        prepared.write_bytes(data)
        prepared.chmod(0o755)
        if args.upx:
            compression = pack_upx(prepared, data, args.upx)
            check_architecture(prepared.read_bytes(), args.target)
            check_static_linkage(prepared)
            notices.update(collect_upx_licenses(args.upx.resolve()))
        executable_bytes = prepared.stat().st_size
        executable_sha256 = compression["sha256"] if compression else digest(data)
        # ELF loaders ignore trailing data. Append notices after packing so ZIP
        # readers can inspect them without running or unpacking the executable.
        timestamp = datetime.datetime.fromtimestamp(max(epoch, 315532800), datetime.timezone.utc)
        stamp = (timestamp.year, timestamp.month, timestamp.day,
                 timestamp.hour, timestamp.minute, timestamp.second // 2 * 2)
        with zipfile.ZipFile(prepared, "a", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as handle:
            for name, content in sorted(notices.items()):
                entry = zipfile.ZipInfo(name, date_time=stamp)
                entry.create_system = 3
                entry.external_attr = 0o100644 << 16
                entry.compress_type = zipfile.ZIP_DEFLATED
                handle.writestr(entry, content, compresslevel=9)
        final = prepared.read_bytes()
        require(digest(final[:executable_bytes]) == executable_sha256,
                "Appending notices changed executable bytes")
        with zipfile.ZipFile(prepared) as handle:
            require(handle.testzip() is None, "Corrupt license archive")
            require(set(handle.namelist()) == set(notices), "Unexpected license archive entries")
            require(min(entry.header_offset for entry in handle.infolist()) == executable_bytes,
                    "License archive overlaps executable bytes")
            require(all(handle.read(name) == content for name, content in notices.items()),
                    "License archive content mismatch")
        if args.upx:
            # UPX's inspection tool locates its header at EOF. The loader accepts
            # our ZIP trailer, but --test/--decompress need the original ELF prefix.
            # Test that exact prefix recovered from the finished artifact; its hash
            # must still match the stream whose unpacking was byte-identical above.
            verified = Path(work) / "verified-packed"
            verified.write_bytes(final[:executable_bytes])
            run([args.upx.resolve(), "--no-env", "--test", verified])
            compression["verification"] = "UPX test and exact round trip of ELF prefix; ZIP trailer excluded"
        prepared.replace(released)
    manifest = {
        "version": version, "target": args.target, "source_commit": commit,
        "source_dirty": source_dirty,
        "source_date_epoch": epoch, "rustc": run(["rustc", f"+{TOOLCHAIN}", "-Vv"]),
        "release_profile": tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["profile"]["release"],
        "target_rustflags": os.environ.get("CARGO_TARGET_" + args.target.upper().replace("-", "_") + "_RUSTFLAGS", ""),
        "build_binary_sha256": digest(data), "build_binary_bytes": len(data),
        "compression": compression,
        "notice_trailer": {
            "format": "zip", "offset": executable_bytes,
            "bytes": len(final) - executable_bytes,
            "sha256": digest(final[executable_bytes:]),
            "entries": sorted(notices), "verified": True,
        },
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
    parser.add_argument("--upx", type=Path,
                        help=f"package with official UPX {UPX_VERSION} (adds launch-time decompression)")
    args = parser.parse_args()
    if args.upx and args.action != "package":
        parser.error("--upx is only valid for package")
    version = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["package"]["version"]
    binary = (args.binary or binary_for(args.target)).resolve()
    if args.action == "smoke":
        smoke(args, version, binary)
    else:
        package(args, version, binary)


if __name__ == "__main__":
    main()
