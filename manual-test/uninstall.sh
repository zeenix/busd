#!/bin/sh
# Stops the test buses and removes what install.sh installed.
set -u

bin="$HOME/.local/bin"
units="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"

systemctl --user stop busd-test.socket busd-test.service busd-test2.service
rm -f "$units/busd-test.socket" "$units/busd-test.service" "$units/busd-test2.service"
systemctl --user daemon-reload
rm -f "$bin/busd-test" "$bin/handover-check"
rm -f "$XDG_RUNTIME_DIR/busd-test2" "$XDG_RUNTIME_DIR/busd-ns"
