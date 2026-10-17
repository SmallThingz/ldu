#!/usr/bin/env python3
"""Collect source-package notices using the compiler actually used by makepkg."""
import argparse
import importlib.util
import json
from pathlib import Path
import re
import hashlib
import subprocess

SOURCE_COMMIT = '01c2d5f09276ec4a0ab605fc883517efbc61bb07'


def run(*command):
    return subprocess.check_output(command, text=True, timeout=300).strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--rust-notices', required=True, type=Path)
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
    commit = re.search(r'^commit-hash: (.+)$', compiler, re.MULTILINE).group(1)
    if commit != 'b940084d7eb6a299eb4bfeb8e34901bc051e7ac4':
        raise RuntimeError('Rust compiler source commit differs from the qualified 1.99.0 notices')
    target = re.search(r'^host: (.+)$', compiler, re.MULTILINE).group(1)
    sysroot = Path(run('rustc', '--print', 'sysroot'))
    # Rustup supplies a complete catalog. Arch splits the installed files and
    # its 1.99.0 catalog omits out-of-tree dependency entries.
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
        # Preserve actual installed attribution bytes, including every SPDX text.
        installed = {'COPYRIGHT-library.html.rustc': arch_catalog.read_bytes()}
        installed.update({p.name: p.read_bytes() for p in sorted(arch_texts.glob('*.txt'))})
        if len(installed) == 1 or any(not data for data in installed.values()):
            raise RuntimeError('Installed Arch Rust license texts are missing or empty')
        book.component('Installed Arch Rust 1.99.0 attribution catalog',
                       ['Source: installed matching Arch rust package; raw catalog and SPDX texts'],
                       installed)
        # The checksum-pinned official catalog supplies dependency notices that
        # Arch omits. Exact source-commit equality is checked above; alternate
        # platforms/build components remain conservatively included.
        supplemental = args.rust_notices.resolve()
        provenance = json.loads((supplemental / 'provenance.json').read_text())
        if (provenance['compiler_release'], provenance['compiler_commit']) != (version, commit):
            raise RuntimeError('Supplemental Rust notice source identity differs')
        for name, expected in provenance['files'].items():
            if hashlib.sha256((supplemental / name).read_bytes()).hexdigest() != expected:
                raise RuntimeError('Supplemental Rust notice checksum differs: ' + name)
        book.sections.append('## Supplemental upstream Rust standard-library attribution\n'
                             + provenance['origin'] + '\nSource: ' + provenance['upstream'])
        notices.add_rust_library(book, supplemental, version)
    # GNU libc is supplied by the declared system dependency. Preserve the
    # bundled runtime notices qualified for Rust 1.99.0, guarded above. Exclude
    # other OS CRTs and musl attribution from this GNU distribution build.
    sources = json.loads((root / 'licenses/runtime/sources.json').read_text())
    for name, source in sorted(sources.items()):
        if name.startswith(('musl-', 'freebsd-')):
            continue
        data = (root / 'licenses/runtime' / name).read_bytes()
        if hashlib.sha256(data).hexdigest() != source['sha256']:
            raise RuntimeError('Compiler runtime notice checksum differs: ' + name)
        book.component(name, [f"Source: {source['url']}"], {name: data})
    args.output.write_bytes(book.render())


if __name__ == '__main__':
    main()
