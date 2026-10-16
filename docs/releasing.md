# Preparing binaries

These commands create local binaries. They do not publish a release or change existing GitHub assets. Each output directory contains only executables; build and smoke reports stay under `.tmp/`. The v0.1.0 installer in `scripts/install.sh` is a local draft: it expects plain binary assets named `ldu-v0.1.0-TARGET` and installs into `~/.local/bin`, or `LDU_INSTALL_DIR` when set. It is not published.

The [release workflow](../.github/workflows/release.yml) still builds the four Linux musl targets. The local packager also accepts macOS and FreeBSD:

| System | Targets | Runtime linking |
| --- | --- | --- |
| Linux | `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `armv7-unknown-linux-musleabihf`, `riscv64gc-unknown-linux-musl` | Static musl |
| macOS | `x86_64-apple-darwin`, `aarch64-apple-darwin` | Apple system libraries |
| FreeBSD | `x86_64-unknown-freebsd`, `aarch64-unknown-freebsd` | FreeBSD base-system libraries |

Runtime qualification uses Linux checks, FreeBSD 14.5 VMs and the [manual native macOS workflow](../.github/workflows/macos-qualification.yml). The macOS workflow runs tests, Clippy, signature verification, package smoke checks, PTY checks and filesystem CLI checks on Intel and Apple Silicon runners; it publishes no binaries. Qualification requires a successful run tied to the source commit and packaged binary hash. A cross-build alone is not runtime evidence. OpenBSD, NetBSD and DragonFly source paths require their own build and runtime checks; no binaries for those systems are prepared here.

For a development build on the host OS, run `cargo build --locked --release` and then `./target/release/ldu .`. Packaging needs the additional platform steps below.

The Rust 1.99.0 release profile uses `opt-level = 3`, full (`fat`) LTO, one codegen unit and symbol stripping. Keep each target's baseline CPU support. Panic unwinding remains enabled so worker guards and preview helpers can clean up after a panic.

Linux x86-64, ARM64 and RISC-V64 use bounded io_uring batches when supported. Ring setup and operation probes select the blocking fallback when unavailable. ARMv7 uses that fallback throughout. macOS and BSD use kqueue for terminal and deletion-completion readiness, with ordinary descriptor waiting if kqueue setup or waiting fails. kqueue does not perform metadata lookup or deletion: that filesystem work remains on the bounded worker threads.

## Linux

For a local x86-64 build:

```sh
rustup toolchain install 1.99.0 --profile minimal
rustup target add --toolchain 1.99.0 x86_64-unknown-linux-musl
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS='-C link-self-contained=yes' \
cargo +1.99.0 build --locked --release --bin ldu --target x86_64-unknown-linux-musl
python3 scripts/release.py package --target x86_64-unknown-linux-musl --output .tmp/release
```

Other linker settings are listed in the workflow. RISC-V additionally needs `-C target-feature=+crt-static`; self-contained linking alone does not select its static runtime. Packaging checks the ELF architecture and rejects a dynamic loader or shared-library dependency.

## macOS

Use the macOS SDK and its supported linker, either on a Mac or in a configured cross toolchain. Unlike ELF, a signed Mach-O executable cannot have an arbitrary notice archive appended. Prepare the notice ZIP first and place it in a real linker section:

```sh
rustup target add --toolchain 1.99.0 aarch64-apple-darwin
mkdir -p .tmp
ldu_notices_dir=$(mktemp -d .tmp/ldu-notices.XXXXXXXX)
python3 scripts/release.py notices --target aarch64-apple-darwin \
  --output "$ldu_notices_dir/notices.zip"
cargo +1.99.0 rustc --locked --release --bin ldu --target aarch64-apple-darwin -- \
  -C "link-arg=-Wl,-sectcreate,__TEXT,__ldu_notices,$ldu_notices_dir/notices.zip"
