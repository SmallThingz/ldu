# Review qualification

Completion requires two consecutive adversarial review passes, each using all six reviewer slots, with no valid findings. A valid finding and its fix reset the clean-pass count. Tests and measurements are separate evidence and do not count as review passes.

Current consecutive clean passes: **2**. Qualification is **COMPLETE**. All six independent reviewers in Pass 15 and all six in Pass 16 reported **NO FINDINGS** on the same frozen source `98616a71236d9ece5dc3a2606500ef80093ebf259b9ff4981f4c5d0cdcccf44c`. The source remained unchanged across both passes.

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
| 16 | 6 of 6 | No findings | Clean; streak 2; current source qualified |

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

All six fresh reviewers were active concurrently and independently reported **NO FINDINGS** on the same source as Pass 15. They reviewed filesystem/scanner/deletion, accounting, navigation/input, publication/docs/dependencies, terminal rendering/harness, and cache ordering/lifetime. Both saturated passes completed after release qualification, with no source changes between them. The final clean streak is **2** and the current implementation is qualified. The source and staged publication bytes were independently rechecked against the qualification digest before committing.
