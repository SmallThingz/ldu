"""Collect release notices without bundling compiler documentation or test crates.

The Cargo graph is filtered to the release target and normal dependency edges.
Procedural macro dependencies are retained conservatively because they generate
code. Rust's own standard-library attribution catalog is also kept intact: it
covers implementation details that cannot safely be inferred from binary symbols.
Only byte-identical license bodies are shared; package/source associations and
all copyright, exception and third-party notices remain in the readable catalog.
"""

import hashlib
import html
from html.parser import HTMLParser
import json
from pathlib import Path
import re
import tomllib


NOTICE_PREFIXES = ("LICENSE", "LICENCE", "COPYING", "NOTICE", "COPYRIGHT")


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


class NoticeBook:
    def __init__(self):
        self.sections = []
        self.texts = {}

    def reference(self, content):
        # Preserve the complete text, including copyright lines and exceptions.
        text = content.decode("utf-8") if isinstance(content, bytes) else content
        require(text.strip(), "Empty license notice")
        if text not in self.texts:
            self.texts[text] = f"T{len(self.texts) + 1:03}"
        return self.texts[text]

    def component(self, title, metadata, notices):
        require(notices, f"No license notices for {title}")
        lines = [f"## {title}", *metadata]
        for name, content in sorted(notices.items()):
            lines.append(f"{name}: [{self.reference(content)}]")
        self.sections.append("\n".join(lines))

    def render(self):
        header = (
            "THIRD-PARTY NOTICES\n\n"
            "Identical notice texts are printed once below and referenced by ID.\n"
            "Each component identifies its version, source and applicable texts.\n"
            "Where stated, MIT is selected from the upstream license alternatives.\n"
            "For MPL-covered components, the linked source archive supplies the\n"
            "corresponding Source Code Form, available under the stated MPL terms.\n"
        )
        texts = [f"===== [{identifier}] =====\n{text}"
                 for text, identifier in self.texts.items()]
        return (header + "\n" + "\n\n".join(self.sections + texts) + "\n").encode("utf-8")


def normal_packages(metadata):
    """Walk normal edges from this package, excluding dev/build-only branches."""
    resolve = metadata.get("resolve") or {}
    root = resolve.get("root")
    require(root, "Cargo metadata has no release package root")
    nodes = {node["id"]: node for node in resolve["nodes"]}
    packages = {package["id"]: package for package in metadata["packages"]}
    pending, visited = [root], set()
    while pending:
        current = pending.pop()
        if current in visited:
            continue
        require(current in nodes and current in packages, "Incomplete Cargo dependency graph")
        visited.add(current)
        for dependency in nodes[current]["deps"]:
            if any(kind["kind"] is None for kind in dependency["dep_kinds"]):
                pending.append(dependency["pkg"])
    return sorted((packages[key] for key in visited if key != root),
                  key=lambda package: (package["name"], package["version"], package["id"]))


def select_notices(expression, notices):
    """Select MIT only for a plain disjunction with a standalone MIT option.

    Conjunctions, unknown expressions and extra files remain untouched. In
    particular, Unicode and third-party licenses must never be dropped just
    because the package also offers its own code under MIT.
    """
    mit_names = [name for name in notices if Path(name).name.upper() in
                 {"LICENSE-MIT", "LICENSE-MIT.TXT", "LICENCE-MIT"}]
    alternatives = re.split(r"\s+OR\s+|\s*/\s*", expression or "")
    select_mit = bool(mit_names) and "MIT" in alternatives and not any(
        token in (expression or "") for token in (" AND ", "(", ")"))
    if not select_mit:
        return expression or "see notices", notices
    # Only omit specifically named alternative license texts. Generic LICENSE,
    # COPYING, COPYRIGHT, NOTICE and nested third-party files are all retained.
    alternatives_to_omit = {
        "LICENSE-APACHE", "LICENSE-APACHE.TXT", "LICENSE-APACHE-2.0_WITH_LLVM-EXCEPTION",
        "UNLICENSE", "LICENSE-UNLICENSE",
    }
    chosen = {name: content for name, content in notices.items()
              if "/" in name or Path(name).name.upper() not in alternatives_to_omit}
    return f"MIT (upstream: {expression})", chosen