```

On macOS, sign and verify the resulting file before packaging:

```sh
codesign --force --sign - target/aarch64-apple-darwin/release/ldu
codesign --verify --strict target/aarch64-apple-darwin/release/ldu
python3 scripts/release.py package --target aarch64-apple-darwin --output .tmp/release
```

Replace the target with `x86_64-apple-darwin` for Intel Macs. Use a fresh notice directory for every rebuild: Cargo does not track the external section data, so the new linker argument forces a relink with current notices. Packaging checks the architecture, macOS platform, system-library dependencies and embedded notices, then copies the binary without changing a signed byte. On macOS it also verifies the signature; cross-host packaging records that signature validation and runtime testing remain outstanding. An ad-hoc signature is for local execution and does not provide Developer ID signing or notarization.

Apple documents the section requirement in [Code Signing In Depth](https://developer.apple.com/library/archive/technotes/tn2206/). Neither macOS target supports the packager's UPX option.

## FreeBSD

Native builds use the FreeBSD toolchain:

```sh
rustup target add --toolchain 1.99.0 x86_64-unknown-freebsd
cargo +1.99.0 build --locked --release --bin ldu --target x86_64-unknown-freebsd
python3 scripts/release.py package --target x86_64-unknown-freebsd --output .tmp/release
```

Use `aarch64-unknown-freebsd` on ARM64. Cross-builds need a FreeBSD sysroot and a linker configured to use its startup objects and base libraries; installing the Rust target alone is not sufficient. The bundled startup notices cover FreeBSD 14.5. Use that official sysroot for these packages; if another toolchain substitutes its own CRT sources, review and retain those notices too.

Packaging requires the FreeBSD ELF ABI, the base-system loader and the expected base libraries. It rejects custom library search paths. FreeBSD binaries retain a compressed notice ZIP trailer and are not UPX-packed. A successful cross-link does not test FreeBSD system calls; run the smoke and terminal checks inside FreeBSD.

## Notices and metadata

Every packaged binary carries the project license and applicable third-party notices. Linux and FreeBSD use a ZIP trailer; macOS uses the signed `__TEXT,__ldu_notices` section. Extract either format to a separate ZIP without executing the binary:

```sh
python3 scripts/release.py extract-notices --target aarch64-apple-darwin \
  --binary .tmp/release/ldu-v0.1.0-aarch64-apple-darwin --output .tmp/notices.zip
```

The destination must not already exist. ZIP readers can also inspect the Linux and FreeBSD binaries directly. Crates that omit notice files use only the exact-version fallbacks documented in [licenses/dependencies](../licenses/dependencies/README.md).

Build metadata under `.tmp/` records hashes, source commit, linkage, notice location and whether the checkout has tracked or untracked changes. A dirty candidate is not a pristine build of the recorded commit. Packaging sets `runtime_tested` to false; a separate smoke report identifies the exact tested binary hash.

## Optional Linux compression

Compression is opt-in; the workflow packages unpacked executables. Use an official Linux amd64 or arm64 UPX 5.2.1 distribution, keeping its adjacent license files:

```sh
python3 scripts/release.py package --target x86_64-unknown-linux-musl \
  --output .tmp/release-packed --upx .tmp/tools/upx-5.2.1-amd64_linux/upx
```

The packager verifies the UPX tool checksum, uses NRV2E compression and checks that decompression restores the original executable byte for byte. It then appends notices, verifies the compressed prefix again, and checks every ZIP entry. Compression adds work at launch; qualify startup time, memory use and application behavior before choosing it for a release.

UPX's inspection tool expects its own footer at the end of the file. To inspect or decompress a packaged file, first remove its notice ZIP from a copy:

```sh
python3 - .tmp/release-packed/ldu-v0.1.0-x86_64-unknown-linux-musl .tmp/ldu-packed-prefix <<'PY'
from pathlib import Path
import sys
import zipfile

source, destination = map(Path, sys.argv[1:])
with zipfile.ZipFile(source) as notices:
    prefix_bytes = min(entry.header_offset for entry in notices.infolist())
with destination.open("xb") as output:
    output.write(source.read_bytes()[:prefix_bytes])
PY
.tmp/tools/upx-5.2.1-amd64_linux/upx --test .tmp/ldu-packed-prefix
.tmp/tools/upx-5.2.1-amd64_linux/upx -d --output=.tmp/ldu-unpacked .tmp/ldu-packed-prefix
chmod +x .tmp/ldu-unpacked
```

Keep the original packaged file for its license notices. The extracted prefix and unpacked copy do not contain the notice ZIP.

## Runtime checks

Run `scripts/release.py smoke` inside the target operating system against the final packaged file. It checks version/help output and ncdu-compatible exports with the auto, native and standard backends. Every entry must match its filename, type and own apparent/allocated sizes; backend exports must agree apart from timestamps and entry order.

Linux cross-architecture checks accept an emulator as a JSON argument list, for example `--runner-json '["qemu-riscv64"]'`. That option assumes the same filesystem and OS interface. It is not a remote-host or cross-OS adapter. For a VM or a configured `cross-run` guest, stage the repository and packaged binary, then execute the smoke script inside the guest so its fixtures and `lstat` reference use that guest's filesystem. A launcher that only starts the executable without forwarding arguments and exit status cannot run this qualification.

Terminal interaction, deletion, cancellation and idle behavior also need the project's PTY qualification before release. Record whether the Linux qualification kernel permits io_uring: a passing fallback run does not exercise live ring operations. On macOS and BSD, exercise kqueue readiness and its descriptor-wait fallback. Cross-compilation and header inspection alone provide no runtime evidence.
