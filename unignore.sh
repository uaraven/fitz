#!/usr/bin/env bash
set -euo pipefail

if [ $# -ne 1 ]; then
    echo "Usage: $0 <directory>" >&2
    exit 1
fi

dir="$1"

if [ ! -d "$dir" ]; then
    echo "$0: not a directory: $dir" >&2
    exit 1
fi

find "$dir" -type f -name '*.ignored' -print0 | while IFS= read -r -d '' file; do
    mv -- "$file" "${file%.ignored}"
done
