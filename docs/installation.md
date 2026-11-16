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

Installer-managed installations automatically check for stable updates in the background when interactive chat or resume starts, at most once per hour per installation. An available update installs without interrupting the current session and takes effect on the next launch. There are no periodic checks during a session. Help, version, `exec` and administrative commands do not start automatic updates.

Run `cocoa update` or `/update` to update manually. Both honor a saved version pin rather than silently returning to latest. The options are the same in the CLI and chat:

| Command | Behavior |
| --- | --- |
| `cocoa update` | Install a newer stable release, or keep the pinned version |
| `cocoa update --check` | Report installed and latest stable versions, and any pin, without installing or changing policy |
| `cocoa update --version 0.2.3` | Install that exact release and keep it pinned, even if it is older |
| `cocoa update --unpin` | Resume stable updates and install a newer stable release if available |

Manual operations bypass the one-hour check interval. Ordinary updates and unpinning never downgrade a newer installed version. Exact versions accept a `v` prefix and preview tags. Passing `--version` to the installer also creates a persistent pin. Reusing the installer without `--version` selects latest stable and clears the pin.

Set `"auto_update": false` in `~/.config/cocoa/config.json` to disable automatic checks and installations. `COCOA_AUTO_UPDATE=0` disables them for one process. Neither prevents manual updates.

Self-updates require the ownership receipt written by the installer. Existing installations from older installers must rerun the current installer once. Cargo installations and copied binaries do not update themselves; use their original installation method or reinstall with the installer. The receipt records the repository, platform, installed binary hash and pin beside the executable, not in project configuration.

Downloads use HTTPS and release checksums. Cocoa stages the replacement beside the executable and uses an atomic rename, so existing processes continue running their original binary. A shared installation lock prevents concurrent installer/updater writes. If an interrupted operation leaves `.cocoa-update.lock`, confirm that no installer or updater is still running before removing that directory. The next update reconciles any pending receipt against the installed binary.

Rolling back replaces only the executable. It does not roll back configuration, credentials or session storage. Older binaries can reject newer configuration keys or database schemas; a pin does not make sessions backward-compatible. Keep newer session data rather than deleting it to force an older binary to open it.

Close Cocoa first, then remove the default installation with:

```sh
rm "$HOME/.local/bin/cocoa" "$HOME/.local/bin/.cocoa-install.json"
```

For a custom installation, remove that directory's executable and receipt. Uninstalling does not delete credentials, configuration or sessions. Their locations are listed in [configuration](configuration.md#file-locations). Remove a PATH entry only if other programs do not need it.
