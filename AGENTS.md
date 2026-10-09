# ldu

- Pure Rust; preserve strict breadth-first depth barriers and parallel metadata batches.
- Draw the first terminal frame before filesystem scanning. Keep progress channels bounded and repaint immediately after keyboard actions.
- Sort scanning and completed entries together by size. Mark scanning entries with a gray, one-cell animated Braille spinner; preserve order within bounded near-size groups and keep selection by stable node ID. Keep sorted directory views warm; hover and navigation must not rebuild or destroy whole-directory collections on the input thread. Never paint unsorted provisional rows on entry.
- Keep the browser minimal: one path header, a `?` help hint and the entry list. At widths of at least 120 columns, preview only the selected directory's subdirectories. Use the same row rendering in both panes, including spinners and fractional block size bars without brackets or approximation inequalities.
- Use thin, sharp borders only on dialogs, with concise highlighted keys and actions. Keep the normal list unframed.
- Preserve ncdu navigation keys. `d` requires confirmation and runs deletion asynchronously; `n` and Escape cancel confirmation. Block confirmation while its target and choices cannot be displayed. Anchor deletion beneath the scan root and reject unsafe replacement paths.
- Apply successful deletion to the model and ancestor totals incrementally. Do not trigger a full rescan or a persistent success dialog. Keep explicit `r` refresh available.
- Preserve raw filenames and distinguish partial/error totals from accurate totals.
- Do not follow symlinks. Count allocated blocks by default; preserve apparent sizes and global hardlink deduplication.
- Use project-relative paths for tools and `.tmp` for temporary fixtures. Never signal unrelated processes.
- Root owns compiler runs, release qualification, commits and repository publication during parallel development.
- Measure only after the host is quiet; do not claim universal speed wins from synthetic timings.
- After the latest implementation is frozen and qualified, complete two consecutive fully saturated six-agent adversarial passes with no valid findings. A finding and its fix, or new user requirements, reset the clean-pass count. Interrupted or unsaturated attempts do not count. Record results in `REVIEW.md`.
