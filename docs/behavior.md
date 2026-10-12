# Behavior and platform details

## Scanning and display

The terminal opens before scanning starts. Available workers favor subtrees with fewer reported entry checks, with an allowance for directory reads already underway. Scheduling compares a bounded sample of competing siblings, so the balance is approximate. Deeper work can start while shallower directories are still being read. Hovering over or entering an unfinished directory gives its pending work priority; active reads continue. Metadata work runs in parallel batches, and browsing does not restart the scan. Progress updates use bounded channels; keyboard actions repaint immediately. Incomplete terminal input does not block resizing, scan updates or signal-driven shutdown.

Scanning and completed entries share the same sort. Near-size groups retain their previous order to reduce movement. Each group spans at most 2%; nearby entries in different groups can still exchange places. At startup, the cursor follows the first sorted row without giving its scan priority. Navigation, hovering over an entry, filtering or an entry action ends this behavior; help, sorting and display-only settings do not. Hiding dotfiles counts as filtering. Afterward, selection tracks the entry by its ID as live totals change. This startup behavior does not restart when changing directories or refreshing.

Gray Braille spinners mark folders still being scanned. Bars use fractional block glyphs. The unframed main list has a path header with a two-column `←─` back button and `? help`. The back button highlights on hover and opens the parent on click. A `Deleting ... [D]` status appears while jobs are queued or active; there is no select shortcut in the header. Long paths retain their trailing components, with the omitted prefix shown as `..`. Visual mode shows the selected count and deletion controls in that header. At widths of at least 96 columns, the list and preview share the width in a 4:3 ratio. Both panes keep their width as the cursor moves between files and directories. Directory previews use the current sorting, size, graph and hidden-entry settings.

Text previews show the beginning of UTF-8 or BOM-marked UTF-16 files. Symlinks show their stored destination path, escaped and wrapped to fit the pane, including dangling links; the target is never opened. When a suitable archive-listing utility is installed, archives show entry names without extracting files. Installed `pdftotext` can preview PDF text. External helper previews are supported on Linux and macOS; other platforms retain their existing preview fallback. Unsupported formats and failed helper requests show file details. Preview work runs in the background with cancellation and limits on displayed output; reaching output limits adds a truncation mark. Helper processes have a time limit and receive an already-open regular file. Special files show metadata without opening their contents. Narrower terminals hide the preview and do not request file contents.

Keyboard movement leaves up to three visible rows above and below the cursor. The mouse wheel scrolls the list itself and keeps the highlight under the pointer. The margin shrinks at the ends of the list and in short terminals. Once selection follows an entry, live resorts keep the cursor on the same screen row where the margin allows. Mouse hovering keeps the viewport still, and held presses retain their painted rows. A plain click opens a directory in normal mode and toggles selection in visual mode. Normal clicks activate on release at the original live row; dragging or an intervening keyboard action cancels activation.

Once scanning and pending updates finish, the screen stops redrawing until input or new results change it. “Updating sizes” appears only while deletion or refresh cleanup is still pending; completing that cleanup restores the settled status.

Hidden entries are visible initially. Hiding dotfiles changes the listing, not the totals. Sizes remain partial while scanning, after read errors, or after deletion if refreshing the parent's metadata was interrupted. A changing filesystem is not a consistent snapshot.

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

Help groups shortcuts into Normal mode, Visual mode and Common keys.

### Normal mode

| Key | Action |
| --- | --- |
| Up / `k`, Down / `j` | Move cursor |
| Shift-Up / Shift-Down | Move by a page |
| Enter | Open selected directory |
| `v` / `V` | Select current row and enter visual mode |
| `d` | Confirm and queue deletion of the current entry |
| `D` | Open the queue |

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
| Right / `l` | Open selected directory |
| Left / `h` / `<` / Backspace | Go to parent |
| Home / End, PageUp / PageDown | Move to an end or by a page |
| `/` | Edit the current directory filter |
| `n`, `s`, `C` | Sort by name, size or recursive item count; repeat to reverse |
| `a` | Toggle allocated disk usage and apparent size |
| `t` | Toggle directories before files |
| `g` | Cycle bar, percentage and no graph |
| `c` | Toggle item counts |
| `e` | Toggle hidden entries |
| `i` | Open selected entry information |
| `r` | Refresh the selected folder |
| `R` | Refresh the current directory |
| `?` | Open help |
| `q`, Ctrl-C | Quit |

