#!/bin/sh
# Download and install the published Slashie headless binary.

set -eu

repository=williamfzc/slashie
api_url="https://api.github.com/repos/$repository/releases/latest"
download_base="https://github.com/$repository/releases/download"
home=${HOME:-}
install_dir=${INSTALL_DIR:-}
tmpdir=
staged=

die() {
    printf '%s\n' "install.sh: $*" >&2
    exit 1
}

cleanup() {
    if [ -n "$staged" ]; then
        rm -f "$staged"
    fi
    if [ -n "$tmpdir" ]; then
        rm -rf "$tmpdir"
    fi
}

download() {
    url=$1
    output=$2
    if command -v curl >/dev/null 2>&1; then
        curl -fL --retry 3 --silent --show-error "$url" -o "$output"
    elif command -v wget >/dev/null 2>&1; then
        wget -O "$output" "$url"
    else
        die "curl or wget is required"
    fi
}

[ "$#" -le 1 ] || die "usage: $0 [VERSION]"
[ -n "$home" ] || die "HOME is not set"
[ -n "$install_dir" ] || install_dir="$home/.local/bin"

case "$(uname -s)" in
    Darwin)
        case "$(uname -m)" in
            arm64|aarch64) asset_suffix=aarch64_macos ;;
            *) die "unsupported platform: macOS $(uname -m); the published headless asset is macOS arm64" ;;
        esac
        ;;
    Linux)
        case "$(uname -m)" in
            x86_64|amd64) asset_suffix=x86_64_linux ;;
            *) die "unsupported platform: Linux $(uname -m); the published headless asset is Linux x86_64" ;;
        esac
        ;;
    *) die "unsupported operating system: $(uname -s); published headless assets target macOS arm64 and Linux x86_64" ;;
esac

tmpdir=$(mktemp -d "${TMPDIR:-/tmp}/slashie-install.XXXXXX") || die "could not create a temporary directory"
trap cleanup 0
trap 'exit 1' 1 2 15

requested=${1:-latest}
if [ "$requested" = latest ]; then
    release_json="$tmpdir/release.json"
    download "$api_url" "$release_json" || die "could not determine the latest release"
    tag=$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$release_json" | sed -n '1p')
    [ -n "$tag" ] || die "latest release did not include a tag"
else
    tag=$requested
fi

case "$tag" in
    v*) version=${tag#v} ;;
    *) version=$tag ;;
esac
case "$version" in
    ''|*[!ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789._-]*)
        die "invalid version: $tag"
        ;;
esac

asset="slashie_${version}_${asset_suffix}.tar.gz"
archive="$tmpdir/$asset"
download "$download_base/v$version/$asset" "$archive" || die "could not download release asset $asset"

members=$(tar -tzf "$archive") || die "could not read release archive"
if ! printf '%s\n' "$members" | awk '
    $0 == "slashie" { slashie++; next }
    $0 == "LICENSE" { license++; next }
    { invalid = 1 }
    END { exit !(slashie == 1 && license <= 1 && !invalid) }
'; then
    die "release archive must contain exactly one slashie and optionally LICENSE"
fi
tar -xzf "$archive" -C "$tmpdir" slashie || die "could not extract the slashie member"
[ -f "$tmpdir/slashie" ] || die "release archive did not produce a regular slashie file"
[ ! -L "$tmpdir/slashie" ] || die "release archive produced an unsafe slashie symlink"
chmod 755 "$tmpdir/slashie" || die "could not prepare the downloaded binary"

mkdir -p "$install_dir" || die "could not create install directory $install_dir"
target="$install_dir/slashie"
if [ -L "$target" ]; then
    die "refusing to replace symlink $target"
fi
if [ -e "$target" ] && [ ! -f "$target" ]; then
    die "refusing to replace non-regular file $target"
fi

staged=$(mktemp "$install_dir/.slashie.XXXXXX") || die "could not create a staging file in $install_dir"
cp "$tmpdir/slashie" "$staged" || die "could not stage the downloaded binary"
chmod 755 "$staged" || die "could not set executable permissions"
mv "$staged" "$target" || die "could not install $target"
staged=

printf 'Installed Slashie %s at %s\n' "$version" "$target"
