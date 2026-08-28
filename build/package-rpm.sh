#!/usr/bin/env bash
# Build the FitSmith .rpm package (Linux only). Run from anywhere; paths below are resolved
# relative to this script.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"

os="$(uname -s)"
if [ "$os" != "Linux" ]; then
    echo "package-rpm.sh: .rpm packages can only be built on Linux (current OS: '$os')" >&2
    exit 1
fi

if ! command -v rpmbuild >/dev/null 2>&1; then
    echo "rpmbuild not found; install it with:" >&2
    echo "  Fedora/RHEL: sudo dnf install rpm-build" >&2
    echo "  Debian/Ubuntu: sudo apt install rpm" >&2
    exit 1
fi

if ! cargo generate-rpm --help >/dev/null 2>&1; then
    echo "cargo-generate-rpm not found; installing it (cargo install cargo-generate-rpm)..."
    cargo install cargo-generate-rpm
fi

# Run from the repo root: [package.metadata.generate-rpm] in fitsmith/Cargo.toml lists
# asset paths (target/release/..., assets/...) relative to it, and the shared workspace
# target/ dir lives there too.
cd "$repo_root"

cargo build --release -p fitsmith -p fitz
strip -s target/release/fitsmith target/release/fitz

cargo generate-rpm -p fitsmith

rpm_path="$(find target/generate-rpm -maxdepth 1 -name '*.rpm' | head -n1)"
echo "Packaged: $rpm_path"
