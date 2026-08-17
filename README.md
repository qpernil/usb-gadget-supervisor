# USB Gadget Supervisor

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Status: design](https://img.shields.io/badge/status-design-orange.svg)](#status)

`usb-gadget-supervisor` is a privilege-separated Linux service design for
running protocol-compatible USB device workers on a USB Device Controller
(UDC), especially on Raspberry Pi 4 and Raspberry Pi 5.

The supervisor owns only the privileged mechanics of Linux USB gadget mode:
ConfigFS, FunctionFS mounts, UDC binding, process credentials, lifecycle, and
cleanup. Device behavior belongs to separate unprivileged workers such as
`virtual-yubikey`, `virtual-trezor`, and a future `virtual-yubihsm`.

This repository is documentation-first. It intentionally contains no runtime
implementation yet.

The goal is a deliberately small privileged boundary: the supervisor performs
the Linux operations that require root, while each device implementation stays
in its own independently testable process.

## Intended architecture

```text
                         USB-C device port
                                |
                     usb-gadget-supervisor
                (small, generic, privileged process)
                                |
                +---------------+---------------+
                |               |               |
        virtual-yubikey  virtual-trezor   virtual-yubihsm
          Rust worker      C worker       Rust/C worker
```

Only one profile can own a Raspberry Pi 4/5 UDC at a time. Switching profiles
is an intentional USB disconnect followed by re-enumeration with a different
device identity. Combining unrelated devices behind one composite VID/PID is
not a compatibility goal.

Endpoint data is never proxied through the supervisor. A worker publishes its
FunctionFS descriptors and reads and writes FunctionFS endpoint files directly.
The supervisor retains control only over gadget configuration and lifecycle.

## Project boundaries

| Project | Responsibility |
| --- | --- |
| `usb-gadget-supervisor` | ConfigFS, FunctionFS mounts, UDC ownership, privilege dropping, worker readiness, bind/unbind, cleanup |
| [`virtual-yubikey`](https://github.com/qpernil/virtual-yubikey) | YubiKey USB profile, FIDO HID, CCID, Management, PIV, FIDO2, state |
| `virtual-trezor` | Upstream Trezor firmware build, Pi HAL, Trezor descriptors, OLED/buttons, state |
| `virtual-yubihsm` | YubiHSM protocol, sessions, objects, capabilities, audit, state |

## Design principles

- The privileged process contains no cryptographic keys or device-protocol
  implementation.
- Workers run as an explicitly selected non-root account with no path back to
  root.
- Device profiles are declarative, root-owned, strictly validated, and owned by
  the corresponding device project rather than compiled into the supervisor.
- Workers communicate lifecycle state over a small versioned local control
  channel. USB payloads stay on FunctionFS or dedicated HID endpoint files.
- A worker crash causes immediate UDC unbind before teardown or restart.
- The supervisor does not pretend that one UDC can expose multiple independent
  USB device identities simultaneously.
- Software-backed devices are compatibility and development tools, not
  substitutes for tamper-resistant hardware.

## Scope

The planned supervisor will:

- load one strictly validated, root-owned device profile;
- create and tear down ConfigFS gadgets and FunctionFS mounts;
- start one worker with inherited endpoint and control descriptors;
- drop the worker to a configured unprivileged account;
- bind the gadget only after the worker reports readiness; and
- unbind immediately if the worker exits or violates the control protocol.

It will not implement FIDO, CCID, Trezor, YubiHSM, cryptography, key storage, or
device UI. Those concerns stay in the worker repositories.

## Documents

- [Architecture](docs/architecture.md)
- [Worker protocol](docs/worker-protocol.md)
- [Profile format](docs/profile-format.md)
- [Trezor One worker](docs/trezor-one.md)
- [Migration from Virtual YubiKey](docs/migration.md)

## Status

The first implementation target is extraction of the existing, working
ConfigFS/FunctionFS supervisor from `virtual-yubikey`, without changing the
YubiKey worker's observable USB behavior. The second worker will be the Trezor
One port; it will be used to validate which abstractions are genuinely common
before declaring a stable supervisor API.

See [migration.md](docs/migration.md) for the proposed delivery sequence.

## Contributing

This project is currently at the design stage. Design reviews, Linux USB gadget
experience, and Raspberry Pi hardware reports are welcome. Please read
[CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request.

Security-sensitive reports should follow [SECURITY.md](SECURITY.md).

## License

Licensed under the [MIT License](LICENSE).
