<img alt="Project logo: a bus" src="data/logo.svg" width="200">

[![Build Status](https://img.shields.io/github/actions/workflow/status/z-galaxy/busd/ci.yml?branch=main)](https://github.com/z-galaxy/busd/actions?query=branch%3Amain)
[![crates.io](https://img.shields.io/crates/v/busd.svg)](https://crates.io/crates/busd)

# busd

A D-Bus bus (broker) implementation in Rust. Since it's pure Rust, it's much easier to cross-build
than other D-Bus brokers.

## Status

Alpha. It's not ready for production use yet. Only the essentials are in place.

## Performance

busd keeps up with [dbus-broker], the broker that most systems run, where the bus is busy, and is
well ahead of [dbus-daemon]. Measured with [dbus-bench] on a laptop with an AMD Ryzen 7 7840U,
running Fedora 44, as the median of 15 runs:

| Broker             | 1 client | 20 clients | CPU time with 20 clients | Peak memory |
|--------------------|---------:|-----------:|-------------------------:|------------:|
| busd               |  0.088 s |    0.926 s |                   0.42 s |     6.3 MiB |
| dbus-broker 37     |  0.082 s |    0.930 s |                   0.41 s |     7.9 MiB |
| dbus-daemon 1.16.2 |  0.160 s |    1.185 s |                   1.14 s |     6.0 MiB |

Each client makes 1000 method calls with 16 KiB of data each, and the 20 clients make theirs all
at once. With 20 clients, the benchmark's client keeps its own CPU busy with busd and dbus-broker
alike, so the CPU time that the broker needs is what tells those two apart. The memory of
dbus-broker is that of its two processes, its launcher and the broker proper, together. The
[results] in dbus-bench have the details, and how busd fares on other machines.

## Installation & Use

Currently, we can only offer installation from source:

```bash
cargo install -f busd
```

Running a session instance is super easy:

```bash
busd --print-address
```

`--print-address` will print the address of the bus to stdout. You can then use that address to
connect to the bus:

```bash
export DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/1000/bus,guid=d0af79a44c000ce7985797ba649dbc05"
busctl --user introspect org.freedesktop.DBus /org/freedesktop/DBus
busctl --user list
```

Since auto-starting of services is not yet implemented, you'll have to start services manually:

```bash
# Probably not the best example since the service just exits after a call to it.
/usr/libexec/dleyna-renderer-service &
busctl call --user com.intel.dleyna-renderer /com/intel/dLeynaRenderer com.intel.dLeynaRenderer.Manager GetRenderers
```

## The plan

### Full compatibility with the D-Bus specification

Implement all features that manadated and recommended by the [specification].

### Additional Header Fields

While the D-Bus spec does not allow custom header fields in messages, `busd` will still support a
few additional on-demand (only) fields, that are useful for certain applications. One example is
addition of [peer credentials] on every message, which can avoid round-trips on the bus.

## License

[MIT](LICENSE-MIT)

[specification]: https://dbus.freedesktop.org/doc/dbus-specification.html
[peer credentials]: https://github.com/z-galaxy/busd/issues/29
[dbus-broker]: https://github.com/bus1/dbus-broker
[dbus-daemon]: https://gitlab.freedesktop.org/dbus/dbus
[dbus-bench]: https://github.com/zeenix/dbus-bench
[results]: https://github.com/zeenix/dbus-bench/tree/main/results
