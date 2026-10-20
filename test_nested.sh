#!/usr/bin/env bash

set -e

echo "==> Building Niri from source..."
cargo build

echo "==> Terminating any existing nested Niri sessions..."
pkill -f "niri --nested" || true
sleep 1

echo "==> Starting nested Niri sandbox (WAYLAND_DISPLAY=wayland-2)..."
WAYLAND_DISPLAY=wayland-1 ./target/debug/niri --nested > /tmp/niri-nested.log 2>&1 &

echo "==> Waiting for Niri to initialize..."
sleep 3
echo "==> Nested session launched successfully."
