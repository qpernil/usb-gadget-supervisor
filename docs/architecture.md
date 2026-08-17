# Architecture

## Purpose

Linux USB gadget setup requires privileges that device protocol implementations
should not possess. The supervisor isolates those privileges in a small process
and gives an unprivileged worker direct access to only the USB and local hardware
resources required by its selected profile.

The design generalizes the process boundary already present in
`virtual-yubikey`: a root process configures the gadget and starts a fresh copy
as an unprivileged protocol worker. The extracted project makes that boundary an
explicit cross-project contract.

## Process model

```text
Host computer
     |
     | USB
     v
Linux UDC
     |
     +-- ConfigFS identity and function composition
     |
     +-- FunctionFS endpoint files --------------------+
                                                       |
                        +------------------------------v--+
                        | unprivileged device worker      |
                        |                                 |
                        | protocol, crypto, state, policy |
                        +---------------------------------+
     ^
     |
usb-gadget-supervisor
  - owns the UDC lock
  - creates and removes ConfigFS objects
  - mounts FunctionFS with worker ownership
  - starts, monitors, and stops the worker
  - binds and unbinds the UDC
```

The supervisor is not in the endpoint data path. This avoids an unnecessary
copy, keeps device latency predictable, and prevents the root process from
handling secrets or attacker-controlled protocol payloads beyond kernel and
lifecycle metadata.

## Ownership

| Resource | Owner | Rationale |
| --- | --- | --- |
| `/sys/kernel/config/usb_gadget/...` | Supervisor | Requires root and controls host-visible identity |
| UDC bind/unbind | Supervisor | Global lifecycle and crash containment |
| FunctionFS mount | Supervisor | Requires privilege and correct worker ownership |
| FunctionFS descriptors | Worker | They are part of the device-specific USB contract |
| FunctionFS data endpoints | Worker | Direct device protocol path |
| ConfigFS HID function creation | Supervisor from profile | Requires root; descriptor is supplied by the device project |
| Device state and private keys | Worker | Must never enter the privileged process |
| I2C/GPIO UI | Device worker or narrowly passed descriptors | Device-specific, not part of generic USB supervision |
| Logs | Both, with separate component labels | Lifecycle and protocol diagnostics have different sensitivity |

## Lifecycle

```text
Idle
  |
  v
Preparing
  - acquire exclusive lock
  - ensure ConfigFS/libcomposite
  - validate profile
  - create gadget but do not bind it
  - mount FunctionFS
  |
  v
AwaitingWorker
  - drop worker UID/GID and supplementary groups
  - set no-new-privileges and parent-death signal
  - start worker with control descriptor
  - wait for FunctionFS-ready notification
  |
  v
Binding
  - link functions in deterministic interface order
  - bind selected UDC
  - prepare any post-bind HID device nodes
  - notify worker that USB is attached
  |
  v
Running
  - monitor worker and termination signals
  - service lifecycle requests such as reconnect
  |
  v
Stopping
  - unbind UDC first
  - terminate worker
  - unmount FunctionFS
  - remove ConfigFS tree and runtime paths
  |
  v
Idle
```

Any failure after gadget creation follows the same reverse-order cleanup. A
worker exit while bound is an error: the supervisor unbinds immediately so the
host never remains connected to a device with no protocol owner.

## Profiles and workers

A root-owned profile selects exactly one worker and describes the USB identity,
configuration, ordered functions, FunctionFS mounts, and runtime account. The
profile is installed by the device project, not edited by a worker at runtime.

The initial worker types are:

- `virtual-yubikey-worker`: native Rust FIDO HID and CCID implementation.
- `trezor-one-pi`: upstream legacy Trezor firmware linked against a Linux/Pi
  hardware abstraction library.
- `virtual-yubihsm-worker`: future implementation of the documented YubiHSM 2
  command and object model.

The supervisor treats all of them as external commands speaking the same
lifecycle protocol.

## One UDC, one identity

A USB device descriptor has one VID/PID even when it has several interfaces or
configurations. Consequently, combining YubiKey, Trezor, and YubiHSM interfaces
into one composite gadget would not faithfully emulate any of the devices.

On Raspberry Pi 4 and 5, the USB-C device controller therefore runs one profile
at a time. Switching profiles requires unbind, cleanup, construction of the new
profile, and rebind. Hosts correctly observe a physical disconnect and a newly
attached device.

Multiple simultaneous identities require multiple UDCs or additional physical
USB-device hardware and are outside the first implementation.

## Trust boundary

The supervisor is trusted to configure the kernel gadget correctly, select the
approved worker binary, and enforce process credentials. It must not parse
wallet commands, APDUs, CTAP messages, Trezor protobuf messages, PINs, seeds, or
private-key material.

Profiles must be regular root-owned files, must not be writable by the worker,
and must use a strict schema. Worker executable paths and auxiliary descriptor
files must be absolute, non-symlinked where practical, and not worker-writable.
Profile-declared local hardware is opened by the supervisor before credential
drop and inherited by descriptor. The worker owns every ioctl, framebuffer,
button, and UI policy decision; the supervisor is only a narrow descriptor
broker.

Workers remain development-grade software security devices. Process separation
reduces accidental privilege exposure; it does not provide physical tamper
resistance, secure-element properties, certification, or side-channel
hardening.
