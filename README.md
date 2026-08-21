# USB Gadget Supervisor

[![CI](https://github.com/qpernil/usb-gadget-supervisor/actions/workflows/ci.yml/badge.svg)](https://github.com/qpernil/usb-gadget-supervisor/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Status: alpha](https://img.shields.io/badge/status-alpha-orange.svg)](#status)

`usb-gadget-supervisor` is a privilege-separated Linux service for
running protocol-compatible USB device workers on a USB Device Controller
(UDC), especially on Raspberry Pi 4 and Raspberry Pi 5.

Deployment is tested on both 64-bit Ubuntu and 64-bit Raspberry Pi OS. On
either system, the normal board-specific prerequisite is to enable DWC2 in
peripheral mode; the supervisor then uses the resulting UDC through ConfigFS
and FunctionFS.

The supervisor owns only the privileged mechanics of Linux USB gadget mode:
ConfigFS, FunctionFS mounts, UDC binding, process credentials, lifecycle, and
cleanup. Device behavior belongs to separate unprivileged workers such as
`virtual-yubikey`, `virtual-trezor`, and a future `virtual-yubihsm`.

The goal is a deliberately small privileged boundary: the supervisor performs
the Linux operations that require root, while each device implementation stays
in its own independently testable process.

## Architecture

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

Endpoint data is never proxied through the supervisor. The supervisor validates
and publishes profile-owned FunctionFS descriptors, then transfers the open
endpoint files to the worker. The supervisor retains control only over gadget
configuration and lifecycle. Device profiles may also request validated
Microsoft OS 1.0/WinUSB metadata and a WebUSB BOS capability; the supervisor
publishes their required global ConfigFS settings.

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
- `systemctl reload` requests the same clean incarnation rebuild without
  restarting the supervisor process.
- The supervisor does not pretend that one UDC can expose multiple independent
  USB device identities simultaneously.
- Software-backed devices are compatibility and development tools, not
  substitutes for tamper-resistant hardware.

## Scope

The supervisor:

- load one strictly validated, root-owned device profile;
- create and tear down ConfigFS gadgets and FunctionFS mounts;
- open declared local character devices and claim exact GPIO line groups before
  dropping privileges;
- start one worker with inherited endpoint and control descriptors;
- drop the worker to a configured unprivileged account;
- bind the gadget only after the worker reports readiness; and
- unbind immediately if the worker exits or violates the control protocol.

It will not implement FIDO, CCID, Trezor, YubiHSM, cryptography, key storage, or
device UI. Those concerns stay in the worker repositories.

## Build

Rust 1.85 or later is required. The binary is Linux-only, while profile and
wire-format unit tests also run on macOS:

```sh
cargo build --release --locked
cargo test --locked
```

Install the privileged boundary in one root-owned directory:

```sh
sudo install -d -o root -g root -m 0755 \
  /opt/usb-gadget-supervisor/profiles
sudo install -o root -g root -m 0755 \
  target/release/usb-gadget-supervisor \
  /opt/usb-gadget-supervisor/usb-gadget-supervisor
sudo install -o root -g root -m 0644 \
  systemd/usb-gadget-supervisor@.service \
  /opt/usb-gadget-supervisor/usb-gadget-supervisor@.service
sudo systemctl link \
  /opt/usb-gadget-supervisor/usb-gadget-supervisor@.service
sudo systemctl daemon-reload
```

This is the only special installation directory. The systemd link under
`/etc/systemd/system` contains no second copy. Each device repository builds
its unprivileged worker in place and installs only its root-owned profile into
`/opt/usb-gadget-supervisor/profiles`. Runtime mounts and persistent state stay
under `/run`, `/dev`, and `/var/lib`; they are data, not installed program
copies.

For example, after cloning and building `virtual-trezor`, its installation is:

```sh
sudo install -o root -g root -m 0644 \
  profiles/virtual-trezor.toml \
  /opt/usb-gadget-supervisor/profiles/virtual-trezor.toml
sudo systemctl enable --now \
  usb-gadget-supervisor@virtual-trezor.service
```

The profile points directly to the worker in that clone's build directory.
Updating the worker is therefore `git pull`, rebuild, and restart. Only profile
changes require reinstalling the profile. Since one UDC can expose only one
identity, stop the currently active profile before starting another one.

The worker file may be owned by root or by `run_as`. It may also be writable by
the `run_as` primary group, which accommodates a normal collaborative build
umask. Set-ID, world-writable, and unrelated-group-writable workers are
rejected. This does not cross the root boundary because the worker is executed
only after the supervisor has dropped to that identity.

The same profile can be run manually after stopping its service:

```sh
sudo /opt/usb-gadget-supervisor/usb-gadget-supervisor \
  --profile /opt/usb-gadget-supervisor/profiles/virtual-yubikey.toml
```

An optional `--udc NAME` selects a controller instead of the first available
entry in `/sys/class/udc`.

Profiles can be schema-checked without root or USB hardware:

```sh
/opt/usb-gadget-supervisor/usb-gadget-supervisor \
  --check-profile --profile /absolute/path/to/profile.toml
```

The selected worker receives a private `AF_UNIX/SOCK_SEQPACKET` control socket,
state/runtime directory paths, exact USB file-descriptor bundles transferred
with `SCM_RIGHTS`, and any profile-approved local-hardware file descriptors.
FunctionFS and HID paths are never exposed to the worker. This lets
I2C and GPIO device nodes remain root-only while display and button semantics
stay entirely inside the device worker. See the
[worker protocol](docs/worker-protocol.md) for the exact contract.

## Documents

- [Architecture](docs/architecture.md)
- [Worker protocol](docs/worker-protocol.md)
- [Profile format](docs/profile-format.md)
- [Trezor One worker](docs/trezor-one.md)
- [Raspberry Pi validation](docs/raspberry-pi-validation.md)

## Status

The supervisor, Virtual YubiKey worker, and Virtual Trezor worker implement the
same fixed version-1 resource protocol and schema-1 profiles. Profile parsing is
strict, FunctionFS resources are derived from installed descriptor blobs, and
USB FDs are passed with `SCM_RIGHTS`. Unit tests and Raspberry Pi-targeted Rust
type checks pass. Run the hardware checklist before treating a profile as
deployable.

## Contributing

This project is currently alpha quality. Design reviews, Linux USB gadget
experience, and Raspberry Pi hardware reports are welcome. Please read
[CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request.

Security-sensitive reports should follow [SECURITY.md](SECURITY.md).

## License

Licensed under the [MIT License](LICENSE).
