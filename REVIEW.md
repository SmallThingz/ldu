# Review qualification

Completion requires two consecutive adversarial review passes, each using all six reviewer slots, with no valid findings. A valid finding and its fix reset the clean-pass count. Tests and measurements are separate evidence and do not count as review passes.

Current consecutive clean passes: **2**. The active revision passed release qualification and consecutive saturated Passes 29 and 30 on unchanged source. Publication is authorized. Passes 22 and 23 qualify the earlier commit `9ffe71e` only.

| Pass | Reviewers | Result | Status |
| --- | --- | --- | --- |
| 1 | 6 of 6 | Findings; not clean | Closed; fixes completed |
| 2 | 6 of 6 | Findings; not clean | Closed; help wording carried into Pass 3 |
| 3 | 6 of 6 | Findings; not clean | Closed; fixes completed |
| 4 | 6 of 6 | One finding; five reviewers reported none | Closed; coverage-summary fix completed |
| 6 | 6 of 6 | Two findings; four other reviewers reported none | Closed; fixes completed |
| 7 | 6 of 6 | One finding; five reviewers reported none | Closed; fix completed and qualified |
| 8 | 6 of 6 | One finding; five reviewers reported none | Closed; fix completed and verified |
| 9 | 6 of 6 | No findings | Clean; streak reached 1, then reset by Pass 10 |
| 10 | 6 of 6 | One unique finding reported by two reviewers; four reported none | Closed; fix completed and qualified |
| 11 | 6 of 6 | No findings | Clean; streak 1 |
| 12 | 6 of 6 | No findings | Clean; streak 2; prior version qualified |
| 13 | 6 of 6 | Three findings; four reviewers reported none | Closed; fixes and qualification complete; streak 0 |
| 14 | 6 of 6 | Two findings; four reviewers reported none | Closed; fixes and qualification complete; streak 0 |
| 15 | 6 of 6 | No findings | Clean; streak 1 |
| 16 | 6 of 6 | No findings | Clean; streak 2; published baseline qualified |
| 17 | 6 of 6 | Four findings; two reviewers reported none | Fixed and requalified; streak 0 |
| 18 | 6 of 6 | No findings | Clean; streak 1, reset by Pass 19 |
| 19 | 6 of 6 | Two findings; four reviewers reported none | Fixed and requalified; streak 0 |
| 20 | 6 of 6 | One finding; five reviewers reported none | Fixed and requalified; streak 0 |
| 21 | 6 of 6 | Two findings; four reviewers reported none | Fixed and requalified; streak 0 |
| 22 | 6 of 6 | No findings | Clean; streak 1 |
| 23 | 6 of 6 | No findings | Clean; streak 2; previous revision qualified |
| 24 | 6 of 6 | Four unique findings; two reviewers reported none | Fixed and requalified; streak 0 |
| 25 | 6 of 6 | Four findings; three reviewers reported none | Fixed and requalified; streak 0 |
| 26 | 6 of 6 | Two findings; four reviewers reported none | Fixed and requalified; streak 0 |
| 27 | 6 of 6 | Three findings; three reviewers reported none | Fixed and requalified; streak 0 |
| 28 | 6 of 6 | Two findings; four reviewers reported none | Fixed and requalified; streak 0 |
| 29 | 6 of 6 | No findings | Clean; streak 1 |
| 30 | 6 of 6 | No findings | Clean; streak 2; current revision qualified |

Pass 1 findings, now closed:

- Missing ncdu `<` parent-navigation alias and a documented `e` hidden-entry toggle that was unsupported.
- Refresh messages could mask subsequent scan errors.
- Scanner temporary fixtures were outside the project `.tmp` directory.
- Benchmark maximum-thread handling and filename/navigation fixtures needed corrections.
- Root paths with trailing symlinks and changed symlink ancestors could escape the intended scan boundary.
- Sorting and linear selection searches on the input thread could delay interaction for wide directories.

Pass 2 findings, now closed; the help wording was completed in Pass 3:

- The first size snapshot could order near ties alphabetically instead of by size.
- Retiring the previous model during refresh could block the UI thread.
- Deletion-dialog scrolling used an incorrect maximum for wrapped paths.
- Information, help and notice dialogs lacked scrolling.
- Header clipping could split Unicode grapheme clusters.
- Help promised previous order for every pair within 2%, although the policy operates within bounded groups.
- Running qualification scripts with Python `-O` disabled assertions.
- A queued Clippy check identified a mode-zero literal that needed correction.

Pass 3 findings, now closed:

- A tiny deletion-dialog viewport still allowed confirmation when the target and confirmation text were not visible.
- Valid deep directory paths could fail with `NAMETOOLONG` without falling back to component-by-component opening.
- Terminal teardown occurred after other UI resources were dropped instead of restoring the terminal first.
- The unconditional near-tie help literal remained unchanged after Pass 2.
- The PTY qualification predicate incorrectly rejected wrapped help text.
- A TestBackend helper treated wide-character continuation cells as displayed text and failed incorrectly.

Pass 4 finding, now closed:

