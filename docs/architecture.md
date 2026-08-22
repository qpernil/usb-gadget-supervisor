# Architecture

## Boundary

Linux USB gadget construction requires root; emulated-device behavior does
not. The supervisor owns privileged construction, authority, and lifecycle.
One unprivileged worker owns USB identity, firmware behavior, protocol data,
UI, secrets, and persistent device state.

```text
                        CBOR USB personality
unprivileged worker -----------------------------> root supervisor
       ^                                                |
       | control records + packet proxy sockets         | ConfigFS,
       +------------------------------------------------+ FunctionFS, UDC
                                                        |
host USB <---------------- Linux kernel <----------------+
```

The supervisor owns every Linux FunctionFS file. It translates `ep0` events
to typed control/lifecycle records and uses small generation-scoped pumps to
preserve data-endpoint packets over nonblocking `SOCK_SEQPACKET` proxies. It
does not interpret device-protocol payloads.

## Ownership

| Resource or decision | Owner |
| --- | --- |
| Root-owned launch/resource TOML | Device project, enforced by supervisor |
| USB identity and descriptor content | Worker or emulated firmware |
| CBOR schema and firmware discovery parser | Shared `usb-gadget-worker` crate |
| CBOR, USB, and FunctionFS structural validation | Supervisor |
| ConfigFS objects and FunctionFS publication | Supervisor |
| UDC discovery, bind, unbind, and re-enumeration | Supervisor |
| Worker credentials and process lifecycle | Supervisor |
| FunctionFS `ep0` and blocking data endpoint files | Supervisor |
| Runtime USB control meaning and firmware dispatch | Worker |
| Packet-preserving endpoint pumps | Supervisor |
| CTAP, CCID, Trezor, or YubiHSM endpoint traffic | Worker |
| Private keys, wallet state, policy, display, buttons | Worker |

The disk profile contains no USB descriptors. The worker publishes one complete
typed configuration at startup and may publish a replacement later.

## Worker-side USB description

The shared Rust crate defines `UsbPersonality`, Serde CBOR encoding, and a
generic discovery parser:

```text
discover(max_speed, control_transfer_callback) -> UsbPersonality
```

The parser directly performs the discovery a USB stack needs: device,
configuration, language and string descriptors, plus recognized Microsoft OS
1.0 and WebUSB capabilities. Its result is a semantic configuration object,
not a list of cached setup-packet responses.

This supports three worker shapes without changing the supervisor:

- a Rust worker constructs and serializes the typed object;
- a static native worker embeds a known CBOR blob; or
- a firmware emulator adapts its virtual EP0/control engine to the discovery
  callback and lets genuine firmware answer descriptor requests.

The C ABI exposes only the discovery call and its result deallocator. CBOR
construction and parsing remain in Rust.

## Supervisor projection

Before changing the serving gadget, the supervisor decodes and validates the
entire candidate. It extracts device identity, configuration attributes,
endpoint topology, strings, Microsoft compatible IDs/properties, and WebUSB
metadata. It then builds:

- ConfigFS device/configuration attributes and strings;
- ConfigFS Microsoft OS 1.0 and WebUSB attributes;
- a FunctionFS v2 descriptor blob and string table; and
- the ordered endpoint map used when transferring proxy FDs to the worker.

The kernel remains the final USB validator. The supervisor parser provides
early diagnostics, prevents authority mismatches, and derives exactly the
endpoints the worker is allowed to use. The accepted typed object is logged
with Rust `Debug`; its exact CBOR is retained under `/run` for inspection.

## USB-generation state machine

```text
Start worker
  send named local-resource FDs
       |
       v
Await Configure(0, request)
  decode and validate CBOR
       |
       v
Build generation 1
  create ConfigFS + FunctionFS
  publish descriptors
  create FunctionFS endpoint pumps
  send nonblocking endpoint proxies
       |
       v
Await Serving(1, request)
  bind UDC
       |
       v
Serving
  worker may send replacement Configure
       |
       +-- invalid --> reject; generation continues serving
       |
       +-- valid --> Quiesce -> unbind/remove -> build N+1 -> bind
```

The worker survives a valid USB reconfiguration. It closes the old generation
only after `Quiesce` and receives new FDs after the supervisor has rebuilt the
kernel objects. This is the software equivalent of firmware-driven disconnect,
personality change, and host re-enumeration.

A worker process remains the broader fault-reset boundary. Worker exit or
control-socket EOF tears down USB and starts a fresh process whose protocol
generation begins at zero. Service stop tears down without restart. `SIGHUP`
transactionally validates the disk launch profile, then replaces the worker.

## Capability transfer

An open file descriptor is both a channel and a capability. The supervisor can
open a root-only endpoint, I2C/SPI node, or GPIO line request and pass a
duplicate with `SCM_RIGHTS`. The worker can use that exact open description but
cannot reopen the path or acquire adjacent devices.

GPIO resources use Linux's v2 line API. The worker receives the line-request
FD rather than the GPIO-chip FD, preserving exact line ownership, direction,
bias, active-low interpretation, and edge subscription chosen by the profile.

The USB endpoint capabilities are deliberately different. The worker receives
only nonblocking packet sockets; the supervisor retains `ep0` and every raw
FunctionFS endpoint. That keeps Linux blocking and cancellation rules out of
device code and leaves room for future supervisor-side access control without
exposing ConfigFS or FunctionFS authority.

## Idle behavior

The control socket, endpoint proxies, and GPIO event handles are pollable. A
worker blocks in one kernel `poll` until USB, lifecycle, GPIO, or an internal
deadline is ready. Supervisor pump threads absorb FunctionFS's synchronous
endpoint waits. This reproduces the interrupt-or-timer wait that prevents a
busy loop on real hardware while still servicing automatic lock, animations,
retries, and other firmware deadlines.

Host suspend is not a generation boundary. The worker, firmware state,
configuration, and proxies survive `SUSPEND`/`RESUME`; a reset-like
`DISABLE`/`ENABLE` sequence resets and reconfigures only the virtual USB
controller. The complete mapping is in [USB lifecycle](usb-lifecycle.md).

## UDC and Raspberry Pi

The supervisor discovers controllers through `/sys/class/udc`, sorts them, and
selects the first unless an exact `--udc` override is given. One UDC exposes one
USB device identity at a time. Pi 4 and Pi 5 use the same DWC2 peripheral-mode,
ConfigFS, and FunctionFS architecture; their UDC names differ, so no name is
hard-coded.

## Supported worker shapes

| Device | Kernel surface | USB-description source |
| --- | --- | --- |
| Virtual Trezor One | FunctionFS vendor interface | genuine legacy firmware control engine via shared discovery parser |
| Future Trezor Safe 3 | FunctionFS interfaces | Core firmware adapter via the same parser or a native Rust object |
| Virtual YubiKey | FunctionFS/HID as selected | native worker object; dynamic management changes can republish it |
| Virtual YubiHSM | FunctionFS vendor bulk | native static or constructed object |

## Trust statement

The supervisor is trusted to validate metadata, configure the kernel, transfer
capabilities, and launch the approved worker identity. It must not parse APDUs,
CTAP messages, Trezor protobufs, PINs, seeds, or private keys. Process
separation reduces privilege exposure; it does not make a Raspberry Pi
tamper-resistant hardware.