def pinned_package_notices(package, base, root):
    """Use reviewed notices only for exact registry releases missing their own."""
    identifier = f"{package['name']}-{package['version']}"
    bundled = root / "licenses" / "dependencies"
    sources = json.loads((bundled / "sources.json").read_text(encoding="utf-8"))
    source = sources.get(identifier)
    require(source, f"No license text found for {identifier}")
    require(package.get("source") == source["registry_source"],
            f"Pinned license registry mismatch: {identifier}")
    require(package.get("license") == source["license"],
            f"Pinned license expression mismatch: {identifier}")
    lock = tomllib.loads((root / "Cargo.lock").read_text(encoding="utf-8"))
    locked = [entry for entry in lock["package"]
              if entry["name"] == package["name"] and entry["version"] == package["version"]
              and entry.get("source") == source["registry_source"]]
    require(len(locked) == 1 and locked[0].get("checksum") == source["registry_sha256"],
            f"Pinned license registry checksum mismatch: {identifier}")
    vcs = json.loads((base / ".cargo_vcs_info.json").read_text(encoding="utf-8"))
    require(vcs == source["vcs"], f"Pinned license source commit mismatch: {identifier}")
    name = source["file"]
    require(Path(name).name == name, "Invalid dependency notice filename")
    content = (bundled / name).read_bytes()
    require(hashlib.sha256(content).hexdigest() == source["sha256"],
            f"Dependency notice checksum mismatch: {identifier}")
    if source["kind"] == "upstream":
        return {f"Upstream license ({source['url']})": content}
    require(source["kind"] == "spdx-template", "Unknown pinned license source kind")
    return {
        f"Upstream license declaration ({source['declaration_url']})": source["declaration"],
        f"Canonical SPDX MIT template, not an upstream notice ({source['url']})": content,
    }


def package_notices(package, root=None):
    base = Path(package["manifest_path"]).parent
    candidates = set()
    for path in base.iterdir():
        if path.name.upper().startswith(NOTICE_PREFIXES):
            if path.is_file():
                candidates.add(path)
            elif path.is_dir():
                candidates.update(child for child in path.rglob("*") if child.is_file())
    if package.get("license_file"):
        declared = Path(package["license_file"])
        candidates.add(declared if declared.is_absolute() else base / declared)
    notices = {}
    for path in sorted(candidates):
        require(path.is_file(), f"Missing license for {package['name']}: {path}")
        # Do not silently package unrelated files outside the dependency tree.
        require(path.resolve().is_relative_to(base.resolve()),
                f"License outside package directory: {path}")
        notices[path.relative_to(base).as_posix()] = path.read_bytes()
    if not notices and root is not None:
        notices = pinned_package_notices(package, base, root)
    require(notices, f"No license text found for {package['name']}-{package['version']}")
    return select_notices(package.get("license"), notices)


def add_dependencies(book, metadata, root, commit):
    for package in normal_packages(metadata):
        identifier = f"{package['name']}-{package['version']}"
        require(re.fullmatch(r"[A-Za-z0-9_.+-]+", identifier), "Invalid package identifier")
        source = package.get("source")
        if source and source.startswith("registry+"):
            source = (f"https://crates.io/api/v1/crates/{package['name']}/"
                      f"{package['version']}/download")
        elif not source:
            base = Path(package["manifest_path"]).parent.resolve()
            require(base.is_relative_to(root.resolve()), "Local dependency is outside repository")
            relative = base.relative_to(root.resolve()).as_posix()
            source = f"https://github.com/SmallThingz/ldu/tree/{commit}/{relative}"
        license_name, notices = package_notices(package, root)
        book.component(identifier, [f"License: {license_name}", f"Source: {source}"], notices)


