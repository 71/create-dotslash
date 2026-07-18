#!/bin/sh

test $# -eq 0 || (echo "Usage: $0"; echo; echo "Update Dotslash bootstrap script."; exit 1)

cd "$(dirname "$0")"

cargo run -- github:facebook/dotslash --force --format sh --output .
