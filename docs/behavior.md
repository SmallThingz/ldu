# Behavior and platform details

## Scanning and display

The terminal opens before scanning starts. Available workers favor subtrees with fewer reported entry checks, with an allowance for directory reads already underway. Scheduling compares a bounded sample of competing siblings, so the balance is approximate. Deeper work can start while shallower directories are still being read. Hovering over or entering an unfinished directory gives its pending work priority; active reads continue. Metadata work runs in parallel batches, and browsing does not restart the scan. Progress updates use bounded channels; keyboard actions repaint immediately. Incomplete terminal input does not block resizing, scan updates or signal-driven shutdown.

Scanning and completed entries share the same sort. Near-size groups retain their previous order to reduce movement. Each group spans at most 2%; nearby entries in different groups can still exchange places. At startup, the cursor follows the first sorted row without giving its scan priority. Navigation, hovering over an entry, filtering or an entry action ends this behavior; help, sorting and display-only settings do not. Hiding dotfiles counts as filtering. Afterward, selection tracks the entry by its ID as live totals change. This startup behavior does not restart when changing directories or refreshing.

Gray Braille spinners mark folders still being scanned. Bars use fractional block glyphs. The unframed main list has clickable path components and `? help`. Components above the scan root are gray and inactive; the scan root and its descendants use bright text and can be opened by clicking their names. Narrow headers use `…` for omitted path components or the omitted part of a long name.

The main browser has a `../` row in every folder, including the scan root. Normal activation opens the immediate parent, or does nothing at the scan root. In visual mode, click or press Enter on `../` to select every direct entry, including hidden entries and entries excluded by the filter. If all are already selected, it clears them instead; Ctrl-click always clears the selection. Selecting this row clears the filter and reveals hidden entries. Selection waits for the direct folder listing, including entries discovered after the request. If scanning stops before that listing finishes, LDU keeps the known marks, cancels any pending confirmation and shows `Selection incomplete` with a request to refresh.

Queued work shows `n queued`; an active deletion with no waiting jobs shows `deleting…`. Outside selection mode, click anywhere on `n queued [D]` or `deleting… [D]` to open the queue. There is no select shortcut in the header. Clicking `? help` opens help, including while editing a filter. Selection mode shows `n selected` followed by `[d] delete`, shortened to `delete` in narrow terminals. Hovering over the count changes it to `X selected`; clicking clears the selection and leaves selection mode. Clicking delete opens the usual confirmation, including while earlier selection work settles.

At widths of at least 96 columns, the list and preview share the width in a 4:3 ratio. Both panes keep their width as the cursor moves between files and directories. Directory previews use the current sorting, size, graph and hidden-entry settings.

Text previews show the beginning of UTF-8 or BOM-marked UTF-16 files. Symlinks show their stored destination path, escaped and wrapped to fit the pane, including dangling links; the target is never opened. When a suitable archive-listing utility is installed, archives show entry names without extracting files. External helper previews are enabled on Linux, macOS, FreeBSD and DragonFly when the required utility and `/dev/fd` input path work. OpenBSD and NetBSD keep ordinary text previews and file details without spawning archive helpers. Unsupported formats and failed helper requests show file details. Preview work runs in the background with cancellation and limits on displayed output; reaching output limits adds a truncation mark. Helper processes have a time limit and receive an already-open regular file. Special files show metadata without opening their contents. Narrower terminals hide the preview and do not request file contents.

Keyboard movement leaves up to three visible rows above and below the cursor. The mouse wheel scrolls the list itself and keeps the highlight under the pointer. The margin shrinks at the ends of the list and in short terminals. Once selection follows an entry, live resorts keep the cursor on the same screen row where the margin allows. Mouse hovering keeps the viewport still, and held presses retain their painted rows. A plain click opens a directory in normal mode and toggles selection in visual mode. Right-clicking a row enters visual mode and selects it on a valid release; existing marks stay selected. Right-clicking `../` selects every direct entry, including hidden entries. Dragging with either button selects a range; a normal-mode left drag enters visual mode first. Normal clicks activate on release at the original live row; dragging or an intervening keyboard action cancels directory activation.

