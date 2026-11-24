#!/bin/sh
# Install the latest rrover release:
#   curl -LsSf https://github.com/danik-tro/redis-rover/releases/latest/download/install.sh | sh
#
# RROVER_INSTALL_DIR  where to put the binary (default: ~/.local/bin)
# RROVER_VERSION      a release tag instead of the latest (e.g. v0.1.0)
# RROVER_RELEASES     releases URL (default: the project's GitHub releases)
set -eu

releases="${RROVER_RELEASES:-https://github.com/danik-tro/redis-rover/releases}"
install_dir="${RROVER_INSTALL_DIR:-$HOME/.local/bin}"

fail() {
    echo "rrover install: $1" >&2
    exit 1
}

need() {
    command -v "$1" >/dev/null 2>&1 || fail "this script needs $1"
}

platform() {
    case "$(uname -s)" in
        Linux) os=linux suffix=-musl ;;
        Darwin) os=macos suffix= ;;
        *) fail "unsupported OS $(uname -s); see $releases" ;;
    esac
    case "$(uname -m)" in
        x86_64 | amd64) arch=x86_64 ;;
        arm64 | aarch64) arch=arm64 ;;
        *) fail "unsupported CPU $(uname -m); see $releases" ;;
    esac
    echo "$os-$arch$suffix"
}

latest_tag() {
    url=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$releases/latest") ||
        fail "could not reach $releases"
    tag=${url##*/tag/}
    if [ "$tag" = "$url" ] || [ -z "$tag" ]; then
        fail "no release found at $releases"
    fi
    echo "$tag"
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d ' ' -f 1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d ' ' -f 1
    else
        fail "this script needs sha256sum or shasum"
    fi
}

main() {
    need curl
    need tar
    target=$(platform)
    tag="${RROVER_VERSION:-$(latest_tag)}"
    name="rrover-$tag-$target"

    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT INT TERM

    echo "Downloading $name"
    curl -fsSL -o "$tmp/$name.tar.gz" "$releases/download/$tag/$name.tar.gz" ||
        fail "could not download $name.tar.gz"
    curl -fsSL -o "$tmp/$name.sha256" "$releases/download/$tag/$name.sha256" ||
        fail "could not download $name.sha256"

    expected=$(cut -d ' ' -f 1 "$tmp/$name.sha256" | tr -d '\r\n' | tr 'A-F' 'a-f')
    actual=$(sha256_of "$tmp/$name.tar.gz")
    case "$expected" in
        '' | *[!0-9a-f]*) fail "$name.sha256 holds no checksum; nothing was installed" ;;
    esac
    [ "${#expected}" -eq 64 ] || fail "$name.sha256 holds no checksum; nothing was installed"
    [ "$expected" = "$actual" ] ||
        fail "sha256 mismatch for $name.tar.gz (expected $expected, got $actual); nothing was installed"

    tar -xzf "$tmp/$name.tar.gz" -C "$tmp" rrover || fail "the archive holds no rrover binary"
    mkdir -p "$install_dir"
    # Replaced by rename, so a running rrover keeps its old file.
    cp "$tmp/rrover" "$install_dir/.rrover.new"
    chmod 755 "$install_dir/.rrover.new"
    mv -f "$install_dir/.rrover.new" "$install_dir/rrover"

    echo "Installed rrover $tag to $install_dir/rrover"
    case ":$PATH:" in
        *":$install_dir:"*) ;;
        *) echo "Add $install_dir to your PATH to run it." ;;
    esac
}

main
