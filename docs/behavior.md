# Behavior and platform details

## Scanning and display

The terminal opens before scanning starts. Directory scanning uses breadth-first depth barriers and parallel metadata batches. Progress updates use bounded channels; keyboard actions repaint immediately.

Scanning and completed entries share the same sort. Near-size groups retain their previous order to reduce movement. Each group spans at most 2%; nearby entries in different groups can still exchange places. Selection tracks the entry by its ID as live totals change.

Gray Braille spinners mark folders still being scanned. Bars use fractional block glyphs. The main list has one path header and a `?` help hint, with no frame. At widths of at least 120 columns, the selected directory's subdirectories appear in a side preview using the current sorting, size, graph and hidden-entry settings. Files are omitted from the preview.

Hidden entries are visible initially. Hiding dotfiles changes the listing, not the totals. Sizes remain partial while scanning or after read errors. A changing filesystem is not a consistent snapshot.

## Options

| Option | Behavior |
| --- | --- |
| `--threads N`, `-j N` | 1–128 workers; default is available parallelism, capped at 32 |
| `--backend auto\|native\|std` | Directory enumeration backend; default `auto` |
| `--buffer-size N` | Native directory buffer in bytes; default 262144, range 4096–4194304 |
| `--batch-size N` | Metadata batch size; default 128, range 1–4096 |
| `--apparent-size` | Start with metadata lengths instead of allocated disk usage |
| `--json` | Print root totals and immediate directory summaries without a terminal |

JSON includes both apparent and allocated sizes. Exit status is 0 for a completed scan, 2 for errors, cancellation or an incomplete scan, and 1 for setup failure. Non-UTF-8 names are preserved internally but represented lossily in JSON. Run `ldu --help` for the complete command syntax.

## Keys and dialogs

| Key | Action |
| --- | --- |
| Up / `k`, Down / `j` | Move selection |
| Right / Enter / `l` | Open selected directory |
| Left / `h` / `<` / Backspace | Go to parent |
| Home / End, PageUp / PageDown | Move to an end or by a page |
| `n`, `s`, `C` | Sort by name, size or recursive item count; repeat to reverse |
| `a` | Toggle allocated disk usage and apparent size |
| `t` | Toggle directories before files |
| `g` | Cycle bar, percentage and no graph |
| `c` | Toggle item counts |
| `e` | Toggle hidden entries |
| `i` | Open selected entry information |
| `r` | Rescan and return to the scan root |
| `d` | Confirm deletion of the selected entry |
| `?` | Open help |
| `q`, Ctrl-C | Quit |

Dialogs have thin, sharp borders and highlighted keys and actions. Help, information and notices scroll with Up/`k`, Down/`j`, PageUp/PageDown and Home/End. Enter, Escape, `q` or `i` closes them. `?` closes help or opens it from information and notices. Ctrl-C quits immediately.

Manual refresh requests are coalesced and wait for scanning and any pending deletion to stop. Shell spawning is unavailable.

## Deletion

`d` opens confirmation. `n` or Escape cancels; `y` or Enter starts background deletion. `q` and `?` are ignored in the confirmation dialog. Confirmation stays disabled until the rendered dialog has at least 18 inner columns, a visible target row and visible confirmation choices. Resize the terminal to enable confirmation; `n` or Escape can always cancel. The scan root cannot be deleted.

Successful deletion updates the listing and ancestor totals incrementally. It does not restart the scan or leave a success dialog open. Failure shows a brief notice and marks the affected branch partial, preserving its observed totals. Use `r` after a partial deletion to refresh them. Once confirmed, deletion has no cancellation command; quitting during recursive removal can leave a partial result.

Deletion requires Linux and accessible procfs. The worker resolves the parent beneath a held root anchor, refuses symlink ancestors, and reads and rechecks the target identity. Identity is captured after confirmation, so changes since the scan or dialog opened are not detected. These checks do not provide an atomic guarantee against concurrent filesystem changes. A final symlink is unlinked without following its target. Other platforms report deletion as unsupported.

## Size accounting

Allocated disk usage is the default on Unix: reported filesystem blocks multiplied by 512. Apparent size is the metadata length. Directory metadata contributes to totals. Symlinks are measured without following their targets.

Regular-file hard links are deduplicated by device and inode; duplicate entries contribute zero to totals. Attribution between directories can depend on discovery order, so totals do not predict the space reclaimed by deleting a directory. Shared extents, compression and copy-on-write clones are not deduplicated.

On non-Unix platforms, allocated size is approximated by apparent size and hard links are not deduplicated.

## Backends and platforms

On Linux, all backends keep directory reads and metadata anchored to the root descriptor. Symlink ancestors in the supplied root path are rejected; a final symlink is measured as a leaf.

The native backend uses Rustix for buffered directory reads. `auto` falls back to standard directory enumeration only when native enumeration is unsupported, before emitting entries. Metadata remains relative to the held descriptor; permission errors remain errors. Linux `std` enumeration requires accessible `/proc/self/fd`.

Other platforms use the standard backend; explicit `native` requires Linux. The non-Linux backend rejects known symlink directories but cannot fully eliminate path-replacement races.

There is no io_uring backend. Performance depends on the filesystem, cache state and concurrent load; no universal speed claim is made. Review qualification and benchmark conditions are tracked in [REVIEW.md](../REVIEW.md).

## Upstream references

Local forks of ncdu and DUA, their pinned commits and licenses are documented in [upstream/README.md](../upstream/README.md). They have not been published as remote forks. LDU is an independent Rust implementation; neither upstream executable is built or linked into it.