Once scanning and pending updates finish, the screen stops redrawing until input or new results change it. “Updating sizes” appears only while deletion or refresh cleanup is still pending; completing that cleanup restores the settled status.

Hidden entries are visible initially. Hiding dotfiles changes the listing, not the totals. Sizes remain partial while scanning, after read errors, or after deletion if refreshing the parent's metadata was interrupted. A changing filesystem is not a consistent snapshot. Input paths drop redundant `.` components and separators while preserving `..` resolution and final symlinks.

## Options

| Option | Behavior |
| --- | --- |
| `--threads N`, `-t N` (`-j` alias) | 1–128 workers; default is available parallelism, capped at 32 |
| `--backend auto\|native\|std` | Scanning backend; default `auto` |
| `--buffer-size N` | Native directory buffer in bytes; default 262144, range 4096–4194304 |
| `--batch-size N` | Metadata batch size; default 128, range 1–4096 |
| `--apparent-size` | Start with metadata lengths instead of allocated disk usage |
| `-o FILE`, `--output FILE` | Export the scanned tree in ncdu-compatible JSON; use `-` for stdout |
| `-0` | Accepted with `-o` for ncdu compatibility; exports always run without a scan UI |

The export uses ncdu’s version 1.2 JSON format and can be opened with `ncdu -f`. It contains the full scanned hierarchy and each entry’s counted sizes; hardlink aliases already excluded from LDU’s totals carry zero bytes. LDU does not export inode or device metadata. Non-UTF-8 names use replacement characters, as in the earlier JSON output. Incomplete entries have `read_error: true`, and an incomplete scan returns a nonzero exit status.

On Linux, ordinary export files are replaced after the new JSON is fully written and synced. Output symlinks retain their link and update the resolved target; other hardlink aliases keep the previous contents. Replacement requires a writable parent directory and permission to preserve the existing owner and access policy. Errors before replacement preserve an existing export. An interrupted process can leave a hidden `.ldu-export-*` temporary file. Devices, pipes and descriptor endpoints receive streaming output. Other platforms retain opened-file streaming output; a write failure can leave a partial export.

JSON includes both apparent and allocated sizes. A file or symlink root is wrapped in a parent-directory record with zero sizes. Export exit status is 0 for a completed scan, 2 for scan errors, internal scan cancellation or an incomplete scan, and 1 for setup or output failure. Process signals retain the shell’s usual termination status, such as 130 for SIGINT. Non-UTF-8 names remain intact internally. Run `ldu --help` for the complete command syntax.

## Configuration

LDU reads `$XDG_CONFIG_HOME/ldu/config` when `XDG_CONFIG_HOME` is an absolute path. Otherwise it uses `$HOME/.config/ldu/config` when `HOME` is absolute. A missing file leaves the defaults in place. LDU does not create a config file. Command-line options override file values; file values override defaults.

The config must be a regular UTF-8 file, at most 64 KiB, with one `key = value` per line; a symlink to a regular file also works. Blank lines and full-line `#` comments are allowed. Values may have matching single or double quotes; quotes have no escape syntax. Unknown keys, duplicate keys, invalid values and unreadable files produce an error. Config keys use underscores; hyphens are also accepted. Config loading applies to browsing and exports; `--help` and `--version` do not load it.

| Config key | Command-line option | Default |
| --- | --- | --- |
| `color` | `--color COLOR` | `cyan` |
| `mouse` | `--mouse[=true\|false]`, `--no-mouse` | `true` |
| `mouse_debounce_ms` | `--mouse-debounce-ms MS` | `250` |
| `mouse_debounce_delay_ms` | `--mouse-debounce-delay-ms MS` | `1250` |
| `mouse_debounce_warning` | `--mouse-debounce-warning[=true\|false]` | `true` |
| `threads` | `--threads N` | Available parallelism, capped at 32 |
| `backend` | `--backend BACKEND` | `auto` |
| `buffer_size` | `--buffer-size N` | `262144` |
| `batch_size` | `--batch-size N` | `128` |
| `apparent_size` | `--apparent-size[=true\|false]` | `false` |

