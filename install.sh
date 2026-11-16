#!/bin/sh

set -eu

COCOA_REPOSITORY=${COCOA_REPOSITORY:-ezegros/cocoa}

main() {
    version=latest
    install_directory=${COCOA_INSTALL_DIR-}
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --version|--install-dir)
                [ "$#" -ge 2 ] && [ -n "$2" ] || fail "Missing value for $1"
                case "$1" in
                    --version) version=$2 ;;
                    --install-dir) install_directory=$2 ;;
                esac
                shift 2
                ;;
            -h|--help)
                printf '%s\n' \
                    'Install Cocoa from GitHub Releases.' \
                    '' \
                    'Usage: bash install.sh [--version VERSION] [--install-dir DIRECTORY]' \
                    '' \
                    '  --version VERSION        Release version, with or without v; default: latest stable' \
                    '  --install-dir DIRECTORY  Absolute binary directory; default: ~/.local/bin' \
                    '  COCOA_INSTALL_DIR        Set the default binary directory' \
                    "  COCOA_REPOSITORY         GitHub owner/repo; default: $COCOA_REPOSITORY" \
                    '' \
                    'No sudo, shell-profile edits, provider setup or user-data changes.'
                return
                ;;
            *) fail "Unknown option: $1. Use --help for usage." ;;
        esac
    done

    if [ -z "$install_directory" ]; then
        [ -n "${HOME-}" ] || fail 'HOME is unset; pass --install-dir with an absolute path.'
        install_directory=$HOME/.local/bin
    fi
    case "$install_directory" in
        /*) ;;
        *) fail 'The installation directory must be an absolute path.' ;;
    esac
    case "$COCOA_REPOSITORY" in
        *[!A-Za-z0-9_./-]*|/*|*/|*/*/*|*..*) fail 'COCOA_REPOSITORY must be a GitHub owner/repo.' ;;
        */*) ;;
        *) fail 'COCOA_REPOSITORY must be a GitHub owner/repo.' ;;
    esac

    for dependency in curl uname tar gzip mktemp mkdir chmod mv rm awk sed; do
        command -v "$dependency" > /dev/null 2>&1 || fail "Required command not found: $dependency"
    done
    [ -x /bin/bash ] || fail 'Cocoa requires /bin/bash at runtime. Install Bash first.'
    if command -v sha256sum > /dev/null 2>&1; then
        checksum_command=sha256sum
    elif command -v shasum > /dev/null 2>&1; then
        checksum_command=shasum
    else
        fail 'SHA-256 verification requires sha256sum or shasum. Nothing was installed.'
    fi

    system=$(uname -s)
    architecture=$(uname -m)
    case "$system:$architecture" in
        Darwin:arm64|Darwin:aarch64) target=aarch64-apple-darwin ;;
        Darwin:x86_64|Darwin:amd64) target=x86_64-apple-darwin ;;
        Linux:arm64|Linux:aarch64) target=aarch64-unknown-linux-musl ;;
        Linux:x86_64|Linux:amd64) target=x86_64-unknown-linux-musl ;;
        *) fail "Unsupported platform: $system $architecture. Cocoa supports macOS and Linux on ARM64 and x86-64." ;;
    esac

    release_root=https://github.com/$COCOA_REPOSITORY/releases
    if [ "$version" = latest ]; then
        if ! release_url=$(download --max-filesize 1048576 --output /dev/null --write-out '%{url_effective}' "$release_root/latest"); then
            fail 'Could not resolve the latest stable release. A draft or prerelease is not available through latest; publish a release or pass --version.'
        fi
        case "$release_url" in
            "$release_root/tag/"*) version=${release_url#"$release_root/tag/"} ;;
            *) fail 'No published stable release was found. Draft releases are not installable; publish a release or pass --version.' ;;
        esac
    fi
    case "$version" in v*) ;; *) version=v$version ;; esac
    if ! printf '%s\n' "$version" | LC_ALL=C awk '
        $0 !~ /^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$/ { invalid = 1 }
        END { exit (NR != 1 || invalid) }
    '; then
        fail 'Invalid version. Use a release version such as v0.1.0.'
    fi

    destination=$install_directory/cocoa
    if [ -L "$destination" ] || { [ -e "$destination" ] && [ ! -f "$destination" ]; }; then
        fail "Refusing to replace a symlink or non-regular file: $destination"
    fi
    umask 077
    temporary_directory=
    trap cleanup 0
    trap 'exit 129' HUP
    trap 'exit 130' INT
    trap 'exit 143' TERM
    mkdir -p "$install_directory"
    temporary_directory=$(mktemp -d "$install_directory/.cocoa-install.XXXXXXXX")

    archive_name=cocoa-$target.tar.gz
    archive=$temporary_directory/$archive_name
    release_url=$release_root/download/$version
    printf 'Installing Cocoa %s for %s\n' "$version" "$target"
    download --max-filesize 65536 --output "$temporary_directory/sha256.sum" "$release_url/sha256.sum" \
        || fail "Could not download checksums for $version. Check that the release is published and complete."
    expected_checksum=$(LC_ALL=C awk -v archive="$archive_name" '
        $2 == archive {
            count++
            if (length($1) != 64 || $1 ~ /[^0-9a-f]/ || $0 != $1 "  " archive) invalid = 1
            checksum = $1
        }
        END {
            if (count != 1 || invalid) exit 1
            print checksum
        }
    ' "$temporary_directory/sha256.sum") || fail "Missing, invalid or duplicate SHA-256 checksum for $archive_name."
    download --max-filesize 268435456 --output "$archive" "$release_url/$archive_name" \
        || fail "Could not download $archive_name for $version. The release may not include this platform."
    case "$checksum_command" in
        sha256sum) actual_checksum=$(sha256sum < "$archive") ;;
        shasum) actual_checksum=$(shasum -a 256 < "$archive") ;;
    esac
    actual_checksum=${actual_checksum%% *}
    [ "$actual_checksum" = "$expected_checksum" ] || fail 'SHA-256 checksum mismatch. Nothing was installed.'

    unset TAR_OPTIONS GZIP
    member=cocoa-$target/cocoa
    listing=$(LC_ALL=C tar -tvzf "$archive" "$member") || fail "Archive is missing $member."
    if ! printf '%s\n' "$listing" | awk '
        NR == 1 && substr($0, 1, 1) == "-" { regular = 1 }
        END { exit !(NR == 1 && regular) }
    '; then
        fail 'The archive must contain exactly one regular Cocoa executable.'
    fi
    # Stream only the executable; archive paths and link targets never reach the filesystem.
    if ! (ulimit -f 524288 && tar -xOzf "$archive" "$member" > "$temporary_directory/cocoa"); then
        fail 'Could not extract the Cocoa executable.'
    fi
    [ -s "$temporary_directory/cocoa" ] || fail 'The Cocoa executable is empty.'
    chmod 755 "$temporary_directory/cocoa"
    mv -f "$temporary_directory/cocoa" "$destination"
    printf 'Installed %s\n' "$destination"
    case ":${PATH-}:" in
        *:"$install_directory":*) printf 'Run cocoa from your project directory to get started.\n' ;;
        *)
            quoted_directory=$(printf '%s' "$install_directory" | sed "s/'/'\\\\''/g")
            printf '\nAdd the directory to your PATH in Bash or zsh:\n  export PATH=\047%s\047:"$PATH"\n' "$quoted_directory"
            printf 'To keep it for future terminals, add that line to your shell profile. Then run cocoa.\n'
            ;;
    esac
}

download() {
    curl -q --fail --silent --show-error --location \
        --proto '=https' --proto-redir '=https' --tlsv1.2 --max-redirs 5 \
        --connect-timeout 10 --max-time 300 --retry 2 --retry-max-time 600 "$@"
}

fail() {
    printf 'cocoa installer: %s\n' "$*" >&2
    exit 1
}

cleanup() {
    if [ -n "$temporary_directory" ]; then
        rm -rf -- "$temporary_directory" || printf 'Could not remove installer temporary files: %s\n' "$temporary_directory" >&2
    fi
}

main "$@"
