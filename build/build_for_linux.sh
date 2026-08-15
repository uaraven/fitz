#!/bin/sh
set -e

IMAGE="fitz-linux-build:latest"

docker buildx build --platform linux/amd64 -f build/linux/Dockerfile -t "$IMAGE" .

PROJECT_DIR="$(realpath $0)/../../"
PROJECT_DIR="$(realpath $PROJECT_DIR)/dist"

echo "PROJECT_DIR=$PROJECT_DIR"

mkdir -p "$PROJECT_DIR"

docker run --rm -v "$PROJECT_DIR:/opt/results" --name "fitz-builder" "$IMAGE"

docker rmi "$IMAGE"