class ReadableHTML(HTMLParser):
    """Render the pinned Rust notice document without retaining its markup."""
    BLOCKS = {"html", "head", "body", "h1", "h2", "h3", "p", "div", "li", "ul",
              "details", "summary", "br", "pre"}

    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.parts = []
        self.ignored = 0
        self.links = []

    def handle_starttag(self, tag, attrs):
        if tag in {"head", "script", "style"}:
            self.ignored += 1
        if self.ignored:
            return
        if tag in self.BLOCKS:
            self.parts.append("\n")
        if tag == "a":
            self.links.append(dict(attrs).get("href", ""))

    def handle_endtag(self, tag):
        if tag in {"head", "script", "style"}:
            self.ignored -= 1
            return
        if self.ignored:
            return
        if tag == "a":
            require(self.links, "Malformed link in Rust notices")
            link = self.links.pop()
            if link and not link.startswith("#"):
                self.parts.append(f" <{link}>")
        if tag in self.BLOCKS:
            self.parts.append("\n")

    def handle_data(self, data):
        if not self.ignored:
            self.parts.append(data)

    def text(self):
        # HTML collapses whitespace outside preformatted license bodies. Those
        # bodies are extracted verbatim before this renderer is called.
        return "\n".join(" ".join(line.split()) for line in "".join(self.parts).splitlines()
                         if line.strip())


def add_rust_library(book, base, toolchain):
    copyright_file = base / "COPYRIGHT-library.html"
    require(copyright_file.is_file(), "Rust toolchain is missing its standard-library notices")
    document = copyright_file.read_text(encoding="utf-8")
    require('id="in-tree-files"' in document and 'id="out-of-tree-dependencies"' in document,
            "Unknown Rust standard-library notice format")
    count = 0

    def replace_text(match):
        nonlocal count
        count += 1
        text = html.unescape(match.group(1))
        return f"<p>License text: [{book.reference(text)}]</p>"

    readable = re.sub(r"<pre>(.*?)</pre>", replace_text, document, flags=re.DOTALL)
    require(count and "<pre" not in readable, "Unrecognized Rust license body markup")
    parser = ReadableHTML()
    parser.feed(readable)
    parser.close()
    book.sections.append(
        f"## Rust {toolchain} standard library\n"
        f"Source: https://github.com/rust-lang/rust/tree/{toolchain}/library\n"
        "The upstream attribution catalog below is preserved conservatively; it\n"
        "also names alternate platforms and build-time implementation components.\n"
        + parser.text())

    # In-tree notices refer to SPDX texts instead of embedding them. Derive the
    # required set from that section, so compiler/doc/font licenses do not leak
    # into the application merely because rust-docs installs them together.
    in_tree = document.split('id="out-of-tree-dependencies"', 1)[0]
    expressions = re.findall(r"<p><b>License:</b>\s*(.*?)</p>", in_tree, flags=re.DOTALL)
    require(expressions, "Rust in-tree license expressions are missing")
    identifiers = set()
    for expression in expressions:
        expression = html.unescape(expression)
        require("<" not in expression, "Unexpected markup in Rust license expression")
        identifiers.update(re.findall(r"[A-Za-z0-9][A-Za-z0-9.+-]*", expression))
    identifiers.difference_update({"AND", "OR", "WITH"})
    texts = {}
    for identifier in sorted(identifiers):
        path = base / "licenses" / f"{identifier}.txt"
        require(path.is_file(), f"Missing Rust in-tree license: {identifier}")
        texts[f"{identifier}.txt"] = path.read_bytes()
    book.component(f"Rust {toolchain} in-tree license texts", [], texts)


