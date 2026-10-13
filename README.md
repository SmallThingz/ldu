# ldu

![Rust](https://img.shields.io/badge/Rust-2024-a65f34?logo=rust&logoColor=ffffff)
![Linux](https://img.shields.io/badge/platform-Linux-495057)
![Ratatui](https://img.shields.io/badge/TUI-Ratatui-526d82)

Terminal disk-usage browser written in Rust. Browse folder sizes while a parallel scan runs.

## Highlights

- Live totals with scan work balanced by entries checked
- Gray Braille spinners beside folders still scanning
- Cached sorted directory views, text previews, symlink destinations and optional archive listings
- Fractional block bars for size distribution
- Cancellable FIFO deletion queue with permission recovery and scrollable failure reports
- Scoped refresh of the selected folder or current directory
- Visual keyboard and mouse selection with confirmed batch deletion

## Download

Choose a Linux binary with `musl` in its name from [Releases](https://github.com/SmallThingz/ldu/releases/latest), make it executable with `chmod +x`, and run it. The musl binaries are statically linked and do not require glibc. See that release's notes for available architectures.

The release workflow in this checkout targets only musl: x86-64, ARM64, ARMv7 and RISC-V. These pipeline changes have not been published.

Deletion requires accessible procfs. See [backend details](docs/behavior.md#backends-and-platforms).

## Build

Build and run the source checkout:

```sh
git clone https://github.com/SmallThingz/ldu.git
cd ldu
cargo build --locked --release
./target/release/ldu /path/to/directory
```

The project toolchain is pinned to Rust 1.99.0. `edition = "2024"` selects the language edition, not the compiler version.


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

With no path, `ldu` scans the current directory. Run it in an interactive terminal, or use `-o FILE` to export the scanned tree in ncdu’s JSON format (`-o -` writes to stdout). Exports run without a terminal interface; `-0` is accepted for ncdu compatibility.

```sh
./target/release/ldu -t 8 /path/to/directory
./target/release/ldu --apparent-size /path/to/directory
./target/release/ldu -o scan.json /path/to/directory
ncdu -f scan.json
```

`--backend auto|native|std` controls scanning. On supported 64-bit Linux targets, `auto` uses io_uring for metadata when available, with blocking worker calls as the fallback. Directory enumeration stays buffered. Deletion also uses available io_uring support for batches of files. See [backend details](docs/behavior.md#backends-and-platforms).

`--threads` (`-t`, with `-j` as an alias) accepts 1–128; the default uses available parallelism, capped at 32. Run `ldu --help` for all options.

## Configuration

LDU reads `$XDG_CONFIG_HOME/ldu/config`, or `~/.config/ldu/config` when `XDG_CONFIG_HOME` is unset, empty or not an absolute path. It does not create the file. Use one `key = value` per line; blank lines and lines starting with `#` are ignored.

```ini
color = cyan
mouse = true
mouse_debounce_ms = 250
mouse_debounce_delay_ms = 1250
mouse_debounce_warning = true
```

`color` changes the interface accent. Use a terminal color name, an index from 0 to 255, or a hex color such as `#cba6f7`. Hex colors use true color when the terminal advertises support, or the nearest supported palette color otherwise.

Command-line options override the file:

```sh
ldu --color '#cba6f7' /path/to/directory
ldu --no-mouse /path/to/directory
ldu --mouse-debounce-ms 0 /path/to/directory
```

The mouse guard applies when hovering changes the active entry shortly before normal-mode deletion. By default, an entry change within 250 ms opens a warning with a muted red background and blocks confirmation for 1250 ms. Early confirmation is discarded; cancel remains available. Set `mouse_debounce_ms = 0` to disable the guard, or `mouse_debounce_warning = false` to hide the warning while retaining the delay. Movement within one entry and wheel scrolling do not trigger it. Keyboard navigation clears it, and visual selection is unaffected.

The scan settings also support config keys. See [all config keys and options](docs/behavior.md#configuration).

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

`d` followed immediately by Enter confirms deletion, including while target checks finish, unless a recent mouse entry change activates the guard described above. In visual mode, `d`, `D` or Delete opens batch confirmation and retains Enter while earlier selection work settles. `n` or Escape cancels the request and keeps the selection. The target area and confirmation choices must be visible before confirmation can proceed.

In normal mode, click a directory to open it. Right-click an entry to enter selection mode and select it. In selection mode, click to toggle a row. Drag from an unselected row to select a range, or from a selected row to deselect it.

Ctrl-click deselects a row; Ctrl-drag removes a range from the selection. Ctrl takes precedence over Shift, and the modifiers held at mouse press determine the whole gesture. Shift-click adds a range; Shift-drag reverses its marks. Dragging at the list edges scrolls through more entries.

Blue rows are selected; `›` marks the cursor and `✓` marks other selected rows. The header shows the selected count. Hovering over it shows `X selected`; click it to clear the selection and leave selection mode. Click the delete action at the right to open the usual confirmation dialog.

The wheel scrolls the list while hover controls the highlight. Click a path component to navigate within the scanned tree. Components above the scan root are dim and inactive. Narrow headers use `…` for omitted path text.

The `../` row opens the parent in normal mode and does nothing at the scan root. In visual mode, click or press Enter on `../` to select every direct entry, including hidden and filtered-out entries; right-click does the same from either mode. Ctrl-click clears the selection. Selecting this row clears the filter and reveals hidden entries. Selection waits for the direct folder listing. If scanning stops early, LDU keeps the known marks, cancels any pending confirmation and asks you to refresh.

The header shows clickable `? help`. Queued work shows `n queued`; an active deletion with no waiting jobs shows `deleting…`. Outside selection mode, click `[D]` to open the queue.

After batch confirmation, an `Enqueueing selection` dialog stays open until the entries join the queue. `n` or Escape stops adding the remaining entries; already queued deletions continue. `q` or Ctrl-C opens a warning while confirmed deletions finish.

Deletion retries a failed removal once, repairs permissions when needed, and rescans a nonempty directory once. If the retry fails, deletion continues through the remaining siblings and reports the errors once the target finishes. Reports include full paths, retain their order while another dialog is open, and scroll without rendering the entire error list. Enter dismisses one report at a time.

In the deletion queue, hover selects a job, click pauses or resumes waiting work, and right-click removes a waiting job or cancels the active job. Space also pauses waiting jobs. Use `j`/`k` to select a job and Shift-Up/Down or `K`/`J` to reorder it. `d` removes the selected waiting job or cancels it if active; `c` cancels the active job. The footer lists the mouse controls.

Scanning pauses while deletion is active and resumes afterward. A red `×` marks queued or active deletion targets and their descendants in the main list and directory preview.

Dialog choices are clickable. Scrollable dialogs accept the mouse wheel and show a scrollbar. The top-right close button dismisses or cancels the current dialog. Closing the queue keeps confirmed deletions running. Closing `Stopping deletions` exits immediately.

Right-click the top bar to open `Quit LDU?`. Yes exits immediately when no deletion is pending; otherwise it shows deletion progress and exits when confirmed work finishes. No or the close button returns to the previous dialog or browser.

With confirmed deletions pending, the first `q` or Ctrl-C stops scans and waits for deletion to finish, then exits. A deletion error hidden by the warning is reported after the terminal is restored. Queue pause still applies. A second Ctrl-C cancels all deletions and quits. `n` or Escape keeps browsing; stopped scans stay stopped until you refresh with `r` or `R`.

Allocated disk usage is the default. Scanning and finished entries sort together. At startup, the cursor stays at the top until you navigate or select an entry. Afterward, live reordering follows the selected entry and keeps up to three rows visible above and below it where space allows. Hovering over or entering an unfinished folder gives its scan priority.

At 96 columns or wider, the list and preview share the width in a 4:3 ratio. The preview shows directory contents and text. Symlinks show their stored destination path, including broken links, without opening the target. Installed archive utilities can provide a bounded listing of archive entries. Formats without a suitable tool, or failed previews, fall back to file details. Nothing needs to be installed for ordinary browsing.

[Behavior and platform details](docs/behavior.md) · [ncdu and DUA references](upstream/README.md)

MIT license; see [LICENSE](LICENSE).
