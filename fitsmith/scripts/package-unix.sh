#!/usr/bin/env bash
# Build native FitSmith packages for the current OS (macOS -> .dmg, Linux -> .deb + .rpm).
# Run from anywhere; paths below are resolved relative to this script.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"

os="$(uname -s)"
case "$os" in
    Darwin) formats=(osx) ;;
    Linux) formats=(deb rpm) ;;
    *)
        echo "package-unix.sh: unsupported OS '$os' (use package-windows.ps1 on Windows)" >&2
        exit 1
        ;;
esac

if ! cargo bundle --help >/dev/null 2>&1; then
    echo "cargo-bundle not found; installing it (cargo install cargo-bundle)..."
    cargo install cargo-bundle
fi

if [[ " ${formats[*]} " == *" rpm "* ]] && ! command -v rpmbuild >/dev/null 2>&1; then
    echo "rpmbuild not found; skipping the .rpm package. Install it with:" >&2
    echo "  Fedora/RHEL: sudo dnf install rpm-build" >&2
    echo "  Debian/Ubuntu: sudo apt install rpm" >&2
    formats=("${formats[@]/rpm}")
fi

# Run from fitsmith/ itself: cargo-bundle resolves the icon/resources globs in
# [package.metadata.bundle] relative to the current directory, not the manifest's.
cd "$repo_root/fitsmith"

for fmt in "${formats[@]}"; do
    [ -z "$fmt" ] && continue
    cargo bundle -p fitsmith --release -f "$fmt"

    bundle_dir="$repo_root/target/release/bundle/$fmt"

    if [ "$fmt" = "osx" ]; then
        app_path="$bundle_dir/FitSmith.app"
        version="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
        dmg_path="$bundle_dir/FitSmith-$version.dmg"
        rm -f "$dmg_path"
        hdiutil create -volname FitSmith -srcfolder "$app_path" -ov -format UDZO "$dmg_path"
        echo "Packaged: $dmg_path"
    else
        pkg_path="$(find "$bundle_dir" -maxdepth 1 -name "*.$fmt" | head -n1)"
        echo "Packaged: $pkg_path"
    fi
done
