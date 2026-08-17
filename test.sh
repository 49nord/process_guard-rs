#!/bin/sh
set -eu

cd "$(dirname "$0")"

cargo test
