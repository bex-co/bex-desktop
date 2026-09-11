#!/usr/bin/env sh
set -eu

# Downloads a tarball from https://zed.dev/releases and unpacks it
# into ~/.local/. If you'd prefer to do this manually, instructions are at
# https://zed.dev/docs/linux.

link_cli() {
    destination="$HOME/.local/bin/bex"
    if [ -e "$destination" ] || [ -L "$destination" ]; then
        if [ -L "$destination" ] && [ "$(readlink "$destination")" = "$1" ]; then
            return
        fi
        echo "Existing command preserved at $destination. Bex CLI is available at $1"
        return
    fi
    ln -s "$1" "$destination"
}

main() {
    platform="$(uname -s)"
    arch="$(uname -m)"
    channel="${BEX_CHANNEL:-${ZED_CHANNEL:-stable}}"
    ZED_VERSION="${BEX_VERSION:-${ZED_VERSION:-latest}}"
    # Use TMPDIR if available (for environments with non-standard temp directories)
    if [ -n "${TMPDIR:-}" ] && [ -d "${TMPDIR}" ]; then
        temp="$(mktemp -d "$TMPDIR/bex-XXXXXX")"
    else
        temp="$(mktemp -d "/tmp/bex-XXXXXX")"
    fi

    if [ "$platform" = "Darwin" ]; then
        platform="macos"
    elif [ "$platform" = "Linux" ]; then
        platform="linux"
    else
        echo "Unsupported platform $platform"
        exit 1
    fi

    case "$platform-$arch" in
        macos-arm64* | linux-arm64* | linux-aarch64)
            arch="aarch64"
            ;;
        macos-x86* | linux-x86*)
            arch="x86_64"
            ;;
        *)
            echo "Unsupported platform or architecture"
            exit 1
            ;;
    esac

    if command -v curl >/dev/null 2>&1; then
        curl () {
            command curl -fL "$@"
        }
    elif command -v wget >/dev/null 2>&1; then
        curl () {
            wget -O- "$@"
        }
    else
        echo "Could not find 'curl' or 'wget' in your path"
        exit 1
    fi

    "$platform" "$@"

    if [ "$(command -v bex)" = "$HOME/.local/bin/bex" ]; then
        echo "Bex has been installed. Run with 'bex'"
    else
        echo "To run Bex from your terminal, you must add ~/.local/bin to your PATH"
        echo "Run:"

        case "$SHELL" in
            *zsh)
                echo "   echo 'export PATH=\$HOME/.local/bin:\$PATH' >> ~/.zshrc"
                echo "   source ~/.zshrc"
                ;;
            *fish)
                echo "   fish_add_path -U $HOME/.local/bin"
                ;;
            *)
                echo "   echo 'export PATH=\$HOME/.local/bin:\$PATH' >> ~/.bashrc"
                echo "   source ~/.bashrc"
                ;;
        esac

        echo "To run Bex now, '~/.local/bin/bex'"
    fi
}

linux() {
    if [ -n "${ZED_BUNDLE_PATH:-}" ]; then
        cp "$ZED_BUNDLE_PATH" "$temp/bex-linux-$arch.tar.gz"
    else
        echo "Downloading Bex version: $ZED_VERSION"
        curl "https://bex.co/releases/$channel/$ZED_VERSION/download?asset=bex&arch=$arch&os=linux&source=install.sh" > "$temp/bex-linux-$arch.tar.gz"
    fi

    suffix=""
    if [ "$channel" != "stable" ]; then
        suffix="-$channel"
    fi

    appid=""
    case "$channel" in
      stable)
        appid="co.bex.Bex"
        ;;
      nightly)
        appid="co.bex.Bex-Nightly"
        ;;
      preview)
        appid="co.bex.Bex-Preview"
        ;;
      dev)
        appid="co.bex.Bex-Dev"
        ;;
      *)
        echo "Unknown release channel: ${channel}. Using stable app ID."
        appid="co.bex.Bex"
        ;;
    esac

    # Unpack
    rm -rf "$HOME/.local/bex$suffix.app"
    mkdir -p "$HOME/.local/bex$suffix.app"
    tar -xzf "$temp/bex-linux-$arch.tar.gz" -C "$HOME/.local/" "bex$suffix.app"

    zed_editor="$HOME/.local/bex$suffix.app/libexec/bex-editor"
    if [ -f "$zed_editor" ] && command -v ldd >/dev/null 2>&1; then
        missing="$(ldd "$zed_editor" 2>/dev/null | sed -n 's/^[[:space:]]*\(.*\) => not found$/\1/p')"
        if [ -n "$missing" ]; then
            echo "Warning: your system is missing libraries that Bex needs:"
            echo "$missing" | sed 's/^/    /'
            echo "Install them with your package manager, or Bex will fail to start."
        fi
    fi

    # Setup ~/.local directories
    mkdir -p "$HOME/.local/bin" "$HOME/.local/share/applications"

    # Link the binary
    if [ -f "$HOME/.local/bex$suffix.app/bin/bex" ]; then
        link_cli "$HOME/.local/bex$suffix.app/bin/bex"
    else
        # support for versions before 0.139.x.
        link_cli "$HOME/.local/bex$suffix.app/bin/cli"
    fi

    # Copy .desktop file
    desktop_file_path="$HOME/.local/share/applications/${appid}.desktop"
    src_dir="$HOME/.local/bex$suffix.app/share/applications"
    if [ -f "$src_dir/${appid}.desktop" ]; then
        cp "$src_dir/${appid}.desktop" "${desktop_file_path}"
    else
        # Fallback for older tarballs
        cp "$src_dir/zed$suffix.desktop" "${desktop_file_path}"
    fi
    sed -i "s|Icon=bex|Icon=$HOME/.local/bex$suffix.app/share/icons/hicolor/512x512/apps/bex.png|g" "${desktop_file_path}"
    sed -i "s|Exec=bex|Exec=$HOME/.local/bex$suffix.app/bin/bex|g" "${desktop_file_path}"
}

macos() {
    echo "Downloading Bex version: $ZED_VERSION"
    curl "https://bex.co/releases/$channel/$ZED_VERSION/download?asset=bex&os=macos&arch=$arch&source=install.sh" > "$temp/Bex-$arch.dmg"
    hdiutil attach -quiet "$temp/Bex-$arch.dmg" -mountpoint "$temp/mount"
    app="$(cd "$temp/mount/"; echo Bex*.app)"
    echo "Installing $app"
    if [ -d "/Applications/$app" ]; then
        echo "Removing existing $app"
        rm -rf "/Applications/$app"
    fi
    ditto "$temp/mount/$app" "/Applications/$app"
    hdiutil detach -quiet "$temp/mount"

    mkdir -p "$HOME/.local/bin"
    # Link the binary
    link_cli "/Applications/$app/Contents/MacOS/cli"
}

main "$@"
