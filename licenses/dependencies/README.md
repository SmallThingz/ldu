# Notices missing from crate archives

`sources.json` pins fallback notices for exact registry releases. Packaging checks the crate name, version, declared license, registry source, lockfile checksum and source commit before using them. Unknown releases still require their own notices. The bundled text hashes are checked too; packaging needs no network access.

The published archives of `compio-buf` 0.8.3, `compio-driver` 0.12.5 and `compio-log` 0.2.1 omit the repository's MIT license file. `compio-LICENSE.txt` preserves that file byte for byte, including its copyright notice. The manifest records the upstream commit and download URL for each crate.

`mod_use` 0.2.3 and `synchrony` 0.1.9 declare MIT in their Cargo manifests but provide no license text in either the published archive or the corresponding repository commit. Their exact declaration lines and source URLs are retained. `SPDX-MIT-template.txt` supplies the canonical MIT terms from the pinned SPDX license list commit, unchanged. It is labeled as a template, not an upstream notice; its copyright placeholders have not been filled with invented names or dates.

The crate archive checksums and embedded `.cargo_vcs_info.json` records were verified against downloads from crates.io. The declaration manifest hashes record the complete upstream Cargo files from which the MIT lines were taken. Recheck the sources when updating any of these versions.
