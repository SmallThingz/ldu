# Runtime license notices

These notices accompany the Rust 1.99.0 standard library and statically linked runtimes. `sources.json` records each original URL and SHA-256. Files are copied without changes.

- Rust compiler-builtins and its libm code come from the Rust 1.99.0 source tree.
- LLVM runtime notices come from `rust-lang/llvm-project` commit `1b9c0d5ff9bbe7634aead059efe6b11a7eeba145`, the submodule pinned by Rust 1.99.0.
- Static Linux builds use musl 1.2.5, as pinned by Rust 1.99.0's `src/ci/docker/scripts/musl.sh`; Rust applies additional security patches during its toolchain build.

The packaging script also copies `COPYRIGHT-library.html` and the license texts from the installed, pinned Rust toolchain's `share/doc/rust` directory. Cargo package notices and source links are collected separately. A notice included here does not imply that every build links every runtime.