Boolean values are `true` or `false`. Use `--mouse` to enable mouse input or `--no-mouse` to disable it; these options cannot be combined. For other boolean overrides, use `=`, such as `--mouse-debounce-warning=false` or `--apparent-size=false`. Scan setting ranges match the options above. The scan path and export destination are command-line arguments, not config keys.

The `color` value sets the accent on text and controls; selection, error and deletion colors retain their meanings. Accepted forms are terminal color names such as `cyan` or `bright-red`, a palette index from `0` to `255`, and `#RRGGBB`. LDU detects advertised terminal color support and chooses the nearest supported palette color when true color is unavailable. It does not change the terminal's palette. `mouse = false` disables mouse capture and mouse actions, leaving keyboard controls available.

The two debounce durations accept `0`–`60000` milliseconds. `mouse_debounce_ms` is the window after hovering changes the active entry during which normal-mode deletion needs extra confirmation time. With the defaults, deletion requested within 250 ms of that change opens a warning and delays confirmation for 1250 ms. The warning keeps a muted red background until the dialog closes, including after the delay expires; terminals with only basic colors use their red palette color. It explains that hovering changed the highlighted item before deletion. Use `--mouse-debounce-warning=false` or `mouse_debounce_warning=false` in config to hide the tint and explanation while retaining the delay. Early `y`, Enter and mouse confirmation are discarded, so a later confirmation is required. Cancellation, scrolling and quit remain available throughout. The delay expires without an animated countdown.

Only normal-mode `d` uses the guard. Moving within the same entry, wheel scrolling and layout changes do not arm it. Keyboard navigation clears it; visual-mode batch deletion and `D` for the queue are unaffected. Set `mouse_debounce_ms = 0` to disable it, or `mouse_debounce_delay_ms = 0` to remove the confirmation delay. `mouse_debounce_warning = false` suppresses the red background and explanatory text; the confirmation action still shows a brief wait until the delay expires. Normal deletion without a recent mouse entry change still accepts an immediate `d`, Enter sequence.

## Keys and dialogs

Help groups shortcuts into Normal mode, Visual mode and Common keys.

### Normal mode

| Key | Action |
| --- | --- |
| Up / `k`, Down / `j` | Move cursor |
| Shift-Up / Shift-Down | Move by a page |
| Enter | Open selected directory |
| `v` / `V` | Enter visual mode and select the current entry (`../` selects all direct entries) |
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

Dialogs have thin, sharp borders, highlighted keys and actions, and a top-right close control. The close control dismisses or cancels the current dialog. Closing the queue leaves its jobs running; closing `Stopping deletions` exits immediately. Dialog choices are compact clickable buttons. Scrollable dialogs accept the mouse wheel and show a scrollbar. When content overflows, clickable arrows appear on the bottom border, with a wheel/page-key hint where space permits. Clicking and releasing outside help, information, the queue or a confirmation cancels or dismisses it. Errors, other notices, enqueueing and deletion progress require their explicit controls.

Right-clicking anywhere in the top bar opens `Quit LDU?`. Yes exits immediately when no deletion is pending; otherwise it opens deletion progress and exits when confirmed work finishes, without another confirmation. Queue pause still applies. No or the close button cancels that quit request and restores the previous dialog or browser, including any pending error report.

Help, information and ordinary notices also scroll with Up/`k`, Down/`j`, PageUp/PageDown and Home/End. Enter, Escape, `q` or `i` closes them. `?` closes help or opens it from information and notices.

