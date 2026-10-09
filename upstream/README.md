# Upstream local forks

These are local Git forks for reference and future development. They retain one upstream commit, the upstream files and licenses, a local `ldu` branch, and an `upstream` remote. They have not been published as remote forks.

| Directory | Upstream | Source branch | Pinned commit | License |
| --- | --- | --- | --- | --- |
| `ncdu/` | https://code.blicky.net/yorhel/ncdu.git | `zig` | `1b3d0a670e730b338de6b716e2a60cf86351e44b` | MIT; `ncdu/LICENSES/MIT.txt`, copyright in SPDX file headers |
| `dua/` | https://github.com/Byron/dua-cli.git | `main` | `d11cb9c771a91d702cd556fb1405f8b79064472b` | MIT; `dua/LICENSE` |

Ncdu 2.x is written in Zig; its legacy `master` branch is C. DUA is written in Rust. The root `ldu` application is an independent Rust implementation informed by ncdu's browsing conventions and DUA's parallel scanning approach. Neither upstream executable is built or linked into `ldu`.

The checkouts are shallow (`--depth 1`). Their retained upstream history can be expanded when needed with `git -C upstream/ncdu fetch --unshallow upstream` and the equivalent command for DUA. No new commits or pushes were made when acquiring these forks.

References: [ncdu project](https://dev.yorhel.nl/ncdu), [ncdu keys and behavior](https://dev.yorhel.nl/ncdu/man), [DUA project](https://github.com/Byron/dua-cli).
