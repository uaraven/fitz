#!/bin/sh
set -e

cd /opt/fitz

cargo clean && \
  cargo build --release && \
  strip -s target/release/fitsmith && \
  strip -s target/release/fitz

cargo generate-rpm -p fitsmith && cp target/generate-rpm/*.rpm /opt/results/
cargo bundle -p fitsmith --release -f deb && cp target/release/bundle/deb/*.deb /opt/results/

cargo clean