- Reused-corpus PTY runs skipped the owned-fixture hidden-entry and deletion checks but still named them in the success summary. The summary must identify skipped checks accurately.

An attempted Pass 5 was interrupted after only the I/O reviewer was launched. It was neither saturated nor completed and does not count as a review pass.

The newer UI/deletion requirements reset qualification to **0** clean passes at that stage: incremental deletion without a full rescan or persistent success dialog, immediate keyboard repainting, a gray one-cell pending prefix, subdirectory previews at widths of at least 120 columns, and concise highlighted actions in thin bordered dialogs. The revised implementation was subsequently frozen and qualified before Passes 11 and 12.

Pass 6 findings, now closed:

- The subdirectory preview retained regular-file rows in its background ordering state instead of filtering them before insertion.
- The README described recursive item counts as child counts. The labels are corrected.

Supplemental reviews after Pass 6, separate from the saturated pass:

- Folder navigation cleared a retained-capacity positions map on the UI thread. Fixed by replacing the old map instead of clearing it.
- The preview could say "No subdirectories" while ordering or scanning was still pending. Fixed to distinguish pending work from an empty result.
- PTY selection checks could skip `j`/`k` before an ordering snapshot arrived. The mandatory multi-entry movement check is corrected and frozen.

These focused reviews did not count as saturated passes. Their validation is recorded below; the clean-pass count was **0** at that stage.

Pass 7 finding, now closed:

- A live subdirectory could arrive after a ready-but-empty preview snapshot and finish scanning before the next snapshot. The preview then falsely showed "No subdirectories." Fixed by tracking an eligible child within bounded state.

Pass 8 finding, now closed:

- Benchmark provenance ignored a failed `git rev-parse HEAD` and could report the literal `HEAD` as the source commit in an unborn repository. Fixed by verifying the revision and checking the command status, otherwise reporting `uncommitted`. Direct verification covered the real unborn repository, failed stdout containing `HEAD`, and a valid commit hash.

Pass 9 was fully saturated and clean: all six reviewers reported no findings. The clean streak reached **1**.

Pass 10 finding, now closed:

- The main list could accept an older ready-but-empty ordering snapshot after live children arrived, then falsely report an empty directory if scanning finished before the next snapshot. Two reviewers independently reported this one finding. Fixed using an eligible-child witness and bounded rechecking, with shared visibility eligibility for the main list and preview.

The Pass 10 correction reset the clean streak to **0**. After fresh qualification, all six reviewers in Pass 11 and all six in Pass 12 reported **NO FINDINGS**. The implementation stayed unchanged between these consecutive passes, satisfying the review requirement.

Validation history:

- Before Pass 2 fixes: 32 passing tests and a passing PTY qualification run.
- During Pass 3: 33 unit tests attempted; 32 passed and one display-helper test failed. Integration and release qualification were not reached in that attempt.
- Prior-version baseline, before the new UI/deletion requirements: **35 unit and 6 integration tests passed (41 total)**; Clippy with warnings denied, the release build, and enhanced PTY checks using the CLI's default worker count all passed. These checks do not qualify the revised implementation.
- Revised implementation, before Pass 6 fixes: **47 unit and 7 integration tests passed (54 total)**; Clippy with warnings denied, the release build, and strengthened PTY checks with fresh owned fixtures passed. This evidence does not qualify the subsequent fixes or count as a clean review pass.
- After Pass 6 and supplemental Rust fixes: **49 unit and 7 integration tests passed (56 total)**; formatting checks, Clippy on all targets with warnings denied, and the release build passed.
- An initial fresh PTY attempt failed the "normal view still has panel borders" check after closing help. After correcting redraw synchronization, the strengthened full fresh PTY run passed, including mandatory multi-entry movement checks. This and the 56-test Rust qualification preceded the Pass 7 fix.
- After the Pass 7 fix, before the Pass 10 correction: **51 unit and 7 integration tests passed (58 total)**; formatting checks, Clippy with warnings denied, the release build, and the full fresh strengthened PTY run passed.
- Final qualification after the Pass 10 correction: **54 unit and 7 integration tests passed (61 total)**; formatting checks, Clippy on all targets with warnings denied, the release build, and the full fresh strengthened PTY run passed. Evidence: `.tmp/qualification-final.log`. The benchmark source-metadata helper also passed its direct verification checks.

The 25,000-file benchmark of the prior release ended without accepted data. Its artifacts are invalid baseline evidence. The subsequent 25,000-file matrix attempt ended while waiting for the shared host to become quiet. It produced no accepted JSON or timings; only busy/hold messages were recorded. A final single host-quiet gate also exited 75; `.tmp/host-quiet-final.log` and its status file record the attempt. No timings have been accepted. No benchmark controller remains active, and no performance result is claimed.

At that stage, changes remained uncommitted. The release executable is `target/release/ldu`; it has not been installed on `PATH`. Logs under `.tmp` are ignored local verification artifacts.

## Latest spinner and navigation revision

The latest requirements reset the clean-pass count after the previously qualified version. Development reviews found and addressed the following before release qualification:

