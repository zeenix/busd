# Manually testing restarts without dropping clients

What to check: `systemctl --user restart` replaces busd with a new instance while its clients stay
connected, keep their unique names, well-known names and match rules, and lose no message. A stop,
by contrast, disconnects them.

Everything runs as your user, on test buses of their own (`$XDG_RUNTIME_DIR/busd-test` and
`$XDG_RUNTIME_DIR/busd-test2`), so your real session bus is left alone.

Needs: systemd 254 or later (`systemctl --version`), a Rust toolchain, `busctl` (systemd) and
`dbus-monitor` (`dbus-tools` on Fedora).

## What's here

- `units/`: user units of the test buses. `busd-test.socket` and `busd-test.service` are a bus
  started through socket activation; `busd-test2.service` is one with no socket activation.
- `handover-check/`: three long-lived zbus clients in one binary:
  - `handover-check serve` owns `org.zbus.HandoverTest`, answers `Ping`, and emits a `Tick(n)`
    signal every 10 ms.
  - `handover-check listen` subscribes to the ticks and reports any that go missing or come twice.
  - `handover-check ping` calls `Ping` in a loop and reports errors and the slowest round trip of
    each second.

  Each prints its unique name when it connects, a line a second, and exits once its connection to
  the bus is gone.
- `install.sh`: builds busd and `handover-check`, installs them as `~/.local/bin/busd-test` and
  `~/.local/bin/handover-check`, installs the units, and reloads systemd.
- `uninstall.sh`: stops the test buses and removes all of that.

## 1. Install

```sh
manual-test/install.sh
systemctl --user start busd-test.socket
```

## 2. Start the clients

In every terminal from here on:

```sh
export DBUS_SESSION_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/busd-test
```

Then, each in its own terminal, in this order (the first connection starts busd through the socket):

1. `journalctl --user -fu busd-test.service`
2. `~/.local/bin/handover-check serve`
3. `~/.local/bin/handover-check listen`
4. `~/.local/bin/handover-check ping`
5. `dbus-monitor --address $DBUS_SESSION_BUS_ADDRESS` (a monitor, which is handed over too)

And take a snapshot of who's connected:

```sh
busctl --address=$DBUS_SESSION_BUS_ADDRESS list | tee /tmp/before.txt
```

## 3. Restart

```sh
systemctl --user restart busd-test.service
```

Expected:

- The journal shows `Handing over N clients and M names.` from the old instance, then `Took N
  clients over from the previous instance of the bus.` from the new one.
- `serve`, `listen`, `ping` and `dbus-monitor` keep running; none of them reconnects or prints an
  error.
- `listen` keeps reporting `missing 0, repeated 0`.
- `ping` keeps reporting `0 errors`. Its slowest round trip in the second of the restart is how long
  the clients were paused.
- `busctl --address=$DBUS_SESSION_BUS_ADDRESS list | diff /tmp/before.txt -` shows the same unique
  names, with `org.zbus.HandoverTest` still owned by the `serve` connection. The only differences
  should be the PID of `org.freedesktop.DBus` and the `busctl` connection itself.
- `systemd-analyze --user fdstore busd-test.service` lists nothing: busd takes everything out of the
  store as it takes over.

Then a burst of restarts, under the same load:

```sh
for i in $(seq 20); do systemctl --user restart busd-test.service; sleep 0.5; done
```

Same expectations. Also run the burst while clients keep connecting, in yet another terminal:

```sh
while busctl --address=$DBUS_SESSION_BUS_ADDRESS list > /dev/null; do :; done; echo FAILED
```

It should still be running, with no `FAILED`, once the burst is over: a client that connects
during a restart waits for the next instance, and one in the middle of its handshake gets to
finish it. Last, stop one of the clients (Ctrl-C on `ping`) and restart: its unique name goes, and
`NameOwnerChanged` for it shows up in `dbus-monitor`.

### An update

Make any change to busd, run `manual-test/install.sh` again (it only replaces the binary), and
restart. The clients should carry on as above. Both binaries have to be builds of this branch, or
of the PR's: an instance that can't take over what the previous one stored (as after a change of
the format of its state) starts afresh and disconnects everyone, which is worth seeing once too.

## 4. A stop is not a restart

```sh
systemctl --user stop busd-test.service
```

Expected: all the clients are disconnected (`serve`, `listen` and `ping` print that the connection
to the bus is gone and exit). The socket unit stays, so the next client starts a fresh bus.

## 5. Without socket activation

busd then keeps the socket that it listens on in the store too, which takes the address of a socket
file. `busd-test2.service` is such a bus, at `$XDG_RUNTIME_DIR/busd-test2`:

```sh
systemctl --user start busd-test2.service
export DBUS_SESSION_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/busd-test2
```

Run the clients of step 2 against it (in fresh terminals with that address), restart with
`systemctl --user restart busd-test2.service`, and expect the same as in step 3. Then:

- After `systemctl --user stop busd-test2.service`, the socket file `$XDG_RUNTIME_DIR/busd-test2` is
  left behind (busd can't tell a stop from a restart).
- `systemctl --user start busd-test2.service` replaces it, and clients connect again.

## 6. A client from outside the bus's PID namespace

Such a client has no PID that busd can see (the kernel gives 0), and busd used to turn it away. Run
busd in a PID namespace of its own:

```sh
unshare --user --map-current-user --pid --fork --mount-proc \
    ~/.local/bin/busd-test --address unix:path=$XDG_RUNTIME_DIR/busd-ns
```

And from an ordinary shell:

```sh
busctl --address=unix:path=$XDG_RUNTIME_DIR/busd-ns list
```

Expected: the list, with your `busctl` connection in it, without a PID. A busd built from upstream
main refuses the connection.

## 7. Clean up

```sh
manual-test/uninstall.sh
```

## What the automated tests cover instead

`cargo test` covers what's hard to set up by hand: a client in the middle of sending a message, file
descriptors sent ahead (including the same one twice, and where the kernel can't tell whether two
are the same), clients that close their end or that a send fails to as the bus hands over, stalled
clients and receivers, per-user quotas (including handshakes that stall), a small fd store and a
low limit of open files, and a failed take-over (`tests/handover.rs`, `tests/fdstore.rs` with a
fake systemd, `tests/stalled_client.rs`).
