#!/usr/bin/env python3
"""Collect source-package notices using the compiler actually used by makepkg."""
import argparse
import importlib.util
import json
from pathlib import Path
import re
import shutil
import subprocess
import tempfile

SOURCE_COMMIT = '01c2d5f09276ec4a0ab605fc883517efbc61bb07'


def run(*command):
    return subprocess.check_output(command, text=True, timeout=300).strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    root = args.source.resolve()
    spec = importlib.util.spec_from_file_location('ldu_licenses', root / 'scripts/licenses.py')
    notices = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(notices)
    compiler = run('rustc', '-vV')
    version = re.search(r'^release: (.+)$', compiler, re.MULTILINE).group(1)
    if version != '1.99.0':
        raise RuntimeError('This recipe requires Rust 1.99.0: its bundled compiler runtime '
                           'notices were qualified for that release. Update the runtime '
                           'attribution before building with another compiler.')
    target = re.search(r'^host: (.+)$', compiler, re.MULTILINE).group(1)
    sysroot = Path(run('rustc', '--print', 'sysroot'))
    # Rustup keeps the catalog and SPDX texts together. Arch's rust package
    # installs the same files in separate license/documentation directories.
    candidates = [sysroot / 'share/doc/rust', sysroot / 'share/doc/rust/html']
    docs = next((p for p in candidates if (p / 'COPYRIGHT-library.html').is_file()), None)
    arch_catalog = sysroot / 'share/licenses/rust/COPYRIGHT-library.html.rustc'
    arch_texts = sysroot / 'share/doc/rustc/licenses'
    if docs is None and not (arch_catalog.is_file() and arch_texts.is_dir()):
        raise RuntimeError('Rust standard-library notices are missing; reinstall the matching '
                           'Arch rust package, or add rust-docs to the installed rustup 1.99.0 toolchain')
    metadata = json.loads(run('cargo', 'metadata', '--frozen', '--format-version=1',
                              '--filter-platform', target))
    book = notices.NoticeBook()
    notices.add_dependencies(book, metadata, root, SOURCE_COMMIT)
    if docs is not None:
        notices.add_rust_library(book, docs, version)
    else:
        # Adapt only paths; preserve the installed catalog and license bytes.
        # Temporary files stay beside the generated build output, never /tmp.
        with tempfile.TemporaryDirectory(prefix='.ldu-notice-docs-', dir=args.output.parent) as work:
            staged = Path(work)
            shutil.copyfile(arch_catalog, staged / 'COPYRIGHT-library.html')
            shutil.copytree(arch_texts, staged / 'licenses')
            notices.add_rust_library(book, staged, version)
    # GNU libc is supplied by the declared system dependency. Preserve the
    # bundled runtime notices qualified for Rust 1.99.0, guarded above. Exclude
    # other OS CRTs and musl attribution from this GNU distribution build.
    sources = json.loads((root / 'licenses/runtime/sources.json').read_text())
    for name, source in sorted(sources.items()):
        if name.startswith(('musl-', 'freebsd-')):
            continue
        data = (root / 'licenses/runtime' / name).read_bytes()
        import hashlib
        if hashlib.sha256(data).hexdigest() != source['sha256']:
            raise RuntimeError('Compiler runtime notice checksum differs: ' + name)
        book.component(name, [f"Source: {source['url']}"], {name: data})
    args.output.write_bytes(book.render())


if __name__ == '__main__':
    main()
