# Behavior and platform details

## Scanning and display

The terminal opens before scanning starts. Directory scanning normally uses breadth-first depth barriers and parallel metadata batches. Hovering over or entering an unfinished directory gives that subtree scan priority; unrelated browsing does not restart the scan. Progress updates use bounded channels; keyboard actions repaint immediately. Incomplete terminal input does not block resizing, scan updates or signal-driven shutdown.

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
| `r` | Refresh the selected folder |
| `R` | Refresh the current directory |
| `d` | Confirm and queue deletion of the selected entry |
| `V` | Toggle visual selection |
| `D` | Open the queue, or confirm batch deletion in visual mode |
| `?` | Open help |
| `q`, Ctrl-C | Quit |

Dialogs have thin, sharp borders and highlighted keys and actions. Help, information and notices scroll with Up/`k`, Down/`j`, PageUp/PageDown and Home/End. Enter, Escape, `q` or `i` closes them. `?` closes help or opens it from information and notices. With deletions queued or active, `q` and Ctrl-C open a quit confirmation: `q`, Enter or a second Ctrl-C cancels the queue and active worker, then waits for active removal to stop before restoring the terminal. Cancelled metadata capture retires in the background and does not delay terminal restoration. `n` or Escape keeps browsing. Confirmation stays disabled until the quit choices fit on screen.

`r` refreshes only a selected directory; selecting a file has no effect. `R` refreshes the directory currently being browsed. Both preserve the current path, retain unrelated entries and update ancestor totals. Requests are coalesced and wait while a queued deletion overlaps their scope. Hardlink accounting remains shared across the original scan and refreshed subtrees. Fresh hardlink metadata updates its global owner even when that owner lies outside the refreshed folder. Shell spawning is unavailable.

`V` starts visual selection by marking the current row. Arrows and Shift-arrows add a range; Enter toggles the current row and starts a new range from it. Shift-click adds a range, while Shift-drag reverses selected membership across the drag range. Blue backgrounds mark selected entries, including the current cursor. Selection tracks stable entry IDs as sorting changes; each range retains the ordering from when it began. `V` or Escape clears selection and leaves visual mode. Navigating to another directory also clears it.

In visual mode, `D` confirms batch deletion. Identity checking runs in the background before the dialog enables `y` or Enter. `n` or Escape cancels and preserves the selection. Confirming enqueues the selected entries and leaves visual mode; queue pause, ordering, overlap and cancellation rules apply to the batch too.

## Deletion

`d` opens confirmation. `n` or Escape cancels; `y` or Enter adds the target to the deletion queue. Further entries can be confirmed while removal runs. `?` is ignored in the deletion confirmation. `q` is ignored while no deletion is queued; with queued or active work it opens the guarded quit dialog. Confirmation waits for the identity snapshot and stays disabled until the rendered dialog has at least 18 inner columns, a visible target row and visible confirmation choices. Resize the terminal to enable confirmation; `n` or Escape can always cancel. The scan root cannot be deleted.

Outside visual mode, `D` opens the styled queue. One worker removes entries in FIFO order. The queue shows the active target, removal progress and waiting targets. Up/Down or `k`/`j` selects a row; Shift-Up/Down or `K`/`J` moves a waiting target. Space or `p` pauses and resumes waiting work. The active worker continues while the queue is paused. `d` removes a selected waiting target or cancels a selected active target; `x` or Delete removes a waiting target, and `c` cancels the active worker. `D` or Escape closes the queue.

Confirming an already queued target, or an entry inside a queued ancestor, does not add a duplicate. A newly queued ancestor replaces its waiting descendants. An active descendant finishes or stops before a queued ancestor starts. Removing a waiting target leaves its files untouched.

Queued branches stop contributing new scan work and related scan errors while deletion is pending. Unrelated scanning continues. Successful deletion updates the listing and ancestor totals incrementally, without a full rescan or success dialog. Cancellation is cooperative between removal steps: already removed entries stay removed, while remaining entries survive. Cancelled or failed directory deletion requests a scoped refresh of the surviving target; an overlapping queued deletion delays that refresh. A failure preserves open dialogs and shows a brief notice after they close. The latest waiting failure replaces earlier waiting notices.

Deletion requires Linux and accessible procfs. An asynchronous snapshot resolves the parent beneath a held root anchor before confirmation. The queue retains the parent and target identities; removal compares them before changing the filesystem and rejects replacement parents or targets. A missing target at snapshot time cannot authorize a replacement that appears later. Symlink ancestors are refused, and target identities are rechecked during traversal. These checks do not provide an atomic guarantee against concurrent inode reuse, renames or filesystem changes. A final symlink is unlinked without following its target. A target already absent when its queued turn arrives succeeds. Other platforms report deletion as unsupported.

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
