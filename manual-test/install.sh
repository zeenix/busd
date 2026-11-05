#!/bin/sh
# Builds busd and the test client, installs them in ~/.local/bin as `busd-test` and
# `handover-check`, and installs the units of the test buses for the user's systemd.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
repo=$(dirname "$here")
bin="$HOME/.local/bin"
units="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"

cargo build --release --manifest-path "$repo/Cargo.toml"
cargo build --release --manifest-path "$here/handover-check/Cargo.toml"
install -D "$repo/target/release/busd" "$bin/busd-test"
install -D "$here/handover-check/target/release/handover-check" "$bin/handover-check"
mkdir -p "$units"
cp "$here"/units/* "$units/"
systemctl --user daemon-reload

echo "Installed $bin/busd-test, $bin/handover-check and the units in $units."
