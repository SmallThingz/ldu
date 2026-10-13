#!/usr/bin/env python3
"""Prepare and smoke-test Linux, FreeBSD and macOS release binaries (Python 3.11+, Git, Rust 1.99.0)."""

import argparse
import datetime
import hashlib
import io
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
    "x86_64-unknown-freebsd",
    "aarch64-unknown-freebsd",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
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


def run(command, timeout=120, env=None):
    result = subprocess.run(
        [str(part) for part in command], cwd=ROOT, check=False, timeout=timeout,
        stdout=subprocess.PIPE, text=True, encoding="utf-8", env=env,
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


def target_system(target):
    require(target in TARGETS, f"Unsupported release target: {target}")
    if target.endswith("-apple-darwin"):
        return "Darwin"
    if target.endswith("-unknown-freebsd"):
        return "FreeBSD"
    return "Linux"


def bounded(data, offset, size):
    require(0 <= offset <= len(data) and 0 <= size <= len(data) - offset,
            "Truncated executable structure")
    return data[offset:offset + size]


def c_string(data, offset):
    require(0 <= offset < len(data), "Invalid executable string offset")
    end = data.find(b"\0", offset)
    require(end >= offset, "Unterminated executable string")
    return data[offset:end].decode("utf-8", errors="strict")


def inspect_elf(data, target):
    expected = {"x86_64": 62, "aarch64": 183, "armv7": 40, "riscv64gc": 243}
    require(len(data) >= 20 and data[:4] == b"\x7fELF" and data[5:7] == b"\x01\x01",
            "Expected little-endian ELF")
    is_32 = target.startswith("armv7-")
    require(data[4] == (1 if is_32 else 2), f"Wrong ELF class for {target}")
    require(struct.unpack_from("<H", data, 18)[0] == expected[target.split("-")[0]],
            f"Wrong executable architecture for {target}")
    system = target_system(target)
    require(data[7] == 9 if system == "FreeBSD" else data[7] in (0, 3),
            f"Wrong ELF OS ABI for {target}")
    require(struct.unpack_from("<H", data, 16)[0] in (2, 3), "ELF is not executable")
    bounded(data, 0, 52 if is_32 else 64)
    if is_32:
        phoff = struct.unpack_from("<I", data, 28)[0]
        phsize, phnum = struct.unpack_from("<HH", data, 42)
    else:
        phoff = struct.unpack_from("<Q", data, 32)[0]
        phsize, phnum = struct.unpack_from("<HH", data, 54)
    require(phnum > 0 and phsize == (32 if is_32 else 56), "Invalid ELF program headers")
    bounded(data, phoff, phsize * phnum)
    segments, dynamic, interpreters = [], [], []
    for index in range(phnum):
        offset = phoff + index * phsize
        if is_32:
            kind, start, address, _, size = struct.unpack_from("<IIIII", data, offset)
        else:
            kind, _, start, address, _, size = struct.unpack_from("<IIQQQQ", data, offset)
        content = bounded(data, start, size)
        if kind == 1:
            segments.append((address, start, size))
        elif kind == 3:
            interpreters.append(c_string(content, 0))
        elif kind == 2:
            entry_size = 8 if is_32 else 16
            require(size % entry_size == 0, "Invalid ELF dynamic table")
            terminated = False
            for entry in range(0, size, entry_size):
                tag, value = struct.unpack_from("<II" if is_32 else "<QQ", content, entry)
                if tag == 0:
                    terminated = True
                    break
                dynamic.append((tag, value))
            require(terminated, "Unterminated ELF dynamic table")
    needed_offsets = [value for tag, value in dynamic if tag == 1]
    libraries = []
    if needed_offsets:
        tables = [value for tag, value in dynamic if tag == 5]
        sizes = [value for tag, value in dynamic if tag == 10]
        require(len(tables) == len(sizes) == 1, "Missing ELF dynamic string table")
        matches = [(start + tables[0] - address, sizes[0])
                   for address, start, size in segments
                   if address <= tables[0] and tables[0] + sizes[0] <= address + size]
        require(len(matches) == 1, "Invalid ELF dynamic string table")
        strings = bounded(data, *matches[0])
        libraries = [c_string(strings, offset) for offset in needed_offsets]
    if system == "Linux":
        require(not interpreters and not libraries, "musl release must be statically linked")
    else:
        require(interpreters == ["/libexec/ld-elf.so.1"],
                "FreeBSD releases require the base-system dynamic loader")
        allowed = {"libc.so.7", "libm.so.5", "libthr.so.3", "librt.so.1", "libgcc_s.so.1",
                   "libutil.so.9", "libexecinfo.so.1"}
        require(libraries and set(libraries) <= allowed,
                f"Unexpected FreeBSD shared libraries: {libraries}")
        require(not any(tag in (15, 29) for tag, _ in dynamic),
                "FreeBSD releases must not depend on a custom library search path")
    return {"format": "ELF", "static": not interpreters and not libraries,
            "interpreter": interpreters[0] if interpreters else None,
            "libraries": libraries}


def inspect_macho(data, target):
    bounded(data, 0, 32)
    require(data[:4] == b"\xcf\xfa\xed\xfe", "Expected little-endian 64-bit Mach-O")
    cpu, _, kind, commands, command_bytes = struct.unpack_from("<IIIII", data, 4)
    expected_cpu = 0x1000007 if target.startswith("x86_64-") else 0x100000c
    require(cpu == expected_cpu, f"Wrong executable architecture for {target}")
    require(kind == 2, "Mach-O is not an executable")
    bounded(data, 32, command_bytes)
    offset, libraries, interpreters, sections, signatures = 32, [], [], [], []
    platforms = []
    for _ in range(commands):
        require(offset + 8 <= 32 + command_bytes, "Truncated Mach-O load command")
        command, size = struct.unpack_from("<II", data, offset)
        require(size >= 8 and size % 8 == 0 and offset + size <= 32 + command_bytes,
                "Invalid Mach-O load command size")
        content = bounded(data, offset, size)
        if command in (0xc, 0x80000018, 0x8000001f, 0x80000023, 0x20):
            require(size >= 24, "Truncated Mach-O dylib command")
            libraries.append(c_string(content, struct.unpack_from("<I", content, 8)[0]))
        elif command == 0xe:
            require(size >= 12, "Truncated Mach-O loader command")
            interpreters.append(c_string(content, struct.unpack_from("<I", content, 8)[0]))
        elif command == 0x19:
            require(size >= 72, "Truncated Mach-O segment")
            segment_name = content[8:24].rstrip(b"\0")
            segment_start, segment_length = struct.unpack_from("<QQ", content, 40)
            bounded(data, segment_start, segment_length)
            count = struct.unpack_from("<I", content, 64)[0]
            require(72 + count * 80 == size, "Invalid Mach-O section table")
            for index in range(count):
                section = content[72 + index * 80:152 + index * 80]
                if section[:16].rstrip(b"\0") == b"__ldu_notices":
                    require(segment_name == b"__TEXT" and section[16:32].rstrip(b"\0") == b"__TEXT",
                            "Notice section must be in __TEXT")
                    length = struct.unpack_from("<Q", section, 40)[0]
                    start = struct.unpack_from("<I", section, 48)[0]
                    require(segment_start <= start and start + length <= segment_start + segment_length,
                            "Notice section lies outside its segment")
                    bounded(data, start, length)
                    sections.append((start, length))
        elif command == 0x32:
            require(size >= 24, "Truncated Mach-O platform command")
            platforms.append(struct.unpack_from("<I", content, 8)[0])
        elif command == 0x24:
            platforms.append(1)
        elif command in (0x25, 0x2f, 0x30):
            raise RuntimeError("Expected a macOS executable, not another Apple OS")
        elif command == 0x8000001c:
            raise RuntimeError("macOS releases must not depend on a custom library search path")
        elif command == 0x1d:
            require(size == 16, "Invalid Mach-O code signature command")
            start, length = struct.unpack_from("<II", content, 8)
            bounded(data, start, length)
            require(length > 0 and start + length == len(data),
                    "Mach-O code signature must end the file; no appended data")
            signatures.append((start, length))
        offset += size
    require(offset == 32 + command_bytes, "Invalid Mach-O load command count")
    require(platforms == [1], "Expected macOS platform metadata")
    require(interpreters == ["/usr/lib/dyld"], "Unexpected macOS dynamic loader")
    require(libraries and all(path.startswith(("/usr/lib/", "/System/Library/Frameworks/"))
                              and "/../" not in path for path in libraries),
            f"Unexpected macOS shared libraries: {libraries}")
    require(len(sections) <= 1 and len(signatures) <= 1, "Duplicate Mach-O metadata")
    return {"format": "Mach-O", "static": False, "interpreter": interpreters[0],
            "libraries": libraries, "notice_section": sections[0] if sections else None,
            "code_signature": signatures[0] if signatures else None}


def check_architecture(data, target):
    return inspect_macho(data, target) if target_system(target) == "Darwin" else inspect_elf(data, target)


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
    linkage = check_architecture(data, args.target)
    require(platform.system() == target_system(args.target),
            "Run smoke inside the target OS; a cross-build or remote launcher is not a native smoke")
    if target_system(args.target) == "Darwin":
        run(["codesign", "--verify", "--strict", binary])
        linkage["signature_verified"] = True
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
        smoke_env = dict(os.environ, XDG_CONFIG_HOME=str(root / "absent-config"))
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
                report = json.loads(run(scan_command, env=smoke_env))
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
    result = {
        "version": version, "target": args.target, "binary_sha256": digest(data),
        "runner": runner, "host_system": platform.system(), "host_machine": platform.machine(),
        "runtime_evidence": "emulated-architecture" if runner else "native-os",
        "checks": ["architecture", "version", "help", *[f"ncdu-export-{backend}" for backend in backends]],
        "linkage": linkage,
    }
    report_path = ROOT / ".tmp" / f"release-smoke-{args.target}.json"
    report_path.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print("SMOKE_METADATA " + json.dumps(result, sort_keys=True), flush=True)
    print(f"Smoke passed: {args.target} ({' '.join(runner) if runner else 'native'})")


def notice_contents(notices, epoch):
    timestamp = datetime.datetime.fromtimestamp(max(epoch, 315532800), datetime.timezone.utc)
    stamp = (timestamp.year, timestamp.month, timestamp.day,
             timestamp.hour, timestamp.minute, timestamp.second // 2 * 2)
    output = io.BytesIO()
    with zipfile.ZipFile(output, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as handle:
        for name, content in sorted(notices.items()):
            entry = zipfile.ZipInfo(name, date_time=stamp)
            entry.create_system = 3
            entry.external_attr = 0o100644 << 16
            entry.compress_type = zipfile.ZIP_DEFLATED
            handle.writestr(entry, content, compresslevel=9)
    return output.getvalue()


def validate_notices(data, notices):
    with zipfile.ZipFile(io.BytesIO(data)) as handle:
        require(handle.testzip() is None, "Corrupt license archive")
        require(len(handle.namelist()) == len(notices) and set(handle.namelist()) == set(notices),
                "Unexpected license archive entries")
        require(all(handle.read(name) == content for name, content in notices.items()),
                "License archive content mismatch; regenerate notices and rebuild")


def release_notices(target):
    notices = collect_licenses(ROOT, TOOLCHAIN, target, run)
    notices["LICENSE"] = (ROOT / "LICENSE").read_bytes()
    return notices


def prepare_notices(args):
    epoch = int(run(["git", "show", "-s", "--format=%ct", "HEAD"]))
    notices = release_notices(args.target)
    data = notice_contents(notices, epoch)
    validate_notices(data, notices)
    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_bytes(data)
    print(f"Prepared {len(data):,} notice bytes for {args.target}: {output}")


def extract_notices(args, binary):
    data = binary.read_bytes()
    linkage = check_architecture(data, args.target)
    if target_system(args.target) == "Darwin":
        section = linkage["notice_section"]
        require(section is not None, "Missing __TEXT,__ldu_notices section")
        archive = bounded(data, *section)
    else:
        with zipfile.ZipFile(io.BytesIO(data)) as handle:
            start = min(entry.header_offset for entry in handle.infolist())
        archive = data[start:]
    # The output is a ZIP, never arbitrary archive extraction onto the filesystem.
    with zipfile.ZipFile(io.BytesIO(archive)) as handle:
        require(handle.testzip() is None, "Corrupt license archive")
    with Path(args.output).open("xb") as output:
        output.write(archive)



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
    linkage = check_architecture(data, args.target)
    system = target_system(args.target)
    require(not args.upx or system == "Linux", "UPX packaging is supported only for Linux musl")
    commit = run(["git", "rev-parse", "HEAD"])
    source_dirty = bool(run(["git", "status", "--porcelain", "--untracked-files=all"]))
    epoch = int(run(["git", "show", "-s", "--format=%ct", "HEAD"]))
    notices = release_notices(args.target)
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
            notices.update(collect_upx_licenses(args.upx.resolve()))
        executable_bytes = prepared.stat().st_size
        executable_sha256 = compression["sha256"] if compression else digest(data)
        if system == "Darwin":
            # Apple forbids Mach-O overlays. The linker places these bytes in a
            # proper section before code signing; packaging never edits the image.
            section = linkage["notice_section"]
            require(section is not None,
                    "Missing __TEXT,__ldu_notices; run notices and build with -sectcreate")
            require(linkage["code_signature"] is not None, "Mach-O release needs a code signature")
            archive = bounded(data, *section)
            validate_notices(archive, notices)
            notice_offset = section[0]
            if platform.system() == "Darwin":
                run(["codesign", "--verify", "--strict", prepared])
                linkage["signature_verified"] = True
            else:
                linkage["signature_verified"] = False
            final = prepared.read_bytes()
            require(final == data, "Packaging changed Mach-O executable bytes")
        else:
            # ELF loaders ignore trailing data. Keep the ZIP readable without
            # executing or unpacking the program, including after optional UPX.
            archive = notice_contents(notices, epoch)
            with prepared.open("ab") as output_file:
                output_file.write(archive)
            final = prepared.read_bytes()
            require(digest(final[:executable_bytes]) == executable_sha256,
                    "Appending notices changed executable bytes")
            notice_offset = executable_bytes
            validate_notices(archive, notices)
            with zipfile.ZipFile(prepared) as handle:
                require(min(entry.header_offset for entry in handle.infolist()) == executable_bytes,
                        "License archive overlaps executable bytes")
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
        "compression": compression, "linkage": linkage,
        "runtime_tested": False,
        "notice_trailer" if system != "Darwin" else "notice_section": {
            "format": "zip", "offset": notice_offset,
            "bytes": len(archive),
            "sha256": digest(archive),
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
    parser.add_argument("action", choices=["smoke", "package", "notices", "extract-notices"])
    parser.add_argument("--target", required=True, choices=sorted(TARGETS))
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--runner-json", default="[]")
    parser.add_argument("--output", default=".tmp/release")
    parser.add_argument("--upx", type=Path,
                        help=f"package with official UPX {UPX_VERSION} (adds launch-time decompression)")
    args = parser.parse_args()
    if args.upx and args.action != "package":
        parser.error("--upx is only valid for package")
    if args.upx and target_system(args.target) != "Linux":
        parser.error("--upx supports only Linux musl targets")
    if args.action in ("notices", "extract-notices") and args.output == ".tmp/release":
        parser.error("--output must name the notice ZIP file")
    if args.action == "notices":
        prepare_notices(args)
        return
    version = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["package"]["version"]
    binary = (args.binary or binary_for(args.target)).resolve()
    if args.action == "smoke":
        smoke(args, version, binary)
    elif args.action == "extract-notices":
        extract_notices(args, binary)
    else:
        package(args, version, binary)


if __name__ == "__main__":
    main()
