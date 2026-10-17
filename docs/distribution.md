# Distribution

## Homebrew tap

The formula in `packaging/homebrew/ldu.rb` installs the released Linux or macOS binary for x86-64 or ARM64.

To publish or update the tap, copy it to `Formula/ldu.rb` in `SmallThingz/homebrew-tap`, check the formula, then commit and push that repository. For a new release, update its version, URLs and SHA256 checksums together.

Once the formula is published:

```sh
brew install SmallThingz/tap/ldu
```

Update an installation with `brew update` followed by `brew upgrade ldu`.

## Nix flake

The root flake installs pinned release binaries for `x86_64-linux`, `aarch64-linux`, `x86_64-darwin` and `aarch64-darwin`. It does not compile LDU. Nix must have flakes and the `nix-command` feature enabled.

After the flake and its lockfile are pushed:

```sh
nix run github:SmallThingz/ldu
nix profile install github:SmallThingz/ldu
```

For a new LDU release, update `version` and every asset hash in `flake.nix`. `nix flake update` updates the pinned Nixpkgs input; it does not select a newer LDU release. The `v0.1.0` tag predates the flake, so use a commit or branch that contains it.

## AUR submission

`packaging/aur/ldu/` builds from source and requires Rust 1.99.0. `packaging/aur/ldu-bin/` installs the released Linux binary. Neither recipe is an AUR submission by itself.

From either recipe directory, build and check it on Arch Linux, then regenerate its metadata:

```sh
makepkg -si
makepkg --printsrcinfo > .SRCINFO
```

Create an [AUR account](https://aur.archlinux.org/register), add your SSH public key, and clone the package repository, for example `git clone ssh://aur@aur.archlinux.org/ldu-bin.git`. Copy the recipe files and `.SRCINFO` into that checkout, then commit and push. The source package also needs `generate-notices.py` and `rust-1.99.0-notices.tar.gz`. Follow the [AUR submission guidelines](https://wiki.archlinux.org/title/AUR_submission_guidelines).

For updates, change the version, source URLs and checksums in each `PKGBUILD`, rebuild, and regenerate `.SRCINFO` before pushing. For the source package, also update the compiler/runtime notices and `SOURCE_COMMIT` in `generate-notices.py`. Changing the supported compiler requires refreshing and revalidating the Rust notice archive too. Neither package has been submitted here.

## crates.io

LDU has not been published to crates.io. Publishing requires registry credentials and a dependency plan that preserves the local Crossterm and filedescriptor patches; Cargo's registry package does not retain those overrides. Resolve that before offering a `cargo install` command.
