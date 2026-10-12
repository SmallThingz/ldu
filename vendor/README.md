# Vendored dependencies

## filedescriptor 0.8.3

`filedescriptor/` contains the published `filedescriptor` 0.8.3 sources from
[WezTerm](https://github.com/wezterm/wezterm). The upstream package version,
MIT license (`filedescriptor/LICENSE.md`), README, and original manifest
(`Cargo.toml.orig`) are retained unchanged. The normalized `Cargo.toml` omits
its package-level `resolver = "2"` setting so the root workspace owns resolver
selection. This is the only local manifest integration change. The source files
retain their upstream tests and existing native FFI code.

The local patch changes the timeout conversion shared by Unix `poll` and Windows
`WSAPoll`: positive fractional milliseconds round up, finite values saturate at
`libc::c_int::MAX`, zero remains nonblocking, and `None` remains an infinite wait.
The conversion helper and its boundary tests use safe Rust; the existing unsafe
native calls are unchanged. macOS retains its upstream `select` implementation.

This fixes idle CPU use with Crossterm 0.28.1's level-triggered `use-dev-tty`
backend. Its precise timeout loop repeatedly invokes `filedescriptor::poll`
with less than one millisecond left. Upstream 0.8.3 truncates that remainder to
zero and busy-polls until the deadline; this patch waits for readiness or the
rounded timeout while preserving the existing terminal parser and resize path.