With confirmed deletions pending, the first `q` or Ctrl-C stops all scans and pending refreshes, opens `Finishing deletions`, and lets deletions finish before exiting. The dialog shows the full active target, waiting count, and current-job removed/known-item totals above `Exit now` and `Browse` buttons. The known total can grow during deletion; long paths scroll. A deletion error hidden by this dialog is reported after terminal restoration with exit status 1. Queue pause still applies. `n`, Escape or Browse dismisses it and keeps browsing; stopped scans do not restart automatically, so use `r` or `R` to refresh.

A second Ctrl-C cancels all deletions and quits, even when the terminal is too small to show the choices. `q` or Enter also confirms cancellation when the choices are visible. Cancellation waits up to one second before restoring the terminal, even if filesystem work is blocked; another Ctrl-C exits immediately during that wait. SIGINT follows the same sequence.

`r` refreshes only a selected directory; selecting a file has no effect. `R` refreshes the directory currently being browsed. Both preserve the current path, retain unrelated entries and update ancestor totals. Requests are coalesced and wait while a queued deletion overlaps their scope. Hardlink accounting remains shared across the original scan and refreshed subtrees. Fresh hardlink metadata updates its global owner even when that owner lies outside the refreshed folder. Shell spawning is unavailable.

`v` or `V` starts visual mode and selects the current entry; on `../`, it selects every direct entry. Up/Down and `j`/`k` move the cursor without changing selection. Shift-Up/Down adds a range. Enter toggles the current row and starts a new range from it; extending that range with Shift preserves an explicit deselection of its starting row. A `›` marks the cursor; `✓` marks other selected rows. Blue backgrounds indicate selection. An unselected cursor has a gray background. The header shows the selection count and a delete action as space allows. Hovering over the count shows `X selected`; clicking it clears the selection and leaves visual mode. Selection tracks stable entry IDs as sorting changes; each range retains the ordering from when it began. `v` or `V` clears selection and leaves visual mode; Escape does so when no filter is active. Navigating to another directory also clears it.

In visual mode, a plain left click toggles a row; a right click selects it on a valid release without clearing existing marks. A drag with either button selects a range when it starts on an unselected row, or deselects the range when it starts on a selected row. Right-dragging from `../` activates its select-all toggle. The starting row’s selection state is captured once after earlier selection input settles. If that row disappears before the gesture can be processed, its selection state at the press determines the direction. Ctrl-click deselects a row and Ctrl-drag removes a range from the pre-press selection; reversing a drag restores original marks outside the current range. Ctrl takes precedence over Shift. Shift-click adds a range from the selection anchor. Shift-drag starts at the pressed row and adds from an unselected origin or removes from a selected origin, like plain drag. The modifiers at mouse press determine the gesture even if Ctrl or Shift changes during the drag. Dragging at the top or bottom of the list scrolls to include offscreen entries. If pending visual input fills the bounded queue, new gestures may be ignored until it catches up. Escape and Ctrl-C remain available.

In visual mode, `d`, `D` or Delete requests batch deletion. The target and confirmation controls remain visible while earlier selection input and identity checks finish. An immediate `y` or Enter is retained for that request; deletion starts only after the selection settles, target checks succeed and the confirmation geometry is reviewable. `n` or Escape cancels the request, clears retained confirmation input and preserves the selection.

Confirming leaves visual mode and opens `Enqueueing selection` while the selected entries are added to the queue in their displayed order. Browsing and further confirmations wait until this finishes; already queued deletions continue. `n` or Escape stops adding the remaining entries, while entries already queued keep their place and may continue deleting. This does not restore visual selection. `q` or Ctrl-C stops scans and opens the warning while confirmed deletions continue, including entries still being added. Confirming cancellation stops the remaining additions and all queued or active deletions. `n` or Escape returns to enqueueing if it is still underway. The enqueueing dialog closes automatically when all entries have been added; queue pause, ordering, overlap and cancellation rules apply to those entries.

## Deletion

