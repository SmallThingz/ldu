"""Regression coverage for release attribution selection and preservation."""

import hashlib
import json
from pathlib import Path
import shutil
import tempfile
import unittest
from unittest.mock import patch

import licenses


class NoticeTests(unittest.TestCase):
    def test_normal_graph_omits_dev_build_and_unreachable_workspace_members(self):
        kinds = {"root": [("normal", None), ("dev", "dev"), ("build", "build")],
                 "normal": [("shared", "build"), ("shared", None)],
                 "dev": [("dev-child", None)], "build": [], "shared": [],
                 "dev-child": [], "unused": []}
        metadata = {"packages": [dict(id=name, name=name, version="1.0") for name in kinds],
                    "resolve": {"root": "root", "nodes": [
                        {"id": name, "deps": [{"pkg": dependency,
                                               "dep_kinds": [{"kind": kind}]}
                                              for dependency, kind in edges]}
                        for name, edges in kinds.items()]}}
        self.assertEqual([package["name"] for package in licenses.normal_packages(metadata)],
                         ["normal", "shared"])

    def test_incomplete_graph_fails_closed(self):
        with self.assertRaisesRegex(RuntimeError, "Incomplete"):
            licenses.normal_packages({"packages": [], "resolve": {"root": "missing", "nodes": []}})

    def test_mit_alternative_keeps_copyright_and_third_party_notices(self):
        notices = {"LICENSE-MIT": b"MIT with Alice copyright", "LICENSE-APACHE": b"Apache",
                   "COPYRIGHT": b"Bob copyright", "NOTICE": b"Attribution",
                   "LICENSE-THIRD-PARTY": b"Third party", "licenses/LICENSE-APACHE": b"Nested"}
        chosen, texts = licenses.select_notices("Apache-2.0 OR MIT", notices)
        self.assertIn("MIT (upstream:", chosen)
        self.assertEqual(set(texts), set(notices) - {"LICENSE-APACHE"})

    def test_conjunctive_and_unknown_licenses_are_preserved(self):
        notices = {"LICENSE-MIT": b"MIT", "LICENSE-APACHE": b"Apache",
                   "LICENSE-UNICODE": b"Unicode copyright"}
        for expression in ["(MIT OR Apache-2.0) AND Unicode-3.0", "BSD-2-Clause AND MIT",
                           "MIT-like OR Apache-2.0", None]:
            with self.subTest(expression=expression):
                self.assertEqual(licenses.select_notices(expression, notices)[1], notices)

    def test_no_mit_file_means_no_alternative_is_removed(self):
        notices = {"LICENSE": b"Combined conditions", "LICENSE-APACHE": b"Apache"}
        self.assertEqual(licenses.select_notices("MIT OR Apache-2.0", notices)[1], notices)

    def test_deduplication_preserves_distinct_authors_and_complete_bytes(self):
        book = licenses.NoticeBook()
        alice = b"Copyright Alice\n\nConditions unchanged.\n"
        bob = b"Copyright Bob\n\nConditions unchanged.\n"
        book.component("one", ["Source: https://example.test/one"], {"LICENSE": alice})
        book.component("two", [], {"LICENSE": alice, "COPYRIGHT": bob})
        rendered = book.render()
        self.assertEqual(rendered.count(alice), 1)
        self.assertEqual(rendered.count(bob), 1)
        self.assertEqual(len(book.texts), 2)
        self.assertIn(b"Source: https://example.test/one", rendered)
        self.assertIn(b"Source Code Form", rendered)

    def pinned_fixture(self, root, identifier="compio-driver-0.12.5"):
        bundled = root / "licenses/dependencies"
        shutil.copytree(Path(__file__).resolve().parents[1] / "licenses/dependencies", bundled)
        source = json.loads((bundled / "sources.json").read_text())[identifier]
        name, version = identifier.rsplit("-", 1)
        base = root / "package"
        base.mkdir()
        (base / "Cargo.toml").write_text("")
        (base / ".cargo_vcs_info.json").write_text(json.dumps(source["vcs"]))
        (root / "Cargo.lock").write_text(
            f'[[package]]\nname = "{name}"\nversion = "{version}"\n'
            f'source = "{source["registry_source"]}"\n'
            f'checksum = "{source["registry_sha256"]}"\n')
        return {"name": name, "version": version, "license": source["license"],
                "source": source["registry_source"], "manifest_path": str(base / "Cargo.toml")}, source

    def test_pinned_upstream_notice_preserves_exact_bytes_and_commit_url(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            package, source = self.pinned_fixture(root)
            expression, notices = licenses.package_notices(package, root)
            self.assertEqual(expression, "MIT")
            self.assertEqual(list(notices.values()),
                             [(root / "licenses/dependencies" / source["file"]).read_bytes()])
            self.assertIn(source["vcs"]["git"]["sha1"], next(iter(notices)))
            self.assertNotIn("template", next(iter(notices)))

    def test_canonical_template_is_not_presented_as_an_upstream_notice(self):
        for identifier in ["mod_use-0.2.3", "synchrony-0.1.9"]:
            with self.subTest(identifier=identifier), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                package, source = self.pinned_fixture(root, identifier)
                expression, notices = licenses.package_notices(package, root)
                book = licenses.NoticeBook()
                book.component(identifier, [f"License: {expression}"], notices)
                rendered = book.render().decode()
                self.assertIn("Canonical SPDX MIT template, not an upstream notice", rendered)
                self.assertIn(source["declaration"], rendered)
                self.assertIn(source["declaration_url"], rendered)
                self.assertIn("<year>", rendered)
                self.assertIn("<copyright holders>", rendered)

    def test_pinned_fallback_rejects_unknown_or_changed_package_identity(self):
        changes = [("name", "unreviewed"), ("version", "99.0.0"),
                   ("license", "MIT OR Apache-2.0"), ("source", "git+https://example.test/fork")]
        for field, value in changes:
            with self.subTest(field=field), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                package, _ = self.pinned_fixture(root)
                package[field] = value
                with self.assertRaises(RuntimeError):
                    licenses.package_notices(package, root)

    def test_pinned_fallback_rejects_changed_registry_checksum_commit_and_notice(self):
        for changed in ["checksum", "commit", "notice"]:
            with self.subTest(changed=changed), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                package, source = self.pinned_fixture(root)
                if changed == "checksum":
                    path = root / "Cargo.lock"
                    path.write_text(path.read_text().replace(source["registry_sha256"], "0" * 64))
                elif changed == "commit":
                    path = Path(package["manifest_path"]).parent / ".cargo_vcs_info.json"
                    value = json.loads(path.read_text())
                    value["git"]["sha1"] = "0" * 40
                    path.write_text(json.dumps(value))
                else:
                    (root / "licenses/dependencies" / source["file"]).write_text("altered")
                with self.assertRaisesRegex(RuntimeError, "mismatch"):
                    licenses.package_notices(package, root)

    def test_rust_catalog_preserves_authorship_links_and_decoded_notice_bodies(self):
        document = '''<html><head><title>Hidden title</title></head><body>
<h2 id="in-tree-files">In-tree</h2><p><b>License:</b> MIT</p>
<p>Copyright A &amp; B</p><a href="https://example.test/source">Source</a>
<h2 id="out-of-tree-dependencies">Dependencies</h2><h3>dep-1</h3>
<p>Author C &lt;c@example.test&gt;</p><pre>  Original &amp; unchanged\n notice.\n</pre>
<h3>dep-2</h3><pre>  Original &amp; unchanged\n notice.\n</pre></body></html>'''
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "licenses").mkdir()
            (root / "licenses/MIT.txt").write_text("MIT full text\n")
            (root / "licenses/GPL-3.0-or-later.txt").write_text("UNRELATED COMPILER LICENSE")
            (root / "COPYRIGHT-library.html").write_text(document)
            book = licenses.NoticeBook()
            licenses.add_rust_library(book, root, "1.99.0")
            output = book.render()
            for value in [b"Copyright A & B", b"Author C <c@example.test>",
                          b"https://example.test/source", b"MIT full text"]:
                self.assertIn(value, output)
            self.assertEqual(output.count(b"  Original & unchanged\n notice.\n"), 1)
            self.assertNotIn(b"UNRELATED COMPILER LICENSE", output)
            self.assertNotIn(b"Hidden title", output)

    def test_missing_rust_in_tree_license_fails_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "COPYRIGHT-library.html").write_text('''<h2 id="in-tree-files"></h2>
<p><b>License:</b> Unknown-Required</p><h2 id="out-of-tree-dependencies"></h2>
<pre>Preserved body</pre>''')
            with self.assertRaisesRegex(RuntimeError, "Missing Rust in-tree license"):
                licenses.add_rust_library(licenses.NoticeBook(), root, "1.99.0")

    def test_runtime_provenance_mismatch_fails_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bundled = root / "licenses/runtime"
            bundled.mkdir(parents=True)
            (bundled / "musl-COPYRIGHT.txt").write_bytes(b"tampered")
            (bundled / "sources.json").write_text(json.dumps({"musl-COPYRIGHT.txt": {
                "url": "https://example.test/musl", "sha256": hashlib.sha256(b"expected").hexdigest()}}))
            with self.assertRaisesRegex(RuntimeError, "checksum mismatch"):
                licenses.add_runtime(licenses.NoticeBook(), root, "x86_64-unknown-linux-musl")

    def test_unsupported_runtime_is_rejected(self):
        with self.assertRaisesRegex(RuntimeError, "Linux musl, FreeBSD or macOS"):
            licenses.add_runtime(licenses.NoticeBook(), Path("."), "x86_64-unknown-linux-gnu")


    def test_runtime_notices_follow_the_target_platform(self):
        root = Path(__file__).resolve().parents[1]
        for target, wanted, unwanted in [
            ("x86_64-unknown-linux-musl", b"musl-COPYRIGHT.txt", b"freebsd-14.5-"),
            ("aarch64-unknown-freebsd", b"freebsd-14.5-", b"musl-COPYRIGHT.txt"),
            ("aarch64-apple-darwin", b"compiler-builtins-LICENSE.txt", b"musl-COPYRIGHT.txt"),
        ]:
            with self.subTest(target=target):
                book = licenses.NoticeBook()
                licenses.add_runtime(book, root, target)
                rendered = book.render()
                self.assertIn(wanted, rendered)
                self.assertNotIn(unwanted, rendered)
                if "apple" in target:
                    self.assertNotIn(b"freebsd-14.5-", rendered)

    def test_upx_preserves_exception_license_and_current_authors_only_when_requested(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            texts = {"LICENSE": b"Official special exception", "COPYING": b"Complete GPL terms",
                     "README": (b"Unneeded documentation\nCOPYRIGHT\n=========\n"
                                b"Copyright (C) Current Authors\nWarranty text\n\nShare and enjoy,\nEnd")}
            for name, text in texts.items():
                (root / name).write_bytes(text)
            expected = {name: hashlib.sha256(text).hexdigest() for name, text in texts.items()}
            with patch.object(licenses, "UPX_NOTICE_HASHES", expected):
                output = licenses.collect_upx_licenses(root / "upx")["licenses/UPX-NOTICES.txt"]
                for text in [texts["LICENSE"], texts["COPYING"], b"Copyright (C) Current Authors",
                             b"Warranty text"]:
                    self.assertIn(text, output)
                self.assertNotIn(b"Unneeded documentation", output)
                (root / "LICENSE").write_bytes(b"Altered exception")
                with self.assertRaisesRegex(RuntimeError, "checksum mismatch"):
                    licenses.collect_upx_licenses(root / "upx")


if __name__ == "__main__":
    unittest.main()
