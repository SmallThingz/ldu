"""Cross-platform artifact guards; fixtures describe headers, not runnable code."""

import contextlib
import io
import json
from pathlib import Path
import struct
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import release


def elf(target="x86_64-unknown-linux-musl", libraries=(), interpreter=None, rpath=False):
    is_32 = target.startswith("armv7-")
    freebsd = target.endswith("freebsd")
    data = bytearray(1024)
    data[:7] = b"\x7fELF" + bytes([1 if is_32 else 2, 1, 1])
    data[7] = 9 if freebsd else 0
    machine = {"x86_64": 62, "aarch64": 183, "armv7": 40, "riscv64gc": 243}
    struct.pack_into("<HHI", data, 16, 2, machine[target.split("-")[0]], 1)
    headers = [(1, 0, 1024)]
    if interpreter:
        encoded = interpreter.encode() + b"\0"
        data[400:400 + len(encoded)] = encoded
        headers.append((3, 400, len(encoded)))
    if libraries or rpath:
        strings = b"\0"
        dynamic = []
        for name in libraries:
            dynamic.append((1, len(strings)))
            strings += name.encode() + b"\0"
        data[700:700 + len(strings)] = strings
        dynamic += [(5, 700), (10, len(strings))]
        if rpath:
            dynamic.append((29, 0))
        dynamic.append((0, 0))
        for index, entry in enumerate(dynamic):
            struct.pack_into("<QQ", data, 500 + index * 16, *entry)
        headers.append((2, 500, len(dynamic) * 16))
    phoff, phsize = (52, 32) if is_32 else (64, 56)
    if is_32:
        struct.pack_into("<I", data, 28, phoff)
        struct.pack_into("<HH", data, 42, phsize, len(headers))
    else:
        struct.pack_into("<Q", data, 32, phoff)
        struct.pack_into("<HH", data, 54, phsize, len(headers))
    for index, (kind, start, size) in enumerate(headers):
        if is_32:
            struct.pack_into("<IIIIIIII", data, phoff + index * phsize,
                             kind, start, start, start, size, size, 5, 4096)
        else:
            struct.pack_into("<IIQQQQQQ", data, phoff + index * phsize,
                             kind, 5, start, start, start, size, size, 4096)
    return bytes(data)


def macho(target="aarch64-apple-darwin", archive=b"notices", library="/usr/lib/libSystem.B.dylib",
          os_platform=1, signature=True, section=True):
    commands = []
    for command, path, header_size in [(0xe, "/usr/lib/dyld", 12), (0xc, library, 24)]:
        size = (header_size + len(path) + 1 + 7) // 8 * 8
        content = bytearray(size)
        struct.pack_into("<III", content, 0, command, size, header_size)
        content[header_size:header_size + len(path)] = path.encode()
        commands.append(content)
    commands.append(bytearray(struct.pack("<IIIIII", 0x32, 24, os_platform, 0xb0000, 0xb0000, 0)))
    section_offset = 512
    if section:
        segment = bytearray(152)
        struct.pack_into("<II16sQQQQIIII", segment, 0, 0x19, 152, b"__TEXT", 0,
                         section_offset + len(archive), 0, section_offset + len(archive), 5, 5, 1, 0)
        struct.pack_into("<16s16sQQIIIIIIII", segment, 72, b"__ldu_notices", b"__TEXT",
                         section_offset, len(archive), section_offset, 0, 0, 0, 0, 0, 0, 0)
        commands.append(segment)
    if signature:
        commands.append(bytearray(struct.pack("<IIII", 0x1d, 16, section_offset + len(archive), 32)))
    header = struct.pack("<IIIIIIII", 0xfeedfacf,
                         0x1000007 if target.startswith("x86_64-") else 0x100000c,
                         0, 2, len(commands), sum(map(len, commands)), 0, 0)
    data = bytearray(header + b"".join(commands))
    data.extend(bytes(section_offset - len(data)))
    data.extend(archive)
    if signature:
        data.extend(bytes(32))
    return bytes(data)


