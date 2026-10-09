# ldu

![Rust](https://img.shields.io/badge/Rust-2024-a65f34?logo=rust&logoColor=ffffff)
![Linux](https://img.shields.io/badge/platform-Linux-495057?logo=linux&logoColor=ffffff)
![Ratatui](https://img.shields.io/badge/TUI-Ratatui-526d82)

Terminal disk-usage browser written in Rust. Browse folder sizes while a parallel breadth-first scan runs.

## Highlights

- Live totals from a parallel breadth-first scan
- Gray Braille spinners beside folders still scanning
- Cached sorted directory views, with matching subfolder previews on wide terminals
- Fractional block bars for size distribution
- Background deletion that updates totals without restarting the scan

## Build

```sh
cargo build --release
./target/release/ldu /path/to/directory
```

With no path, `ldu` scans the current directory. Run it in an interactive terminal, or use `--json` for root totals and immediate directory summaries.

```sh
./target/release/ldu --threads 8 /path/to/directory
./target/release/ldu --apparent-size /path/to/directory
./target/release/ldu --json /path/to/directory
```

`--backend auto|native|std` chooses directory enumeration. `--threads` (`-j`) accepts 1–128; the default uses available parallelism, capped at 32. Run `ldu --help` for all options.

## Keys

| Key | Action |
| --- | --- |
| Up / `k`, Down / `j` | Move |
| Right / Enter / `l` | Open directory |
| Left / `h` / `<` / Backspace | Parent directory |
| `n`, `s`, `C` | Sort by name, size, or item count; repeat to reverse |
| `a`, `g` | Change size mode or graph |
| `e`, `t` | Toggle hidden entries or directories first |
| `d` | Delete with confirmation |
| `r` | Rescan |
| `?`, `q` / Ctrl-C | Help, quit |

Allocated disk usage is the default. Scanning and finished entries sort together; the selected entry stays selected as totals change. At 120 columns or wider, hovering over a directory shows its subdirectories beside the main list.

[Behavior and platform details](docs/behavior.md) · [ncdu and DUA references](upstream/README.md) · [Review results](REVIEW.md)

MIT license; see [LICENSE](LICENSE).