Press `/` to filter file and folder names in the current directory using fuzzy matching. It does not search subdirectories. Results update as you type. Backspace removes the last character, Ctrl-U clears the query, Enter finishes typing, and Escape clears the filter. In visual mode, changing or clearing the query clears the marks and keeps visual mode active. Entering a directory or going to its parent clears the filter.

Dialogs have thin, sharp borders and highlighted keys and actions. Help, information and ordinary notices scroll with Up/`k`, Down/`j`, PageUp/PageDown and Home/End. Enter, Escape, `q` or `i` closes them. `?` closes help or opens it from information and notices. With confirmed deletions pending, the first `q` or Ctrl-C stops all scans and pending refreshes, opens a warning, and lets deletions finish before exiting. A deletion error hidden by the warning is reported after terminal restoration with exit status 1. Queue pause still applies. `n` or Escape dismisses the warning and keeps browsing; stopped scans do not restart automatically, so use `r` or `R` to refresh.

A second Ctrl-C cancels all deletions and quits, even when the terminal is too small to show the choices. `q` or Enter also confirms cancellation when the choices are visible. Cancellation waits up to one second before restoring the terminal, even if filesystem work is blocked; another Ctrl-C exits immediately during that wait. SIGINT follows the same sequence.

`r` refreshes only a selected directory; selecting a file has no effect. `R` refreshes the directory currently being browsed. Both preserve the current path, retain unrelated entries and update ancestor totals. Requests are coalesced and wait while a queued deletion overlaps their scope. Hardlink accounting remains shared across the original scan and refreshed subtrees. Fresh hardlink metadata updates its global owner even when that owner lies outside the refreshed folder. Shell spawning is unavailable.

`v` or `V` starts visual selection by marking the current row. Up/Down and `j`/`k` move the cursor without changing selection. Shift-Up/Down adds a range. Enter toggles the current row and starts a new range from it; extending that range with Shift preserves an explicit deselection of its starting row. A `›` marks the cursor; `✓` marks other selected rows. Blue backgrounds indicate selection. An unselected cursor has a gray background. The header shows the selection count and controls as space allows. Selection tracks stable entry IDs as sorting changes; each range retains the ordering from when it began. `v` or `V` clears selection and leaves visual mode; Escape does so when no filter is active. Navigating to another directory also clears it.

In visual mode, a plain click toggles a row and a plain drag adds a range. Ctrl-click deselects a row and Ctrl-drag removes a range from the pre-press selection; reversing a drag restores original marks outside the current range. Ctrl takes precedence over Shift. Shift-click adds a range from the selection anchor; Shift-drag reverses selected membership across the drag range. The modifiers at mouse press determine the gesture even if Ctrl or Shift changes during the drag. Dragging at the top or bottom of the list scrolls to include offscreen entries. If pending visual input fills the queue, the header shows `Input full`; new gestures may be ignored until it catches up. Escape and Ctrl-C remain available.

In visual mode, `d`, `D` or Delete requests batch deletion. If a range or earlier selection input is still being processed, a `Selecting entries` notice stays open until it finishes. Enter is ignored throughout selection-preparation waits. `n` or Escape cancels the pending request and preserves the selection. Identity checking then runs in the background before the confirmation dialog enables `y` or Enter. Cancelling that dialog also preserves the selection.

Confirming leaves visual mode and opens `Enqueueing selection` while the selected entries are added to the queue in their displayed order. Browsing and further confirmations wait until this finishes; already queued deletions continue. `n` or Escape stops adding the remaining entries, while entries already queued keep their place and may continue deleting. This does not restore visual selection. `q` or Ctrl-C stops scans and opens the warning while confirmed deletions continue, including entries still being added. Confirming cancellation stops the remaining additions and all queued or active deletions. `n` or Escape returns to enqueueing if it is still underway. The enqueueing dialog closes automatically when all entries have been added; queue pause, ordering, overlap and cancellation rules apply to those entries.

## Deletion

Outside visual mode, `d` opens confirmation for the current entry. `n` or Escape cancels; `y` or Enter adds the target to the deletion queue. Further entries can be confirmed while removal runs. `?` is ignored in the deletion confirmation. `q` is ignored while no deletion is queued; with queued or active work it opens the warning while deletions finish. Confirmation waits for the identity snapshot and stays disabled until the rendered dialog has at least 18 inner columns, a visible target row and visible confirmation choices. Resize the terminal to enable confirmation; `n` or Escape can always cancel. The scan root cannot be deleted.

