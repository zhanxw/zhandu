#!/bin/sh
# Install/upgrade a verified GitHub release without Rust or administrator access.
set -eu
fail() { printf '%s\n' "zhandu installer: $*" >&2; exit 1; }
for tool in curl tar awk uname mktemp; do
    command -v "$tool" >/dev/null 2>&1 || fail "Required command missing: $tool"
done
case "$(uname -s):$(uname -m)" in
    Darwin:arm64|Darwin:aarch64) target=aarch64-apple-darwin ;;
    Darwin:x86_64) target=x86_64-apple-darwin ;;
    Linux:aarch64|Linux:arm64) target=aarch64-unknown-linux-gnu ;;
    Linux:x86_64|Linux:amd64) target=x86_64-unknown-linux-gnu ;;
    *) fail 'Supported: macOS/Linux x86_64 or ARM64. On Windows, use install.ps1.' ;;
esac
if command -v sha256sum >/dev/null 2>&1; then
    checksum() { sha256sum "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then
    checksum() { shasum -a 256 "$1" | awk '{print $1}'; }
else
    fail 'sha256sum or shasum is required to verify downloads.'
fi
release_url=https://github.com/zhanxw/zhandu/releases
version=${ZHANDU_VERSION:-}
if [ -z "$version" ]; then
    resolved=$(curl --proto '=https' --tlsv1.2 -fsSL -o /dev/null -w '%{url_effective}' "$release_url/latest")
    version=${resolved##*/}
fi
# Only version tags may become download URLs or archive paths.
printf '%s\n' "$version" | LC_ALL=C awk '/^v[0-9]+\.[0-9]+\.[0-9]+$/ {ok=1} END {exit !ok}' || fail 'Version must have the form v0.6.1.'
archive="zhandu-$version-$target.tar.gz"
folder="zhandu-$version-$target"
work=$(mktemp -d)
staged=
trap 'rm -rf "$work"; if [ -n "$staged" ]; then rm -f "$staged"; fi' EXIT
trap 'exit 1' HUP INT TERM
printf 'Downloading zhandu %s (%s)...\n' "$version" "$target"
curl --proto '=https' --tlsv1.2 -fsSL "$release_url/download/$version/$archive" -o "$work/$archive"
curl --proto '=https' --tlsv1.2 -fsSL "$release_url/download/$version/SHA256SUMS" -o "$work/SHA256SUMS"
expected=$(awk -v name="$archive" '$2 == name {print $1; found++} END {if (found != 1) exit 1}' "$work/SHA256SUMS") || fail 'Archive is missing from the checksum manifest.'
[ "$(checksum "$work/$archive")" = "$expected" ] || fail 'Checksum mismatch; existing installation was not changed.'
tar -xzf "$work/$archive" -C "$work" "$folder/zhandu"
binary="$work/$folder/zhandu"
chmod 755 "$binary"
actual=$("$binary" --version) || fail 'Binary cannot run on this system. Linux requires glibc 2.35+ (x64) or 2.39+ (ARM64). Existing installation was not changed.'
[ "$actual" = "zhandu ${version#v}" ] || fail 'Downloaded binary has an unexpected version.'
install_dir=${ZHANDU_INSTALL_DIR:-"$HOME/.local/bin"}
mkdir -p "$install_dir"
[ ! -d "$install_dir/zhandu" ] || fail "Destination is a directory: $install_dir/zhandu"
staged=$(mktemp "$install_dir/.zhandu-install.XXXXXX")
cp "$binary" "$staged"
chmod 755 "$staged"
# Same-filesystem rename keeps the previous executable intact until replacement.
mv -f "$staged" "$install_dir/zhandu"
staged=
printf 'Installed %s to %s/zhandu\n' "$actual" "$install_dir"
case ":$PATH:" in
    *":$install_dir:"*) ;;
    *) printf 'Add this directory to PATH in your shell configuration: %s\n' "$install_dir"
       printf 'For the default directory, run: export PATH="$HOME/.local/bin:$PATH"\n' ;;
esac
printf 'Run this installer again to upgrade to the latest release.\n'
