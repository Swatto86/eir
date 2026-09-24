#!/usr/bin/env bash
# Verified prebuilt test tooling; never compile a driver during app CI.
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dest="$root/.webdriver"
version=2.0.6
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*)
    platform=windows
    target=x86_64-pc-windows-msvc
    binary=tauri-driver.exe
    expected=9c207f120daf1963d3c3972fac6690c371debcecee8cc511220a22e83617c402
    ;;
  Linux)
    platform=linux
    target=x86_64-unknown-linux-gnu
    binary=tauri-driver
    expected=edda0667c7a5f2297ca646e2616a0cdfa38658c20bbad5f11bcbceea14fcdb49
    ;;
  *) echo "This WebDriver setup supports Windows x64 and Linux x64." >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64|amd64) ;;
  *) echo "No reviewed tauri-driver archive is pinned for this architecture." >&2; exit 1 ;;
esac
mkdir -p "$dest"
archive="$dest/tauri-driver-$platform.tar.gz"
if [ ! -f "$archive" ]; then
  pending=$(mktemp "$dest/driver-download.XXXXXX")
  trap 'rm -f "$pending"' EXIT
  curl --fail --location --silent --show-error \
    "https://github.com/cargo-bins/cargo-quickinstall/releases/download/tauri-driver-$version/tauri-driver-$version-$target.tar.gz" \
    -o "$pending"
  printf '%s  %s\n' "$expected" "$pending" | sha256sum -c -
  mv "$pending" "$archive"
fi
# Check cached archives too, before extracting or executing anything.
printf '%s  %s\n' "$expected" "$archive" | sha256sum -c -
tar -xzf "$archive" -C "$dest" "$binary"
chmod +x "$dest/$binary"
echo "Verified tauri-driver $version in $dest. Add this directory to PATH."
