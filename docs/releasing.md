# Preparing Linux binaries

These commands create local binaries. They do not publish a release or change existing GitHub assets. The musl-only pipeline changes in this checkout remain unpublished.

The [release workflow](../.github/workflows/release.yml) builds x86-64, ARM64, ARMv7 and RISC-V with Rust 1.99.0. All four targets use static musl linking. Packaging checks the ELF architecture and rejects executables that require a dynamic loader or shared libraries.

The release profile uses `opt-level = 3`, full (`fat`) LTO, one codegen unit and symbol stripping. Builds keep each target's baseline CPU support. Panic unwinding remains enabled so worker guards and preview helpers can clean up after a panic.

io_uring support is built for x86-64, ARM64 and RISC-V64. Ring setup and operation probes select the blocking fallback when unavailable. ARMv7 builds use that fallback throughout; the current ring bindings do not support the target.

For a local x86-64 build:

```sh
rustup toolchain install 1.99.0 --profile minimal
rustup target add --toolchain 1.99.0 x86_64-unknown-linux-musl
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS='-C link-self-contained=yes' \
cargo +1.99.0 build --locked --release --bin ldu --target x86_64-unknown-linux-musl
python3 scripts/release.py package --target x86_64-unknown-linux-musl --output .tmp/release
```

Other targets and linker settings are listed in the workflow. RISC-V additionally needs `-C target-feature=+crt-static`; self-contained linking alone does not select its static runtime.

Packaging appends a ZIP containing the project license and applicable third-party notices. ZIP readers can inspect these notices without running the executable. Crates that omit notice files use only the exact-version fallbacks documented in [licenses/dependencies](../licenses/dependencies/README.md). Build metadata stays under `.tmp/`, including hashes, source commit and whether the checkout has tracked or untracked changes. A dirty candidate is not a pristine build of the recorded commit.

## Optional executable compression

Compression is opt-in; the workflow currently packages unpacked executables. Use an official Linux amd64 or arm64 UPX 5.2.1 distribution, keeping its adjacent license files:

```sh
python3 scripts/release.py package --target x86_64-unknown-linux-musl \
  --output .tmp/release-packed --upx /path/to/upx-5.2.1-amd64_linux/upx
```

The packager verifies the UPX tool checksum, uses NRV2E compression and checks that decompression restores the original executable byte for byte. It then appends notices, verifies the compressed prefix again, and checks every ZIP entry. The executable remains directly runnable. Compression adds work at launch; qualify startup time, memory use and application behavior before choosing it for a release.

UPX's inspection tool expects its own footer at the end of the file. The appended notice ZIP therefore needs to be removed from a copy before `upx --test` or `upx -d`. To recover the unchanged packed prefix:

```sh
python3 - .tmp/release-packed/ldu-v0.1.1-x86_64-unknown-linux-musl .tmp/ldu-packed-prefix <<'PY'
from pathlib import Path
import sys
import zipfile

source, destination = map(Path, sys.argv[1:])
with zipfile.ZipFile(source) as notices:
    prefix_bytes = min(entry.header_offset for entry in notices.infolist())
with destination.open("xb") as output:
    output.write(source.read_bytes()[:prefix_bytes])
PY
/path/to/upx-5.2.1-amd64_linux/upx --test .tmp/ldu-packed-prefix
/path/to/upx-5.2.1-amd64_linux/upx -d --output=.tmp/ldu-unpacked .tmp/ldu-packed-prefix
chmod +x .tmp/ldu-unpacked
```

Keep the original packaged file for its license notices. The extracted prefix and unpacked copy do not contain the notice ZIP.

## Smoke checks

Run `scripts/release.py smoke` against the final packaged file, not just the build output. It checks version/help output and ncdu-compatible JSON exports with the auto, native and standard backends. Every exported entry must match its filename, type and own apparent/allocated sizes; backend exports must agree apart from timestamps and entry order. Cross-architecture checks accept an emulator as a JSON argument list, for example `--runner-json '["/path/to/qemu-riscv64"]'` for a static RISC-V binary. Terminal interaction, deletion, cancellation and idle behavior also need the project's PTY qualification before release. Record whether the qualification kernel permits io_uring: a passing fallback run does not exercise live ring operations. Check ring and blocking paths on a capable host before comparing their throughput.
