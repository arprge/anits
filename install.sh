#!/bin/sh

set -e

if ! command -v cargo >/dev/null 2>&1; then
    echo "Error: Rust's cargo toolchain is required to install anits."
    echo "Install Rust via https://rustup.rs and re-run:"
    echo "  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
    exit 1
fi

if [ -f "Cargo.toml" ]; then
    cargo install --path . --locked
else
    cargo install anits --locked
fi

echo ""
echo "anits is installed! Run 'anits' to launch."
