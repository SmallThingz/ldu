# ldu

Browse disk usage, preview files and queue deletions in your terminal.

## Install

**NixOS (x86-64 or ARM64)**

```sh
nix --extra-experimental-features 'nix-command flakes' profile install github:SmallThingz/ldu
```

**macOS ([Homebrew](https://brew.sh))**

```sh
brew install SmallThingz/tap/ldu
```

**Other Linux distributions and FreeBSD (Bash and curl)**

```sh
bash -o pipefail -c 'curl -fsSL https://raw.githubusercontent.com/SmallThingz/ldu/v0.1.0/scripts/install.sh | bash'
```

The script installs to `~/.local/bin`; add that directory to `PATH`.

Run `ldu` or `ldu DIRECTORY`.

[Controls and configuration](docs/behavior.md) · [Platforms and builds](docs/releasing.md) · [Distribution](docs/distribution.md) · [MIT license](LICENSE)
