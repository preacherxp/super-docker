#!/bin/sh
set -eu

main() {
    repository=https://github.com/preacherxp/super-docker
    version=${1:-latest}
    install_dir=${SUPER_DOCKER_INSTALL_DIR:-"$HOME/.local/bin"}

    case "$(uname -s)" in
        Darwin) platform=apple-darwin ;;
        Linux) platform=unknown-linux-musl ;;
        *) echo "Unsupported OS: macOS and Linux are supported." >&2; exit 1 ;;
    esac
    case "$(uname -m)" in
        arm64|aarch64) arch=aarch64 ;;
        x86_64|amd64) arch=x86_64 ;;
        *) echo "Unsupported architecture: x86_64 and ARM64 are supported." >&2; exit 1 ;;
    esac

    if [ "$version" = latest ]; then
        release_url=$(curl --proto '=https' --proto-redir '=https' -fsSL \
            --connect-timeout 10 --max-time 30 -o /dev/null -w '%{url_effective}' \
            "$repository/releases/latest")
        case "$release_url" in
            "$repository/releases/tag/"*) version=${release_url##*/} ;;
            *) echo "Could not find a published release." >&2; exit 1 ;;
        esac
    fi
    if ! printf '%s\n' "$version" | grep -Eq '^v?[0-9]+\.[0-9]+\.[0-9]+$'; then
        echo "Expected a stable release tag, such as v0.1.2." >&2
        exit 1
    fi

    asset="super-docker-$arch-$platform.tar.gz"
    base="$repository/releases/download/$version"
    temporary=$(mktemp -d)
    staging=
    trap 'rm -rf "$temporary"; if [ -n "$staging" ]; then rm -rf "$staging"; fi' 0
    trap 'exit 1' HUP INT TERM
    echo "Downloading super-docker $version ($arch-$platform)…"
    for file in "$asset" "$asset.sha256"; do
        if ! curl --proto '=https' --proto-redir '=https' -fsSL \
            --connect-timeout 10 --max-time 120 "$base/$file" -o "$temporary/$file"; then
            echo "Download failed. Check that $version has binaries for $arch-$platform." >&2
            exit 1
        fi
    done

    expected=$(cut -d ' ' -f 1 "$temporary/$asset.sha256")
    if [ "${#expected}" -ne 64 ] || printf '%s' "$expected" | grep -q '[^0-9a-f]'; then
        echo "Invalid release checksum." >&2
        exit 1
    fi
    if command -v sha256sum >/dev/null 2>&1; then
        actual=$(sha256sum "$temporary/$asset")
    else
        actual=$(shasum -a 256 "$temporary/$asset")
    fi
    if [ "${actual%% *}" != "$expected" ]; then
        echo "Checksum mismatch; nothing was installed." >&2
        exit 1
    fi
    tar -xzf "$temporary/$asset" -C "$temporary" sd
    if [ ! -f "$temporary/sd" ] || [ -L "$temporary/sd" ]; then
        echo "Release archive does not contain a regular sd binary." >&2
        exit 1
    fi

    mkdir -p "$install_dir"
    for name in sd super-docker; do
        if [ -d "$install_dir/$name" ]; then
            echo "Cannot replace directory: $install_dir/$name" >&2
            exit 1
        fi
    done
    # Stage on the destination filesystem so a running binary can be replaced by rename.
    staging=$(mktemp -d "$install_dir/.super-docker.XXXXXX")
    cp "$temporary/sd" "$staging/sd"
    chmod 755 "$staging/sd"
    ln -s sd "$staging/super-docker"
    mv -f "$staging/sd" "$install_dir/sd"
    mv -f "$staging/super-docker" "$install_dir/super-docker"
    echo "Installed sd and super-docker in $install_dir."
    case ":$PATH:" in
        *":$install_dir:"*) echo "Run sd to get started." ;;
        *) printf 'Run %s/sd, or add %s to your PATH to use sd.\n' "$install_dir" "$install_dir" ;;
    esac
}

main "$@"