Outside visual mode, `d` opens confirmation for the current entry. `n` or Escape cancels; `y` or Enter adds the target to the deletion queue. Further entries can be confirmed while removal runs. `?` is ignored in the deletion confirmation. `q` is ignored while no deletion is queued; with queued or active work it opens the warning while deletions finish. Without a recent mouse entry change, an immediate `d`, Enter sequence retains confirmation while the identity snapshot is prepared. The [mouse guard](#configuration) discards early confirmation when it is active. The rendered dialog still needs at least 18 inner columns, a visible target row and visible confirmation choices before confirmation can proceed. Resize the terminal to enable confirmation; `n` or Escape can always cancel. The scan root cannot be deleted.

Outside visual mode, `D` opens the styled queue. One worker removes entries in FIFO order. The queue shows the active target, waiting targets and the queue status. Hover selects a row; click pauses or resumes waiting work, and right-click removes a waiting row or cancels the active row. The two-line footer lists the mouse controls. Up/Down or `k`/`j` selects a row; Shift-Up/Down, `K`/`J`, or dragging a waiting row moves that target. Dragging cannot reorder the active job; row identities remain stable as other jobs finish. Space or `p` pauses and resumes waiting work. The active worker continues while the queue is paused. `d` removes a selected waiting target or cancels a selected active target; `x` or Delete removes a waiting target, and `c` cancels the active worker. `D` or Escape closes the queue.

Confirming an already queued target, or an entry inside a queued ancestor, does not add a duplicate. A newly queued ancestor replaces its waiting descendants. An active descendant finishes or stops before a queued ancestor starts. Removing a waiting target leaves its files untouched. A red `×` marks each queued or active target and its descendants in the main list and directory preview, replacing the scan spinner. Pausing the queue keeps these marks. Removing a waiting job clears its marks immediately; cancelling an active job keeps them until the worker stops. A queued ancestor keeps its descendants marked.

Queued branches stop contributing new scan work and related scan errors while deletion is pending. While a deletion is active, all scanning pauses to give removal priority; an in-flight batch may finish. Scanning resumes after the active job ends unless a quit request stopped it. Successful deletion updates the listing and ancestor totals incrementally, without a full rescan or success dialog. Cancellation is cooperative between removal steps: already removed entries stay removed, and a submitted batch may finish before the worker stops. Cancelled or failed directory deletion requests a scoped refresh of the surviving target unless a quit request stopped scans; an overlapping queued deletion delays that refresh.

An entry failure does not stop removal of its siblings. After the target finishes, one report lists its remaining errors with full paths. Reports preserve open dialogs and wait in FIFO order; Enter dismisses one report and reveals the next. Up/Down, PageUp/PageDown and Home/End scroll the errors; Left/Right scroll long paths and messages. Only visible rows are rendered, including for very large reports. Confirmed deletion continues while reports are open.

A failed removal gets one retry. Permission failures trigger anchored recovery first; a nonempty directory is rescanned once before its second removal attempt. Entries that exhausted their retry are skipped during ancestor rescans, and final failures do not stop sibling deletion. Directory cycles are detected from active ancestor identities, reported once with their full paths and skipped while sibling deletion continues. Recovery uses the process's existing privileges; it cannot override a read-only filesystem or grant privileges the process does not possess.

Deletion uses descriptor-relative Unix operations on Linux, macOS and BSD. Ordinary removal does not require procfs; Linux permission repair requires accessible `/proc/self/fd`. An asynchronous snapshot resolves the parent beneath a held root anchor before confirmation. The queue retains the parent and target identities; removal compares them before changing the filesystem and rejects replacement parents or targets. A missing target at snapshot time cannot authorize a replacement that appears later. Symlink ancestors are refused, and target identities are rechecked during traversal. These checks do not provide an atomic guarantee against concurrent inode reuse, renames or filesystem changes. A final symlink is unlinked without following its target. A target already absent when its queued turn arrives succeeds. Non-Unix platforms report deletion as unsupported.

## Size accounting

Allocated disk usage is the default on Unix: reported filesystem blocks multiplied by 512. Apparent size is the metadata length. Directory metadata contributes to totals. Symlinks are measured without following their targets.

Regular-file hard links are deduplicated by device and inode; duplicate entries contribute zero to totals. Attribution between directories can depend on discovery order, so totals do not predict the space reclaimed by deleting a directory. Shared extents, compression and copy-on-write clones are not deduplicated. A scan traverses each directory device/inode identity once. Repeated directory aliases, bind-mount cycles and directory hard links are skipped with a partial-read error rather than traversed again.

On non-Unix platforms, allocated size is approximated by apparent size and hard links are not deduplicated.

## Backends and platforms

Unix backends keep directory reads, metadata, deletion and previews anchored to the opened root descriptor. Symlink ancestors in the supplied root path are rejected; a final symlink is measured as a leaf. File previews use the held scan root rather than reopening its display path. These checks preserve the anchor when a pathname changes; they do not make a changing filesystem an atomic snapshot.

On Linux, the native backend uses Rustix for buffered directory reads. `auto` falls back to standard directory enumeration only when native enumeration is unsupported, before emitting entries. Metadata remains relative to the held descriptor; permission errors remain errors. Linux `std` enumeration and permission repair require accessible `/proc/self/fd`; native enumeration and ordinary descriptor-relative deletion do not.

On macOS and BSD, `auto`, `native` and `std` all use descriptor-relative metadata and the platform's buffered `fdopendir`/`readdir` stream. The batch-size setting applies; the Linux directory-buffer-size setting does not change these streams. Metadata lookup and deletion run on the existing bounded worker threads, keeping the interface responsive without a separate async filesystem runtime.

Linux and FreeBSD retain ancestors through `O_PATH`, which does not require reading their contents. macOS uses `O_EVTONLY`; the inherited process I/O policy can still require read permission on those directories. Other BSD systems use read-only directory handles. If an ancestor cannot be opened safely, the operation reports the permission failure. LDU does not relax anchoring or change the process I/O policy to work around it. The same restriction can prevent permission recovery on a directory that cannot be opened.

macOS, FreeBSD, OpenBSD, NetBSD and DragonFly use kqueue for terminal and deletion-completion readiness. If setup, registration or waiting fails, that waiter switches to ordinary descriptor waiting: `select` on macOS, `poll` on the BSDs. The fallback retains the current wait deadline and cancellation behavior. kqueue is a readiness mechanism; it does not perform metadata lookup or unlink operations. Both paths sleep while waiting instead of busy-polling.

Local release packaging covers macOS and FreeBSD on x86-64 and ARM64 alongside the four Linux musl targets. The broader BSD source paths need their own compiler and native runtime qualification. A cross-build or a valid executable header alone does not establish that scanning, deletion or terminal interaction works on that OS. See [packaging and runtime checks](releasing.md). Non-Unix systems retain path-based scanning and file metadata previews; they do not support anchored deletion.

On Linux x86-64, ARM64 and RISC-V64, `auto` tries io_uring for metadata. Explicit `native` and `std` keep blocking metadata calls. Deletion independently tries io_uring for batches of known non-directory entries; directory reads and directory removal keep their blocking paths. ARMv7 uses the blocking implementation because the current ring bindings do not support that target.

Ring setup and the required operations are checked before use. Each batch has at most 64 outstanding operations on an existing filesystem worker, without kernel submission polling (`SQPOLL`). Unavailable rings use the existing threaded metadata and blocking deletion paths. Cancellation stops new submissions; accepted removals can still finish. An unknown removal result after a driver failure is reported without automatically repeating the removal.

Performance depends on the filesystem, cache state, kernel and concurrent load.

## Upstream references

Local forks of ncdu and DUA, their pinned commits and licenses are documented in [upstream/README.md](../upstream/README.md). They have not been published as remote forks. LDU is an independent Rust implementation; neither upstream executable is built or linked into it.
