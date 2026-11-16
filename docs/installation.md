# Installation

Cocoa runs on macOS and Linux. The installer downloads a prebuilt executable from [GitHub Releases](https://github.com/ezegros/cocoa/releases) to `~/.local/bin`, without sudo or a Rust toolchain. If no release is available for your platform, [build from source](../CONTRIBUTING.md#build-from-source).

## Install

```sh
curl -fsSL https://github.com/ezegros/cocoa/releases/latest/download/install.sh | bash
```

If Cocoa is not on your PATH, add this line to your shell's startup file, such as `~/.zshrc` or `~/.bashrc`:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

Open a new terminal, run `cocoa --version`, then run `cocoa` from your project directory. First-run setup connects a [provider](providers.md). The installer does not launch Cocoa, read credentials or edit shell profiles.

## Requirements

Cocoa provides builds for macOS on Apple Silicon and Intel, and Linux on ARM64 and x86-64. Windows and 32-bit systems are unsupported. A macOS shell running under Rosetta selects the Intel build.

Install `curl`, `tar`, `gzip`, standard Unix utilities and either `sha256sum` or `shasum`. Cocoa needs `/bin/bash` to execute shell commands. Minimal Linux images may require these packages. Chat requires an interactive terminal.

## Choose a version or directory

Set `VERSION` to a published release tag, including a preview tag if desired:

```sh
VERSION=v0.1.0
curl -fsSL "https://github.com/ezegros/cocoa/releases/download/$VERSION/install.sh" | bash -s -- --version "$VERSION"
```

Versions accept an optional `v` prefix. Without `--version`, the installer selects the latest stable release, never a preview. A versioned script URL alone does not pin the executable; pass `--version` too.

Choose a different absolute installation directory:

```sh
curl -fsSL https://github.com/ezegros/cocoa/releases/latest/download/install.sh | bash -s -- --install-dir "$HOME/bin"
```

`COCOA_INSTALL_DIR` sets the same default; the flag takes precedence. `COCOA_REPOSITORY` selects a GitHub `owner/repo` for forks. Pass these variables to the installer process, not to `curl` in a pipeline. A downloaded installer supports `bash install.sh --help` without downloading a release.

## Verify the download

Piping to Bash runs code from the publisher. To inspect it first:

```sh
curl -fSL https://github.com/ezegros/cocoa/releases/latest/download/install.sh -o install.sh
less install.sh
bash install.sh
```

Only run the file after a successful download. The installer checks the archive against the release's `sha256.sum` before replacing the executable. This detects corruption, not a compromised publisher. Releases include GitHub provenance attestations for manual verification with `gh attestation verify`; the installer does not verify them automatically.

## Update, roll back or uninstall

Rerun the install command to update, or pin an older published version to roll back.

Remove the default installation with:

```sh
rm "$HOME/.local/bin/cocoa"
```

For a custom installation, remove that directory's `cocoa` executable. Uninstalling does not delete credentials, configuration or sessions. Their locations are listed in [configuration](configuration.md#file-locations). Remove a PATH entry only if other programs do not need it.