def add_runtime(book, root, target):
    if "-linux-musl" in target:
        system = "linux-musl"
    elif target.endswith("-unknown-freebsd"):
        system = "freebsd"
    elif target.endswith("-apple-darwin"):
        system = "darwin"
    else:
        raise RuntimeError("Release notices require a Linux musl, FreeBSD or macOS target")
    bundled = root / "licenses" / "runtime"
    sources = json.loads((bundled / "sources.json").read_text(encoding="utf-8"))
    if system == "linux-musl":
        require("musl-COPYRIGHT.txt" in sources, "Missing musl notice provenance")
    elif system == "freebsd":
        require(any(name.startswith("freebsd-14.5-") for name in sources),
                "Missing FreeBSD startup notice provenance")
    for name, source in sorted(sources.items()):
        platforms = source.get("platforms")
        # Older inventories predate target filters; musl was their only OS runtime.
        if platforms is None and name == "musl-COPYRIGHT.txt":
            platforms = ["linux-musl"]
        if platforms is not None and system not in platforms:
            continue
        require(Path(name).name == name, "Invalid runtime notice filename")
        content = (bundled / name).read_bytes()
        require(hashlib.sha256(content).hexdigest() == source["sha256"],
                f"Runtime notice checksum mismatch: {name}")
        book.component(name, [f"Source: {source['url']}"], {name: content})


def collect_licenses(root, toolchain, target, run):
    """Return a single jointly compressible text entry for the executable ZIP."""
    root = Path(root)
    metadata = json.loads(run([
        "cargo", f"+{toolchain}", "metadata", "--locked", "--format-version=1",
        "--filter-platform", target,
    ], timeout=300))
    book = NoticeBook()
    add_dependencies(book, metadata, root, run(["git", "rev-parse", "HEAD"]))
    sysroot = Path(run(["rustc", f"+{toolchain}", "--print", "sysroot"]))
    add_rust_library(book, sysroot / "share" / "doc" / "rust", toolchain)
    add_runtime(book, root, target)
    return {"licenses/THIRD-PARTY-NOTICES.txt": book.render()}


# The two official Linux host archives contain identical notice files. Pin the
# notices as well as the compressor (release.py) so a replaced tool directory
# cannot silently alter the exception or strip its authorship.
UPX_NOTICE_HASHES = {
    "LICENSE": "905d4a66d5324e154d976bbf88829d4d9263815a2b97cc40954f06d4a23c28eb",
    "COPYING": "8177f97513213526df2cf6184d8ff986c675afb514d4e68a404010521b880643",
    "README": "04085613a1d16c859a8557b069fc72babd2a94fdc237446f606397f9a6c68f2b",
}


def collect_upx_licenses(upx_path):
    """Include the official 5.2.1 stub's GPL terms and executable exception.

    Called only when the executable is packed with the pinned, unmodified UPX
    binary. The compressor itself and its unrelated dependencies are not shipped.
    """
    directory = Path(upx_path).resolve().parent
    notices = {}
    for name, expected in UPX_NOTICE_HASHES.items():
        content = (directory / name).read_bytes()
        require(hashlib.sha256(content).hexdigest() == expected,
                f"UPX 5.2.1 notice checksum mismatch: {name}")
        notices[name] = content
    parts = notices["README"].split(b"COPYRIGHT\n=========\n", 1)
    require(len(parts) == 2, "Missing UPX current copyright notice")
    copyright = parts[1].split(b"\n\nShare and enjoy,", 1)[0]
    require(b"Copyright (C)" in copyright, "Empty UPX copyright notice")
    header = (
        b"UPX 5.2.1 executable decompressor stub\n"
        b"Source: https://github.com/upx/upx/releases/tag/v5.2.1\n"
        b"GPL v2 or later with the special exception for compressed executables\n"
        b"from the official unmodified UPX distribution.\n\n"
    )
    return {"licenses/UPX-NOTICES.txt": (
        header + copyright + b"\n\n===== LICENSE AND SPECIAL EXCEPTION =====\n"
        + notices["LICENSE"] + b"\n\n===== GNU GENERAL PUBLIC LICENSE =====\n"
        + notices["COPYING"])}