Outside visual mode, `D` opens the styled queue. One worker removes entries in FIFO order. The queue shows the active target, removal progress and waiting targets. Up/Down or `k`/`j` selects a row; Shift-Up/Down or `K`/`J` moves a waiting target. Space or `p` pauses and resumes waiting work. The active worker continues while the queue is paused. `d` removes a selected waiting target or cancels a selected active target; `x` or Delete removes a waiting target, and `c` cancels the active worker. `D` or Escape closes the queue.

Confirming an already queued target, or an entry inside a queued ancestor, does not add a duplicate. A newly queued ancestor replaces its waiting descendants. An active descendant finishes or stops before a queued ancestor starts. Removing a waiting target leaves its files untouched. A red `×` marks each queued or active target and its descendants in the main list and directory preview, replacing the scan spinner. Pausing the queue keeps these marks. Removing a waiting job clears its marks immediately; cancelling an active job keeps them until the worker stops. A queued ancestor keeps its descendants marked.

Queued branches stop contributing new scan work and related scan errors while deletion is pending. While a deletion is active, all scanning pauses to give removal priority; an in-flight batch may finish. Scanning resumes after the active job ends unless a quit request stopped it. Successful deletion updates the listing and ancestor totals incrementally, without a full rescan or success dialog. Cancellation is cooperative between removal steps: already removed entries stay removed, while remaining entries survive. Cancelled or failed directory deletion requests a scoped refresh of the surviving target unless a quit request stopped scans; an overlapping queued deletion delays that refresh. An entry failure does not stop removal of its siblings. After the target finishes, one report lists its remaining errors with full paths. Reports preserve open dialogs and wait in FIFO order; Enter dismisses one report and reveals the next. Up/Down, PageUp/PageDown and Home/End scroll the errors; Left/Right scroll long paths and messages. Only visible rows are rendered, including for very large reports. Confirmed deletion continues while reports are open. Permission failures trigger an anchored recovery attempt and one retry before becoming reported errors. Recovery uses the process's existing privileges; it cannot override a read-only filesystem or grant privileges the process does not possess.

Deletion requires Linux and accessible procfs. An asynchronous snapshot resolves the parent beneath a held root anchor before confirmation. The queue retains the parent and target identities; removal compares them before changing the filesystem and rejects replacement parents or targets. A missing target at snapshot time cannot authorize a replacement that appears later. Symlink ancestors are refused, and target identities are rechecked during traversal. These checks do not provide an atomic guarantee against concurrent inode reuse, renames or filesystem changes. A final symlink is unlinked without following its target. A target already absent when its queued turn arrives succeeds. Other platforms report deletion as unsupported.

## Size accounting

Allocated disk usage is the default on Unix: reported filesystem blocks multiplied by 512. Apparent size is the metadata length. Directory metadata contributes to totals. Symlinks are measured without following their targets.

Regular-file hard links are deduplicated by device and inode; duplicate entries contribute zero to totals. Attribution between directories can depend on discovery order, so totals do not predict the space reclaimed by deleting a directory. Shared extents, compression and copy-on-write clones are not deduplicated.

On non-Unix platforms, allocated size is approximated by apparent size and hard links are not deduplicated.

## Backends and platforms

On Linux, all backends keep directory reads and metadata anchored to the root descriptor. Symlink ancestors in the supplied root path are rejected; a final symlink is measured as a leaf.

The native backend uses Rustix for buffered directory reads. `auto` falls back to standard directory enumeration only when native enumeration is unsupported, before emitting entries. Metadata remains relative to the held descriptor; permission errors remain errors. Linux `std` enumeration requires accessible `/proc/self/fd`.

Other platforms use the standard backend; explicit `native` requires Linux. The non-Linux backend rejects known symlink directories but cannot fully eliminate path-replacement races. File previews on Linux use the opened scan root. Other Unix systems reopen the root path, so directory replacement can change the preview; symlinks are not followed. Non-Unix systems show file metadata only.

There is no io_uring backend. Performance depends on the filesystem, cache state and concurrent load; no universal speed claim is made.

## Upstream references

Local forks of ncdu and DUA, their pinned commits and licenses are documented in [upstream/README.md](../upstream/README.md). They have not been published as remote forks. LDU is an independent Rust implementation; neither upstream executable is built or linked into it.