- Static pending markers and divergent preview formatting: shared row rendering now uses animated one-cell Braille spinners and fractional bars in both panes.
- Hover ordering resets and unsorted provisional entry frames: one prewarmed directory cache supplies sorted main and preview views.
- Input-thread snapshot destruction and publication contention: immutable shared snapshots and lock-free publication keep large cleanup on the worker.
- Selection jumps during live sorting: movement temporarily holds the displayed order, preserves entry identity and anchors its screen row when updates arrive.
- Hidden spans losing Home/End or relative movement: bounded searches carry unresolved navigation intent; entry actions cannot use its previous target.
- Overflowed ancestor updates, starved replay and omitted parent links: fair bounded feeds separate ordered structural discovery from dirty replay and coalesced updates.
- Whole-arena growth and repeated ancestry checks: segmented node storage avoids moving existing nodes; liveness is constant time before deletion.
- Stale empty-list labels: exact direct-child witnesses distinguish filtered emptiness from pending enumeration.
- Worker queue backpressure: blocked feeds yield without spinning; pending configuration is retried even without further row changes.

README structure follows [SmallThingz/reverb](https://github.com/SmallThingz/reverb/blob/master/README.md). The requested [Unslop skill](https://github.com/theclaymethod/unslop) was installed from commit `17ed39c9d0b522f44190ff0c6233867eadee192a` and applied to the README. Its phrase, structure and silhouette scanners reported no flags with the documentation genre. The contextual review found no prose defects, so the README remained unchanged after diagnosis. Unrelated Matomo skills were archived outside the active skill catalogue.

The user authorized committing and publishing the qualified source to [SmallThingz/ldu](https://github.com/SmallThingz/ldu). The new qualification and completed saturated review pair are recorded below.

### Pass 13

All six reviewers completed adversarial review of frozen source `beb61e9f4d75f262987212ae2b6b8c4ae6fa1b6d46637bd28b4fde27d096b5ec`. Four reported no findings. Two reviewers found three issues:

- Relative paging counted raw snapshot positions instead of visible rows after hiding entries.
- Viewport anchoring counted filtered or deleted rows, allowing a second cursor jump on publication.
- PTY widget checks searched filenames for bracket, corner and footer text, falsely rejecting legal external corpus names.

The three findings are closed. The clean-pass count remains **0** before the next saturated pair.

Qualification before this pass: 69 unit tests and 7 filesystem integration tests passed, along with formatting, all-target Clippy with warnings denied, release build and the full PTY checks. An earlier tiny-dialog harness run timed out; isolated batched/separate key checks and subsequent full reruns passed. The redundant manual SIGWINCH after TIOCSWINSZ was removed as a harness defect. This does not establish it as the timeout's cause. The latest full run preserved the deletion-refusal and fixture-survival assertions and passed.

Development review of the correction also found upward scrolling jumping a page, an exhausted cursor preventing new cached rows from appearing, and deletion invalidating either a provisional target or an earlier counted row. These were corrected before the new review pair. The earlier-counted-row regression failed with destination 2511 instead of 2512 before the deletion cursor reset; both deletion cases pass afterward.

Latest frozen source: `0ee851611e4b673631d7fd3244b0e8ecfdf9f787e15002fd9850d47c36663b23`. The digest covers sorted paths and bytes separated by NULs, excluding this review ledger. Qualification after all corrections: **75 unit and 7 filesystem integration tests passed (82 total)**; formatting, all-target Clippy with warnings denied, release build, full owned-fixture PTY checks and a reused corpus with legal bracket/corner/footer/Help filenames all passed. Local evidence: `.tmp/qualification-visible-nav-final.log` and `.tmp/qualified-visible-nav.json`. No performance timings are claimed.

### Pass 14

All six independent reviewers were running concurrently and completed review of source `0ee851611e4b673631d7fd3244b0e8ecfdf9f787e15002fd9850d47c36663b23`. Four reported no findings. Two valid findings reset the clean-pass count to **0**:

- Nested directory totals could saturate while only the scan root received an overflow warning, leaving the nested branch labeled accurate. Saturation must remain marked through deletion because precision cannot be recovered by subtracting from a clamped total.
- Reused-corpus terminal navigation checked raw directory names against the escaped display, rejecting legal newline or backslash names.

Both findings are corrected and qualified. Development review also corrected root/child prefix collisions in the header predicates. The harness retains its ASCII-width, fully visible synthetic-directory-path contract, now stated in its `--root` help.

Latest frozen source: `98616a71236d9ece5dc3a2606500ef80093ebf259b9ff4981f4c5d0cdcccf44c`. After these corrections, **75 unit and 7 filesystem integration tests passed (82 total)**, plus formatting, all-target Clippy with warnings denied, release build and four real-PTY corpus runs: the complete owned fixture, legal widget-like filenames, escaped directory names and a root/child name-prefix collision. The existing model regression now covers independent nested batch/cross-batch overflows, warning persistence after deletion, an exact `u64::MAX` boundary, and own-metadata replacement. Local evidence: `.tmp/qualification-overflow-final.log` and `.tmp/qualified-overflow.json`. The clean streak is **0** before the next pair.

### Pass 15

All six fresh reviewers were active concurrently and independently reported **NO FINDINGS** for frozen source `98616a71236d9ece5dc3a2606500ef80093ebf259b9ff4981f4c5d0cdcccf44c`. Domains covered navigation/input, ordering/cache lifetime, model/accounting, filesystem/scanner/deletion, terminal rendering/harness, and documentation/dependencies/provenance. No source changes or new qualification runs occurred during the pass. The clean streak is **1**; a second saturated clean pass is required.

### Pass 16

All six fresh reviewers were active concurrently and independently reported **NO FINDINGS** on the same source as Pass 15. They reviewed filesystem/scanner/deletion, accounting, navigation/input, publication/docs/dependencies, terminal rendering/harness, and cache ordering/lifetime. Both saturated passes completed after release qualification, with no source changes between them. The baseline clean streak is **2** and that published implementation is qualified. The source and staged publication bytes were independently rechecked against the qualification digest before committing.

## Active revision: deletion queue, selection and scan priority

The previous clean pair qualifies the published baseline only. New deletion, scoped refresh, visual selection and focused scanning requirements reset the clean-pass count to zero. The record below tracks development, release qualification and fresh saturated review passes for this revision.

Development checks for the active revision, before formal review:

- Debug qualification reached 141 unit and 7 filesystem tests; three Clippy cleanup findings were corrected. Earlier checks passed smaller development checkpoints. These results do not qualify the final revision.
- A real PTY run exposed Crossterm losing keyboard readiness when resize and input arrived together. Level-triggered polling and immediate event consumption address the defect; the harness now queues resize and End together against its stopped, owned child. Final verification is pending.
- Independent probes found unbounded cancelled batch and single-target capture admission, stale focus after scoped refresh or automatic selection, a modal mouse-release leak, and a visual-mode queue hint mismatch. Corrections and final qualification are pending; these probes do not count as saturated passes.

The later development checkpoint passed **156 unit and 7 filesystem tests (163 total)** plus all-target Clippy with warnings denied. Its full debug PTY run reached the visual tests and exposed a swallowed immediate batch retry after cancellation. The isolated owned-child resize-plus-input regression passed. A further saturated development probe found rapid Enter parity, mouse-press ordering and zero-charge root-metadata notification defects. These fixes require fresh final qualification; development probes remain separate from formal passes.

### Active revision release qualification

All development findings above are closed. Final frozen source: `e10889814d4f6bcd833c4fb2341a52667bf3cc382f882870b52daaad04c4db8d` across 24 source/publication paths, excluding this ledger. **160 unit and 7 filesystem tests passed (167 total)**; formatting, all-target Clippy with warnings denied, release build, four real-PTY corpora and nine native/std/auto JSON crosschecks passed. The owned terminal suite observed active cancellation with surviving files and checked FIFO/reordering/overlap, guarded shutdown, scoped refresh/hardlinks, visual keyboard/mouse batch deletion and simultaneous resize/input.

The first release harness attempt required an intermediate Scanning frame on a fixture that completed before the next repaint. The corrected predicate accepts the completed expected root, preserving mandatory initial Waiting to scan before filesystem work. All four corpora passed after that harness correction. Evidence: `.tmp/qualification-deferred-debug.log`, `.tmp/qualification-queue-release.log` (successful build; old harness failure), `.tmp/qualification-queue-release-final.log`, and `.tmp/qualified-queue-selection.json`. No performance timings are claimed. Clean streak remains **0** before fresh formal review.

### Pass 17

All six fresh reviewers were verified running concurrently on frozen qualified source `e10889814d4f6bcd833c4fb2341a52667bf3cc382f882870b52daaad04c4db8d`. Review domains: scanner/filesystem, queue/deletion/capture, model/accounting, input/selection, rendering/terminal harness, and cache/docs/dependencies. All six completed before any source changes. Two reported no findings; four each reported a distinct valid finding, resetting the clean streak to **0**:

- Rapid Enter / move / Enter during unfinished visual work lost the ordered toggles. Pending clicks and gestures require the same ordering guarantees.
- A retained directory replaced by a file could remain in the preview until background cache publication; preview rendering must check the current kind.
- Retiring a completed scoped handle destroyed its exclusion tree on the input thread. Retirement needs bounded off-thread ownership and deep trie destruction must be iterative.
- Incomplete terminal UTF-8 or escape input could block polling, repaint and controlled shutdown. An owned-child probe confirmed that a lone `0xC3` stopped repainting and SIGTERM did not exit until `0xA9` arrived. Raw reads must return when buffered bytes are exhausted.

Fixes and fresh qualification are in progress. The prior qualified manifest is preserved as `.tmp/qualified-pass17.json`; source `e1088981…c4db8d` does not qualify the fixes. Two new clean saturated passes are required after qualification.

### Qualification after Pass 17 fixes

All four findings and related ordered-input edge cases are fixed. Frozen source: `4946bde852c3d6bda1c02fe6c3eee2556e70810341aa562a899eab95bfe80def` over 25 paths, excluding this ledger. **168 unit and 7 filesystem tests passed (175 total)**, plus formatting, all-target Clippy with warnings denied, release build, four real-PTY corpora and nine native/std/auto JSON crosschecks. The terminal suite verifies fragmented UTF-8/Escape completion, repaint and SIGTERM without the remaining byte, and again observed active cancellation with surviving files. Input fixture setup now waits for exact default ordering before testing End/Home, avoiding an asynchronous sort-toggle race.

Evidence: `.tmp/qualification-pass17-fixes-release.log`, `.tmp/qualification-pass17-fixes-pty.log`, and `.tmp/qualified-queue-selection.json`. No source changes are planned during the next pair of six-agent reviews. Clean streak: **0**.

### Pass 18

All six fresh reviewers were verified running concurrently on qualified source `4946bde852c3d6bda1c02fe6c3eee2556e70810341aa562a899eab95bfe80def`. Domains cover filesystem/scanner/terminal reads, queue/deletion/batch, accounting, input/selection, rendering/harness, and cache/dependencies/docs. All six independently reported **NO FINDINGS**. Source remained unchanged throughout the pass. Clean streak: **1**; another saturated clean pass is required.

### Pass 19

All six fresh reviewers were verified running concurrently on the same qualified source as Pass 18 and completed before any source changes. Four reported no findings. Two distinct valid findings reset the clean streak to **0**:

- Deferred Shift-click/drag committed a new cursor without requesting scan priority.
- A background deletion failure replaced an open batch confirmation, orphaning its preparation worker.

Development review also identified cursor replacement after filtering/deletion retaining old scan priority, and successful reconciliation clearing a deletion error hidden behind another modal. Fixes request priority at those cursor transitions, preserve active dialogs, and retain one brief pending failure notice until a dialog can be shown. Fresh qualification and two saturated clean passes are required.

### Qualification after Pass 19 fixes

Frozen source: `eaebb344aeaa5088a366a94fd1139b6b75035bc4337261126ca7ed3da99df407` over the same 25 publication paths. **171 unit and 7 filesystem integration tests passed (178 total)**, plus formatting, all-target Clippy with warnings denied, release build, four real-PTY corpora and nine native/std/auto JSON crosschecks. Regressions cover deferred mouse scan priority, batch preparation surviving an unrelated deletion failure and successful reconciliation, cancellation/retirement/retry, and failure-notice or quit-dialog preservation during accepted batch confirmation. Development review also caught and fixed the test fixture root mismatch and confirmation clearing an intervening notice. The staged source bytes match the frozen digest. Evidence: `.tmp/qualification-pass19-fixes.log` and `.tmp/qualified-queue-selection.json`. Clean streak: **0** before two fresh saturated passes.

### Pass 20

All six fresh reviewers were verified running concurrently on qualified source `eaebb344aeaa5088a366a94fd1139b6b75035bc4337261126ca7ed3da99df407`. Domains: queue/deletion, input/selection, scanner/filesystem, accounting, terminal rendering, and cache/lifetime/provenance. All six completed before source changes. Five reported no findings; one found that removal cleared a visible obsolete deletion confirmation but did not clear the same confirmation suspended behind the quit guard. Cancelling quit could restore a dialog for a vanished target. The correction applies identical target invalidation to both dialog slots. A generic regression covers visible/suspended Delete, Info, Help and failure dialogs. Fresh qualification and two clean saturated passes are required; streak **0**.

### Qualification after Pass 20 fix

Frozen source: `4c7788b52c2afed6f68fa40654f80f5010a300c5b313620f0d2c3d3b1b7b0de7` over 25 publication paths. **172 unit and 7 filesystem integration tests passed (179 total)**, plus formatting, all-target Clippy with warnings denied, release build, four real-PTY corpora and nine native/std/auto JSON crosschecks. The new generic test exercises target invalidation in visible and suspended dialogs while preserving Help and unrelated failure notices.

Earlier terminal runs exposed two harness readiness assumptions: navigation accepted a retained first-row frame before configured Home completed, and fragmented-input setup expected both rows visible despite initial cursor anchoring during size updates. The harness now observes End/Home transitions before directory entry, and explicitly establishes name order and Home in input fixtures. The complete final suite passed with these corrections. Failed attempts remain in `.tmp/qualification-pass20-fix.log` and `.tmp/qualification-pass20-fix-final.log`; final evidence is `.tmp/qualification-pass20-ready.log` and `.tmp/qualified-queue-selection.json`. No performance timings are claimed. Clean streak **0** before fresh review.

### Pass 21

All six fresh reviewers were verified running concurrently on the qualified source above. Domains cover scanning/filesystem, queue/deletion, input/selection, accounting, terminal rendering/harness, and cache/lifetime/provenance. All six completed before source changes. Four reported no findings. Two valid findings keep the clean streak at **0**: target actions could use the previous cursor while cursor-changing visual intents waited, and cancelled metadata capture could delay terminal restoration after actual deletion stopped. The corrections share one bounded cursor-pending predicate across action/toggle decisions and await only active destructive deletion on exit. A generic action regression covers queued Home, click, hover and gesture intents while allowing stable-only toggles. Fresh qualification and two clean saturated passes are required.

### Qualification after Pass 21 fixes

Frozen source: `e79f19183b267a549fbbc7dd3f7ee8a303e6344a5dccb2b4b81da9ffbfefa3a4` over 25 publication paths. **173 unit and 7 filesystem integration tests passed (180 total)**, plus formatting, all-target Clippy with warnings denied, release build, four real-PTY corpora and nine native/std/auto JSON crosschecks. A generic regression exercises queued Home/click/hover/gesture against five target actions and permits stable-only toggles. Static ownership review confirms cancelled capture cannot dispatch deletion after exit; active deletion remains the shutdown barrier.

The first terminal attempt observed a retained victim row after a filter change before Home committed. The action guard safely refused deletion. Navigation and deletion checks now share an End/Home acknowledgement helper; all final checks passed. Failed evidence is `.tmp/qualification-pass21-fixes.log`; final evidence is `.tmp/qualification-pass21-ready.log` and `.tmp/qualified-queue-selection.json`. Source stays frozen during the next pair; clean streak **0**.

### Pass 22

All six fresh reviewers were verified running concurrently on qualified source `e79f19183b267a549fbbc7dd3f7ee8a303e6344a5dccb2b4b81da9ffbfefa3a4`. Domains cover input/selection, queue/deletion, scanner/filesystem, accounting, rendering/terminal, and cache/lifetime/provenance. All six independently reported **NO FINDINGS**. Source remained unchanged throughout; clean streak **1**. One further saturated clean pass is required.

### Pass 23

All six fresh reviewers were launched before the first final report on the same qualified source as Pass 22. Domains cover filesystem/scanner/terminal reads, selection/input, queue/deletion, accounting, rendering/harness, and cache/lifetime/provenance. All six independently reported **NO FINDINGS**. Both saturated passes completed after release qualification with no source changes between them. Clean streak **2**; the final revision is qualified for authorized publication. Working and staged source bytes were verified against the qualification digest before committing.

## Active revision: complete visual interaction

The user reported that visual selection remained incomplete. Development audits confirmed ignored lowercase `v`, ignored visual Delete, single-target lowercase `d`, lost batch requests during pending range work, unsupported ordinary dragging, drag modifier-release and edge-scroll gaps, newly discovered rows missing frozen ranges, explicit Enter deselection lost on subsequent movement, and opposite queued movements incorrectly collapsed at boundaries. These findings reset the clean streak to **0**.

Six development agents are working on keyboard/batch admission, mouse gestures, selection semantics, terminal regression coverage, documentation/requirements and independent batch review. Development audits do not count as formal clean passes. Final qualification and the new saturated review pair are pending.

Follow-up development review also found visual entry overtaking unresolved navigation, premature Enter cancelling later batch-wait phases, and mouse endpoint/order/timer cases. These corrections are included in the active revision. An intermediate keyboard/selection checkpoint passed **179 unit and 7 filesystem tests (186 total)**; it predates the mouse changes and is not final qualification.

Active visual revision is frozen at `6e73663c493a4639494669dc28be63088d9a815da8e3b3e6dfddc069943910f7` across 25 source/publication paths (this ledger excluded). **185 unit and 7 filesystem tests passed (192 total)**; formatting, all-target Clippy with warnings denied, release build, four real-PTY corpora, and nine native/std/auto JSON crosschecks passed. Focused terminal checks also exercised lowercase mode entry, all batch aliases, plain drag, released-Shift inversion, an 8,192-row pending range, premature Enter, and timer-driven edge scrolling. A queue-fixture path overflow was corrected by using 119 columns without weakening exact-path checks. Evidence: `.tmp/visual-mouse-development.log`, `.tmp/qualification-visual-complete.log`, and `.tmp/qualified-visual-complete.json`.

Unslop audit found no phrase or structure defects in README or behavior documentation. Soft readability/repetition metrics and the behavior document's heading-overlap warning were reviewed and retained as normal technical documentation. No prose rewrite was warranted.

Formal Pass 24 is starting with six fresh adversarial reviewers. Clean streak remains **0**.

### Pass 24

All six fresh reviewers were verified active concurrently before the start barrier was released. Four reviewers reported four unique findings; two reported no findings. The source remained unchanged until all six completed:

- Adjacent queued Enter pairs were discarded despite each toggle also committing and reanchoring the range.
- A mouse press baseline could restore a selected ID after asynchronous deletion.
- Ordinary clicks or hover on blank list rows were clamped to the final entry.
- An unrelated, unconfirmed batch dialog prevented previously confirmed FIFO jobs from starting.

All four fixes are implemented. Toggle intents retain their side effects; deletion removes IDs from the retained mouse baseline; only active drags clamp blank hits, while wheel events still navigate over blank list space; confirmed queue jobs continue during batch review. Grouped regressions cover both toggle variants, ordinary/Shift and immediate/deferred gestures, blank clicks/hover, wheel input and edge drags. A real-PTY case uses 32 paused confirmed jobs and one independent batch target to verify FIFO progress while the new confirmation remains open.

Post-fix Rust checks passed **187 unit and 7 filesystem tests (194 total)** plus all-target Clippy with warnings denied. Release build and full terminal qualification are pending. The clean streak remains **0**.

After the Pass 24 fixes, frozen source is `d502dd6a68bd251d893094b58fffa0d990888e70c1b3b36aa1d056384404ea4a`. **187 unit and 7 filesystem tests passed (194 total)**, with formatting, all-target Clippy, release build, four real-PTY corpora and nine JSON backend crosschecks. The full terminal suite includes confirmed FIFO progress during an unconfirmed batch. Evidence is in `.tmp/visual-pass24-fixes.log`, `.tmp/qualification-visual-complete.log`, and `.tmp/qualified-visual-complete.json`. Formal Pass 25 is starting; clean streak **0**.

### Pass 25

All six fresh reviewers were verified active before release of the start barrier, and the source stayed frozen until all six reports completed. Three reviewers reported no findings. The other three found four issues:

- Deleting an original range anchor or a shared segment endpoint could make subsequent movement skip intermediate rows.
- Deleting either endpoint of a queued drag discarded the entire surviving interval.
- A mouse press after an unpainted sort could capture the painted order while freezing a different view.
- A later confirmed deletion could overtake the undispatched tail of an earlier confirmed batch.

The fixes retain logical frozen intervals despite endpoint removal, recover new-segment boundaries within the pump budget, preserve the exact painted view and viewport during held gestures, and hold an explicit enqueueing dialog until a confirmed batch has entered the FIFO. Already queued deletions continue; stopping additions leaves their accepted prefix intact. Development and requalification are in progress; clean streak **0**.

Pass 25 fixes are complete and qualified. The frozen publication-source digest is `f7b50c54bc32ad9a766a39f30485aa9d59598c0ba70c52503a1ca0c3052fe8c6` (all tracked paths except this ledger). Validation passed: 197 unit tests, 7 filesystem integration tests, formatting, warnings-denied Clippy, release build, full owned-fixture PTY suite, three reused-corpus PTY runs, and nine native/std/auto JSON checks. Evidence: `.tmp/visual-pass25-fixes.log`, `.tmp/qualification-visual-complete.log`, and `.tmp/qualified-visual-complete.json`.

The PTY range-test setup now proves that all 8,192 rows have reached the sorted view before the regression sequence. Scanning completion alone was insufficient. Readiness probes use fresh visual selections and one deadline; the actual stopped-child End + delete + Enter burst is unchanged and executes once. The corrected focused check and full terminal qualification passed. Current clean streak remains zero; Pass 26 begins on this frozen source.

### Pass 26

All six fresh reviewers were verified running simultaneously before RELEASE. The source stayed frozen until all six final reports. Four reported no findings; two reported one finding each:

- A Shift-click with no prior range could lose intermediate surviving rows when its original visual anchor was deleted between press and release.
- Batch admission could observe an empty event channel, then observe worker completion after the final events were published, and incorrectly discard those confirmed events as a failure.

Both findings reset the clean streak to zero. Fixes and deterministic regression coverage are in progress; root retains sole ownership of compiler runs and qualification.

Pass 26 development fixes are complete. A constant-size deleted-anchor tombstone now transfers the original slot into range exclusions; immediate and deferred drag reconstruction also normalizes removed anchors before starting a range. Grouped regressions cover fresh/toggled anchors, both directions, immediate/deferred input, stale/fresh snapshots, Add/Invert, explicit reanchoring, and continuation after a resort. Batch completion now acquires the finished state before checking the current event queue. Its regression injects final entries after the first empty receive, without sleeps or scheduling assumptions. Both primary regressions were run against the old code and failed at the intended assertions (`.tmp/pass26-shiftclick-before.log`, `.tmp/pass26-batch-before.log`). The batch regression passed after the fix.

All four supporting development audits completed with no outstanding findings. Formatting, 202 unit tests and 7 filesystem integration tests (209 total), and warnings-denied Clippy passed. Release build and terminal qualification are pending. Candidate source digest is `5b21780b57c10a794de094d982704e44ba8b25ab73d1d45415119c95092fc81e`. These development audits do not count toward the required fresh formal pair.

Pass 26 fixes are fully qualified on frozen source `5b21780b57c10a794de094d982704e44ba8b25ab73d1d45415119c95092fc81e`: 202 unit plus 7 filesystem tests, formatting, warnings-denied all-target Clippy, release build, four real-PTY corpora and nine JSON backend checks passed. Evidence: `.tmp/visual-pass26-fixes.log`, `.tmp/qualification-visual-complete.log`, `.tmp/qualified-visual-complete.json`. Pass 27 starts on these unchanged publication bytes; clean streak remains zero.

### Pass 27

All six fresh reviewers were verified running simultaneously before RELEASE. Source stayed frozen until all six final reports. Three reviewers reported no findings; three reported one finding each:

- Continuing a keyboard range to a newly discovered row could restore an explicitly deselected original anchor after an internal segment transition.
- A confirmation key delivered alongside a terminal shrink could use reviewability from the previous larger frame before the resize event was processed.
- Non-Linux root refreshes read metadata cached at initial anchor creation, leaving stale root inode sizes even when child contents refreshed.

Fixes and deterministic regression coverage are in progress. The clean streak remains zero.

Pass 27 fixes are implemented. Stable-ID explicit membership now survives internal segment changes while deliberate gestures reset it and deletion exclusions take precedence. Ready Delete, Batch and Quit confirmations compare live terminal geometry with the last frame before dispatch; Unix uses an ioctl-only query. Non-Linux root metadata is read fresh without following the final symlink.

The actual keyboard-discovery regression failed before the selection fix (`.tmp/pass27-selection-before.log`). All three stopped-child resize/confirmation tests failed before the geometry fix, with single/batch jobs improperly enqueued and guarded quit exiting (`.tmp/pass27-resize-before.log`). Source-only supporting audits found no outstanding issues. Formatting and 206 Linux unit plus 7 filesystem tests passed; remaining compiler/build/terminal checks are underway. Non-Linux runtime tests require a native runner; macOS compilation is included in qualification.

Pass 27 fixes are fully qualified on frozen source `895f0cd34644a106eebd053e6d54606196f794c8fe89e3d169ba21887fae9e2b`: 206 unit plus 7 filesystem tests (213 total), formatting, warnings-denied Linux all-target Clippy, release build, macOS all-target compilation, four PTY corpora and nine JSON backend checks passed. The full PTY run includes simultaneous resize/confirmation for single deletion, batch deletion and guarded quit. macOS compilation emitted three test-only dead-code warnings; macOS runtime tests were not run. Evidence: `.tmp/visual-pass27-fixes.log`, `.tmp/qualification-visual-complete.log`, `.tmp/qualified-visual-complete.json`. Pass 28 starts on these unchanged bytes; clean streak remains zero.

### Pass 28

All six fresh reviewers were verified running simultaneously before RELEASE; source stayed frozen until all six final reports. Four reported no findings; two found:

- Shift mouse entry into visual mode captured the old cursor when an earlier normal navigation was still pending, producing different selected IDs than the same input after navigation settled.
- Switching an inverse range to keyboard Add after the original anchor was deleted discarded its retained frozen slot and skipped intermediate survivors.

Both fixes are in development with deterministic regressions and independent supporting audits. Clean streak remains zero.

Pass 28 regressions reproduced both defects before their fixes: `mode_changes_keep_deleted_anchor_intervals_and_exclusions` and the actual-keyboard continuation test failed (`.tmp/pass28-selection-before.log`); pending Shift mouse entry included the stale cursor ID (`.tmp/pass28-mouse-before.log`). A shared range-restart helper now retains a deleted anchor's frozen interval and exclusions when changing modes. Mouse entry follows the existing deferred visual-entry contract. Supporting source audits and final regression additions are completing before root qualification.

Pass 28 fixes are fully qualified on frozen source `745bcf043a8526ff68e60b7af413dc8ac021d53a94fb0bb9647658aabefec57d`: 209 unit plus 7 filesystem tests (216 total), formatting, strict Linux Clippy, macOS all-target compilation, release build, four PTY corpora and nine JSON backend checks passed. Supporting source audits found no outstanding issues. The mouse regression also verifies batch preparation waits for corrected selection and cancellation preserves it. macOS runtime remains untested; its three test-only dead-code warnings are unchanged. Evidence: `.tmp/visual-pass28-fixes.log`, `.tmp/qualification-visual-complete.log`, `.tmp/qualified-visual-complete.json`. Pass 29 starts on these unchanged bytes; clean streak remains zero.

### Pass 29

All six fresh reviewers were verified running concurrently before RELEASE on qualified source `745bcf043a8526ff68e60b7af413dc8ac021d53a94fb0bb9647658aabefec57d`. Domains: keyboard, mouse, selection lifecycle, deletion, scanner/accounting, and rendering/validation. All six independently reported **NO FINDINGS**. No repository files changed until all six reports completed. Source digest remains unchanged; clean streak is **1**. A fresh saturated Pass 30 follows on the same publication bytes.

### Pass 30

All six fresh reviewers were verified running simultaneously before RELEASE on the same qualified source as Pass 29: `745bcf043a8526ff68e60b7af413dc8ac021d53a94fb0bb9647658aabefec57d`. Domains: keyboard, mouse, selection lifecycle, deletion, scanning/accounting, and rendering/validation. All six independently reported **NO FINDINGS**. Source remained unchanged through both rounds, with no repository writes during either review. Clean streak is **2**. Working, staged and committed publication source bytes are checked against the qualification digest before the authorized push.
