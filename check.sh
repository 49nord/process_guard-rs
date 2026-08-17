#!/bin/sh
set -eu

cd "$(dirname "$0")"

./format.sh --check
cargo clippy --all-targets -- -D warnings
