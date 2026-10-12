# ldu

![Rust](https://img.shields.io/badge/Rust-2024-a65f34?logo=rust&logoColor=ffffff)
![Platforms](https://img.shields.io/badge/platforms-Linux%20%7C%20macOS%20%7C%20Windows-495057)
![Ratatui](https://img.shields.io/badge/TUI-Ratatui-526d82)

Terminal disk-usage browser written in Rust. Browse folder sizes while a parallel scan runs.

## Highlights

- Live totals with scan work balanced by entries checked
- Gray Braille spinners beside folders still scanning
- Cached sorted directory views, text previews, symlink destinations and optional archive/PDF previews
- Fractional block bars for size distribution
- Cancellable FIFO deletion queue with permission recovery and scrollable failure reports
- Scoped refresh of the selected folder or current directory
- Visual keyboard and mouse selection with confirmed batch deletion

## Download

Download an archive for your OS and architecture from [Releases](https://github.com/SmallThingz/ldu/releases/latest). Each archive contains the executable, license, and usage notes; `SHA256SUMS` lists the archive checksums. Linux downloads include glibc and statically linked musl builds; see the release notes for architectures and compatibility details.

Deletion requires Linux and accessible procfs. macOS supports browsing and previews; Windows supports browsing with metadata previews. On Windows, allocated size is approximated by apparent size and hard links are not deduplicated. See [platform details](docs/behavior.md#backends-and-platforms).

## Build

Build and run the source checkout:

```sh
git clone https://github.com/SmallThingz/ldu.git
cd ldu
cargo build --locked --release
./target/release/ldu /path/to/directory
```

To update an existing checkout, run these commands from its directory:

```sh
git pull --ff-only
cargo build --locked --release
./target/release/ldu /path/to/directory
```

To install or update the `ldu` command in Cargo's binary directory:

```sh
cargo install --locked --path . --force
```

After a successful build, `./target/release/ldu` runs this checkout's new binary.

With no path, `ldu` scans the current directory. Run it in an interactive terminal, or use `--json` for root totals and immediate directory summaries.

```sh
./target/release/ldu --threads 8 /path/to/directory
./target/release/ldu --apparent-size /path/to/directory
./target/release/ldu --json /path/to/directory
```

`--backend auto|native|std` chooses directory enumeration. `--threads` (`-j`) accepts 1–128; the default uses available parallelism, capped at 32. Run `ldu --help` for all options.

## Keys

Help groups shortcuts into Normal mode, Visual mode and Common keys.

### Normal mode

| Key | Action |
| --- | --- |
| Up / `k`, Down / `j` | Move cursor |
| Shift-Up / Shift-Down | Move by a page |
| Enter | Open directory |
| `v` / `V` | Select current row and enter visual mode |
| `d` | Confirm deletion of the current entry |
| `D` | Open queue |

### Visual mode

| Key | Action |
| --- | --- |
| Up / `k`, Down / `j` | Move cursor without changing selection |
| Shift-Up / Shift-Down | Add a range to selection |
| Enter | Toggle current row |
| `d`, `D`, Delete | Confirm batch deletion |
| `v` / `V` | Clear selection and leave visual mode |
| Escape | Clear an active filter; otherwise clear selection and leave visual mode |

### Common keys

| Key | Action |
| --- | --- |
| Right / `l` | Open directory |
| Left / `h` / `<` / Backspace | Parent directory |
| Home / End, PageUp / PageDown | Move to an end or by a page |
| `/` | Edit the current directory filter |
| `n`, `s`, `C` | Sort by name, size, or item count; repeat to reverse |
| `a`, `g` | Change size mode or graph |
| `e`, `t` | Toggle hidden entries or directories first |
| `r`, `R` | Refresh selected folder or current directory |
| `?`, `q` / Ctrl-C | Help, quit |

Press `/` to filter file and folder names in the current directory using fuzzy matching. It does not search subdirectories. Results update as you type. Backspace removes the last character, Ctrl-U clears the query, Enter finishes typing, and Escape clears the filter. In visual mode, changing or clearing the query clears the marks and keeps visual mode active. Entering a directory or going to its parent clears the filter.

Press `v` or `V` to select the current row. In visual mode, Up/Down and `j`/`k` move the cursor without changing selection. Shift-Up/Down adds a range; Enter toggles the current row. `d`, `D` or Delete opens batch confirmation, waiting for any pending selection work. Before confirming, cancel with `n` or Escape to keep the selection.

In normal mode, click a directory to open it. In visual mode, click to toggle a row, or drag to add a range. Ctrl-click deselects a row; Ctrl-drag removes a range from the selection. Ctrl takes precedence over Shift, and the modifiers held at mouse press determine the whole gesture. Shift-click adds a range; Shift-drag reverses its marks. Dragging at the list edges scrolls through more entries. `v` or `V` leaves visual mode; Escape does so when no filter is active. Blue rows are selected; `›` marks the cursor and `✓` marks other selected rows. The header shows the selected count and current controls as space allows. The wheel scrolls the list while hover controls the highlight. The two-column `←─` back arrow highlights on hover and opens the parent folder on click. Long header paths show their final components. Outside visual mode, the header shows `? help` and adds `Deleting ... [D]` while deletion is queued or active.

After batch confirmation, an `Enqueueing selection` dialog stays open until the entries join the queue. `n` or Escape stops adding the remaining entries; already queued deletions continue. `q` or Ctrl-C opens a warning while confirmed deletions finish.

Deletion continues past individual failures, tries permission recovery when needed, and reports remaining errors once each target finishes. Reports include full paths, retain their order while another dialog is open, and scroll without rendering the entire error list. Enter dismisses one report at a time.

In the deletion queue, Space pauses waiting jobs, `j`/`k` selects, Shift-Up/Down or `K`/`J` reorders, and `d` removes the selected waiting job or cancels the active job. `c` cancels the active job. Scanning pauses while deletion is active and resumes afterward. A red `×` marks queued or active deletion targets and their descendants in the main list and directory preview.

With confirmed deletions pending, the first `q` or Ctrl-C stops scans and waits for deletion to finish, then exits. A deletion error hidden by the warning is reported after the terminal is restored. Queue pause still applies. A second Ctrl-C cancels all deletions and quits. `n` or Escape keeps browsing; stopped scans stay stopped until you refresh with `r` or `R`.

Allocated disk usage is the default. Scanning and finished entries sort together. At startup, the cursor stays at the top until you navigate or select an entry. Afterward, live reordering follows the selected entry and keeps up to three rows visible above and below it where space allows. Hovering over or entering an unfinished folder gives its scan priority.

At 96 columns or wider, the list and preview share the width in a 4:3 ratio. The preview shows directory contents and text. Symlinks show their stored destination path, including broken links, without opening the target. Installed archive utilities can provide a bounded listing of archive entries; `pdftotext` can provide PDF text. Formats without a suitable tool, or failed previews, fall back to file details. Nothing needs to be installed for ordinary browsing.

[Behavior and platform details](docs/behavior.md) · [ncdu and DUA references](upstream/README.md) · [Review results](REVIEW.md)

MIT license; see [LICENSE](LICENSE).