class FormatTests(unittest.TestCase):
    def test_all_linux_musl_architectures_remain_static(self):
        for target in release.TARGETS:
            if "-linux-" in target:
                with self.subTest(target=target):
                    self.assertTrue(release.check_architecture(elf(target), target)["static"])
        with self.assertRaisesRegex(RuntimeError, "statically linked"):
            release.check_architecture(elf(libraries=["libc.so.6"],
                                           interpreter="/lib64/ld-linux-x86-64.so.2"),
                                       "x86_64-unknown-linux-musl")

    def test_freebsd_requires_its_abi_loader_and_only_base_libraries(self):
        for target in ["x86_64-unknown-freebsd", "aarch64-unknown-freebsd"]:
            data = elf(target, ["libc.so.7", "libthr.so.3"], "/libexec/ld-elf.so.1")
            self.assertFalse(release.check_architecture(data, target)["static"])
            with self.assertRaisesRegex(RuntimeError, "OS ABI"):
                release.check_architecture(data, target.replace("freebsd", "linux-musl"))
            for libraries, loader, rpath in [(["libextra.so.1"], "/libexec/ld-elf.so.1", False),
                                              (["libc.so.7"], "/custom/loader", False),
                                              (["libc.so.7"], "/libexec/ld-elf.so.1", True)]:
                with self.assertRaises(RuntimeError):
                    release.check_architecture(elf(target, libraries, loader, rpath), target)

    def test_both_macos_architectures_keep_notice_section_inside_signature(self):
        for target in ["aarch64-apple-darwin", "x86_64-apple-darwin"]:
            data = macho(target)
            result = release.check_architecture(data, target)
            self.assertEqual(release.bounded(data, *result["notice_section"]), b"notices")
            self.assertEqual(sum(result["code_signature"]), len(data))

    def test_wrong_apple_os_architecture_libraries_and_overlays_are_rejected(self):
        for data in [macho(os_platform=2), macho("x86_64-apple-darwin"),
                     macho(library="@rpath/libextra.dylib"), macho() + b"trailer"]:
            with self.subTest(length=len(data)), self.assertRaises(RuntimeError):
                release.check_architecture(data, "aarch64-apple-darwin")

    def test_truncated_binary_headers_are_rejected(self):
        for target, data in [("aarch64-apple-darwin", macho()),
                             ("x86_64-unknown-linux-musl", elf())]:
            for length in [0, 16, 31, 50, 100]:
                with self.subTest(target=target, length=length), self.assertRaises(RuntimeError):
                    release.check_architecture(data[:length], target)

    def test_zip_rejects_changed_notice_texts(self):
        data = release.notice_contents({"LICENSE": b"original"}, 1)
        release.validate_notices(data, {"LICENSE": b"original"})
        with self.assertRaisesRegex(RuntimeError, "regenerate notices"):
            release.validate_notices(data, {"LICENSE": b"changed"})


class PackageTests(unittest.TestCase):
    def setUp(self):
        temporary = release.ROOT / ".tmp"
        temporary.mkdir(exist_ok=True)
        self.directory = tempfile.TemporaryDirectory(prefix="package-test-", dir=temporary)
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        (self.root / "Cargo.toml").write_text("[profile.release]\nopt-level = 3\n")

    @staticmethod
    def command(command, **_):
        if command[:2] == ["git", "rev-parse"]:
            return "a" * 40
        if command[:2] == ["git", "status"]:
            return ""
        if command[:2] == ["git", "show"]:
            return "1700000000"
        if command[0] == "rustc":
            return "rustc 1.99.0"
        raise AssertionError(command)

    def package(self, data, notices, target="aarch64-apple-darwin"):
        binary = self.root / "input"
        binary.write_bytes(data)
        args = SimpleNamespace(target=target, output=self.root / "out", upx=None)
        with patch.object(release, "ROOT", self.root), patch.object(release, "run", self.command), \
                patch.object(release, "release_notices", return_value=notices), \
                patch.object(release.platform, "system", return_value="Linux"), \
                contextlib.redirect_stdout(io.StringIO()):
            release.package(args, "0.1.0", binary)
        return self.root / "out" / f"ldu-v0.1.0-{target}"

    def test_mac_package_preserves_every_signed_byte_and_records_no_runtime_claim(self):
        notices = {"LICENSE": b"terms", "licenses/THIRD-PARTY-NOTICES.txt": b"required"}
        data = macho(archive=release.notice_contents(notices, 1700000000))
        output = self.package(data, notices)
        self.assertEqual(output.read_bytes(), data)
        manifest = json.loads((self.root / ".tmp/release-manifest-aarch64-apple-darwin.json").read_text())
        self.assertFalse(manifest["runtime_tested"])
        self.assertFalse(manifest["linkage"]["signature_verified"])
        self.assertEqual([path.name for path in output.parent.iterdir()], [output.name])

    def test_mac_missing_or_stale_embedded_notices_never_produce_an_asset(self):
        notices = {"LICENSE": b"current"}
        for data in [macho(section=False),
                     macho(archive=release.notice_contents({"LICENSE": b"stale"}, 1))]:
            with self.assertRaises(RuntimeError):
                self.package(data, notices)
            self.assertFalse(any((self.root / "out").iterdir()))

    def test_elf_packages_keep_notice_zip_and_machine_code_separate(self):
        notices = {"LICENSE": b"terms"}
        for target, data in [("x86_64-unknown-linux-musl", elf()),
                             ("aarch64-unknown-freebsd", elf("aarch64-unknown-freebsd",
                                ["libc.so.7"], "/libexec/ld-elf.so.1"))]:
            output = self.package(data, notices, target)
            self.assertEqual(output.read_bytes()[:len(data)], data)
            release.validate_notices(output.read_bytes()[len(data):], notices)
            args = SimpleNamespace(target=target, output=self.root / (target + ".zip"))
            release.extract_notices(args, output)
            release.validate_notices(args.output.read_bytes(), notices)

    def test_foreign_os_smoke_is_rejected_before_executing_any_binary(self):
        binary = self.root / "input"
        binary.write_bytes(macho())
        with patch.object(release.platform, "system", return_value="Linux"), \
                patch.object(release, "run") as run, self.assertRaisesRegex(RuntimeError, "target OS"):
            release.smoke(SimpleNamespace(target="aarch64-apple-darwin"), "0.1.0", binary)
        run.assert_not_called()


if __name__ == "__main__":
    unittest.main()
