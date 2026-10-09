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
- Cancellable FIFO deletion queue that updates totals without restarting the scan
- Scoped refresh of the selected folder or current directory
- Visual keyboard and mouse selection with confirmed batch deletion

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
| `d` | Confirm and queue deletion |
| `V` | Toggle visual selection |
| `D` | Open queue; confirm batch deletion in visual mode |
| `r`, `R` | Refresh selected folder or current directory |
| `?`, `q` / Ctrl-C | Help, quit |

In visual mode, arrows and Shift-arrows extend selection, Enter toggles the current row, and `D` confirms deletion of the selected entries. Shift-click extends selection; Shift-drag reverses marks in its range. `V` or Escape leaves visual mode.

In the deletion queue, Space pauses waiting jobs, `j`/`k` selects, Shift-Up/Down or `K`/`J` reorders, and `d` removes the selected waiting job or cancels the active job. `c` cancels the active job. Quitting with deletions queued requires confirmation and cancels the remaining work.

Allocated disk usage is the default. Scanning and finished entries sort together; the selected entry stays selected as totals change. Hovering over or entering an unfinished folder gives its scan priority. At 120 columns or wider, hovering over a directory shows its subdirectories beside the main list.

[Behavior and platform details](docs/behavior.md) · [ncdu and DUA references](upstream/README.md) · [Review results](REVIEW.md)

MIT license; see [LICENSE](LICENSE).
