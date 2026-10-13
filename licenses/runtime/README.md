# Runtime license notices

These notices accompany the Rust 1.99.0 standard library and statically linked runtimes. `sources.json` records each original URL and SHA-256. Standalone license files are copied without changes; FreeBSD source files contribute their complete leading notice comments.

- Rust compiler-builtins and its libm code come from the Rust 1.99.0 source tree.
- LLVM runtime notices come from `rust-lang/llvm-project` commit `1b9c0d5ff9bbe7634aead059efe6b11a7eeba145`, the submodule pinned by Rust 1.99.0.
- FreeBSD startup-object notices come from 14.5-RELEASE, commit `4099b5c828800776fda3eccd6537ceed638a2a8b`, covering common, amd64 and aarch64 `lib/csu` sources. Each entry records both the original source hash and extracted notice hash. The aarch64 `crt.h` is an empty compatibility header without code or a license notice.
- Static Linux builds use musl 1.2.5, as pinned by Rust 1.99.0's `src/ci/docker/scripts/musl.sh`; Rust applies additional security patches during its toolchain build.

The packaging script also copies `COPYRIGHT-library.html` and the license texts from the installed, pinned Rust toolchain's `share/doc/rust` directory. Cargo package notices and source links are collected separately. A notice included here does not imply that every build links every runtime.

Runtime notices are filtered by target: musl appears only in Linux packages, FreeBSD startup notices only in FreeBSD packages. macOS links Apple system libraries rather than redistributing them. The Rust runtime catalog remains conservative across all three systems. Cross-linkers that substitute a different CRT must supply its applicable notices before packaging.
