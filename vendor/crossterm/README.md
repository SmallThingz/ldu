# Crossterm for LDU

Pinned to crossterm 0.29.0. `UPSTREAM.json` records the crate checksum and
upstream commit; `LICENSE` and `Cargo.toml.orig` are unchanged upstream files.
The private fork uses LDU's Rust 1.99 toolchain and current synchronous Windows
test support.

The local patch adds `event::input_is_idle()` for Unix `use-dev-tty` builds.
LDU uses it to avoid confirming deletion while terminal input remains queued.
Ordinary event polling and reading retain their existing interface.

All library sources, unit tests and doctests are retained. Examples and their
async runtimes are omitted; only library-test dependencies remain. Existing
upstream ignored tests are unchanged. `cargo test --workspace` includes this
crate. LDU's strict Clippy and formatting checks do not rewrite upstream code.
