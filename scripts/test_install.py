"""Installer checks using local fake downloads; no network or downloaded program runs."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
INSTALLER = ROOT / 'scripts/install.sh'


class InstallerTests(unittest.TestCase):
    def setUp(self):
        (ROOT / '.tmp').mkdir(exist_ok=True)
        self.directory = tempfile.TemporaryDirectory(prefix='installer-test-', dir=ROOT / '.tmp')
        self.addCleanup(self.directory.cleanup)
        self.work = Path(self.directory.name)
        self.tools = self.work / 'tools'
        self.tools.mkdir()
        self.home = self.work / 'home with spaces'
        self.home.mkdir()
        self.destination = self.home / '.local/bin'
        self.log = self.work / 'download-url'
        self.tool('uname', '''#!/bin/sh
case "$1" in
  -s) printf '%s\\n' "$LDU_TEST_OS" ;;
  -m) printf '%s\\n' "$LDU_TEST_ARCH" ;;
  *) exit 2 ;;
esac
''')
        self.tool('sysctl', '''#!/bin/sh
printf '%s\\n' "${LDU_TEST_ROSETTA:-0}"
''')
        self.tool('curl', r'''#!/usr/bin/env python3
import os
from pathlib import Path
import struct
import sys
arguments = sys.argv[1:]
output = Path(arguments[arguments.index('--output') + 1])
Path(os.environ['LDU_TEST_LOG']).write_text(arguments[-1])
mode = os.environ.get('LDU_TEST_DOWNLOAD', 'valid')
asset = arguments[-1].rsplit('/', 1)[-1]
_, system, arch = asset.split('-')
mac = system == 'macos'
if mode == 'fail':
    output.write_bytes(b'partial download')
    raise SystemExit(22)
if mode == 'empty':
    data = b''
elif mode == 'wrong':
    data = b'not an executable'
else:
    if mac:
        header_size = 32
        data = bytearray(struct.pack('<IIIIIIII', 0xfeedfacf,
            0x1000007 if arch == 'x86_64' else 0x100000c, 0, 2, 1, 24, 0, 0))
        data.extend(struct.pack('<IIIIII', 0x32, 24, 1, 0xb0000, 0xb0000, 0))
    else:
        arm = arch == 'armv7'
        header_size, phsize = (52, 32) if arm else (64, 56)
        data = bytearray(header_size + phsize)
        data[:9] = b'\x7fELF' + bytes([1 if arm else 2, 1, 1,
                                      9 if system == 'freebsd' else 0, 0])
        machine = {'x86_64': 62, 'arm64': 183, 'armv7': 40, 'riscv64': 243}[arch]
        struct.pack_into('<HHI', data, 16, 2, machine, 1)
        if arm:
            struct.pack_into('<IIIIHHHHHH', data, 24,
                0, header_size, 0, 0x05000400, header_size, phsize, 1, 0, 0, 0)
            struct.pack_into('<IIIIIIII', data, header_size, 1, 0, 0, 0,
                len(data), len(data), 5, 4096)
        else:
            struct.pack_into('<QQQIHHHHHH', data, 24,
                0, header_size, 0, 5 if arch == 'riscv64' else 0,
                header_size, phsize, 1, 0, 0, 0)
            struct.pack_into('<IIQQQQQQ', data, header_size, 1, 5, 0, 0, 0,
                len(data), len(data), 4096)
    if mode.startswith('truncated-'):
        data = data[:int(mode.split('-')[1])]
    elif mode == 'short-header':
        data = data[:header_size - 1]
    else:
        if mode == 'wrong-architecture':
            struct.pack_into('<I' if mac else '<H', data, 4 if mac else 18,
                (0x100000c if arch == 'x86_64' else 0x1000007) if mac
                else (183 if arch == 'x86_64' else 62))
        elif mode == 'wrong-os':
            if mac:
                data[:4] = b'\x7fELF'
            else:
                data[7] = 0 if system == 'freebsd' else 9
        elif mode == 'wrong-type':
            struct.pack_into('<I' if mac else '<H', data, 12 if mac else 16, 1)
        elif mode == 'wrong-class':
            data[4] = 2 if arch == 'armv7' else 1
        elif mode == 'wrong-endian':
            data[5] = 2
        elif mode == 'wrong-header-size':
            struct.pack_into('<H', data, 40 if arch == 'armv7' else 52, 0)
        elif mode == 'wrong-float-abi':
            struct.pack_into('<I', data, 36 if arch == 'armv7' else 48,
                0x05000200 if arch == 'armv7' else 2)
        data.extend(b'fixture executable and embedded notices')
if mode == 'destination-directory':
    destination = output.parent.parent / 'ldu'
    destination.mkdir()
    (destination / 'sentinel').write_bytes(b'preserve')
output.write_bytes(data)
''')

    def tool(self, name, text):
        path = self.tools / name
        path.write_text(text)
        path.chmod(0o755)

    def install(self, system='Linux', architecture='x86_64', **overrides):
        env = dict(os.environ, HOME=str(self.home), PATH=str(self.tools) + os.pathsep + os.environ['PATH'],
                   LDU_TEST_OS=system, LDU_TEST_ARCH=architecture, LDU_TEST_LOG=str(self.log))
        env.pop('LDU_INSTALL_DIR', None)
        for key, value in overrides.items():
            if value is None:
                env.pop(key, None)
            else:
                env[key] = value
        return subprocess.run(['sh', str(INSTALLER)], cwd=self.work, env=env,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, timeout=10)

    def assert_clean(self, directory=None):
        directory = directory or self.destination
        if directory.exists():
            self.assertEqual(list(directory.glob('.ldu-install.*')), [])

    def test_supported_targets_install_plain_binary_with_0755_and_no_staging_files(self):
        cases = [
            ('Linux', 'x86_64', 'ldu-linux-x86_64'),
            ('Linux', 'aarch64', 'ldu-linux-arm64'),
            ('Linux', 'armv7l', 'ldu-linux-armv7'),
            ('Linux', 'riscv64', 'ldu-linux-riscv64'),
            ('Darwin', 'x86_64', 'ldu-macos-x86_64'),
            ('Darwin', 'arm64', 'ldu-macos-arm64'),
            ('FreeBSD', 'amd64', 'ldu-freebsd-x86_64'),
            ('FreeBSD', 'aarch64', 'ldu-freebsd-arm64'),
        ]
        for system, architecture, asset in cases:
            with self.subTest(asset=asset):
                result = self.install(system, architecture)
                self.assertEqual(result.returncode, 0, result.stderr)
                installed = self.destination / 'ldu'
                self.assertEqual(installed.stat().st_mode & 0o777, 0o755)
                self.assertTrue(installed.read_bytes().endswith(b'embedded notices'))
                self.assertEqual(self.log.read_text(),
                    f'https://github.com/SmallThingz/ldu/releases/download/v0.1.0/{asset}')
                self.assert_clean()

    def test_rosetta_prefers_native_apple_silicon(self):
        result = self.install('Darwin', 'x86_64', LDU_TEST_ROSETTA='1')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(self.log.read_text().endswith('/ldu-macos-arm64'))

    def test_unsupported_target_never_downloads_or_creates_install_directory(self):
        for system, architecture in [('OpenBSD', 'amd64'), ('NetBSD', 'aarch64'),
                                     ('Linux', 'i686'), ('FreeBSD', 'riscv64')]:
            with self.subTest(system=system, architecture=architecture):
                result = self.install(system, architecture)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('no binary for', result.stderr)
                self.assertFalse(self.log.exists())
                self.assertFalse(self.destination.exists())

    def test_failed_empty_or_wrong_download_preserves_previous_install(self):
        self.destination.mkdir(parents=True)
        installed = self.destination / 'ldu'
        installed.write_bytes(b'previous install')
        installed.chmod(0o700)
        for mode in ['fail', 'empty', 'wrong']:
            with self.subTest(mode=mode):
                result = self.install(LDU_TEST_DOWNLOAD=mode)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(installed.read_bytes(), b'previous install')
                self.assertEqual(installed.stat().st_mode & 0o777, 0o700)
                self.assert_clean()

    def test_failed_chmod_preserves_previous_install_and_cleans_download(self):
        self.destination.mkdir(parents=True)
        installed = self.destination / 'ldu'
        installed.write_bytes(b'previous install')
        self.tool('chmod', '#!/bin/sh\nexit 1\n')
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(installed.read_bytes(), b'previous install')
        self.assert_clean()

    def test_failed_header_reader_preserves_previous_install_and_cleans_download(self):
        self.destination.mkdir(parents=True)
        installed = self.destination / 'ldu'
        installed.write_bytes(b'previous install')
        installed.chmod(0o700)
        # Output must not hide the header reader's failure status.
        self.tool('od', '#!/bin/sh\nprintf "7f 45 4c 46\\n"\nexit 1\n')
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(self.log.exists())
        self.assertEqual(installed.read_bytes(), b'previous install')
        self.assertEqual(installed.stat().st_mode & 0o777, 0o700)
        self.assert_clean()

    def test_invalid_headers_preserve_previous_install_on_every_target(self):
        self.destination.mkdir(parents=True)
        installed = self.destination / 'ldu'
        installed.write_bytes(b'previous install')
        installed.chmod(0o700)
        for system, architecture in [
            ('Linux', 'x86_64'), ('Linux', 'aarch64'), ('Linux', 'armv7l'),
            ('Linux', 'riscv64'), ('FreeBSD', 'amd64'), ('FreeBSD', 'aarch64'),
            ('Darwin', 'x86_64'), ('Darwin', 'arm64'),
        ]:
            modes = ['truncated-4', 'truncated-20', 'short-header',
                     'wrong-architecture', 'wrong-os', 'wrong-type']
            if system != 'Darwin':
                modes += ['wrong-class', 'wrong-endian', 'wrong-header-size']
            if architecture in ('armv7l', 'riscv64'):
                modes += ['wrong-float-abi']
            for mode in modes:
                with self.subTest(system=system, architecture=architecture, mode=mode):
                    result = self.install(system, architecture, LDU_TEST_DOWNLOAD=mode)
                    self.assertNotEqual(result.returncode, 0, result.stdout)
                    self.assertIn('invalid header', result.stderr)
                    self.assertEqual(installed.read_bytes(), b'previous install')
                    self.assertEqual(installed.stat().st_mode & 0o777, 0o700)
                    self.assert_clean()

    def test_directory_created_during_download_is_preserved_without_nested_binary(self):
        result = self.install(LDU_TEST_DOWNLOAD='destination-directory')
        self.assertNotEqual(result.returncode, 0)
        destination = self.destination / 'ldu'
        self.assertEqual((destination / 'sentinel').read_bytes(), b'preserve')
        self.assertEqual(sorted(path.name for path in destination.iterdir()), ['sentinel'])
        self.assertNotIn('Installed ldu', result.stdout)
        self.assert_clean()

    def test_directory_at_destination_is_not_overwritten_or_downloaded(self):
        (self.destination / 'ldu').mkdir(parents=True)
        sentinel = self.destination / 'ldu/sentinel'
        sentinel.write_bytes(b'preserve')
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.log.exists())
        self.assertEqual(sentinel.read_bytes(), b'preserve')
        self.assert_clean()

    def test_override_can_be_relative_with_spaces_and_without_home(self):
        result = self.install(HOME=None, LDU_INSTALL_DIR='custom location/bin')
        self.assertEqual(result.returncode, 0, result.stderr)
        custom = self.work / 'custom location/bin'
        self.assertTrue((custom / 'ldu').is_file())
        self.assertFalse(self.destination.exists())
        self.assert_clean(custom)

    def test_replacing_a_symlink_does_not_overwrite_its_target(self):
        self.destination.mkdir(parents=True)
        sentinel = self.work / 'existing-real-file'
        sentinel.write_bytes(b'preserve')
        (self.destination / 'ldu').symlink_to(sentinel)
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.destination / 'ldu').is_symlink())
        self.assertEqual(sentinel.read_bytes(), b'preserve')
        self.assert_clean()


if __name__ == '__main__':
    unittest.main()
