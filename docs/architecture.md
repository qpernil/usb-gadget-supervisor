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
       | control records + FunctionFS endpoint FDs      | ConfigFS,
       +------------------------------------------------+ FunctionFS ep0, UDC
                                                        |
host USB <---------------- Linux kernel <----------------+
```

The supervisor owns the FunctionFS mount and `ep0`. It translates `ep0` events
to typed control/lifecycle records and passes the opened data-endpoint files to
the worker. It never reads or writes device-protocol payloads.

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
| FunctionFS mount, `ep0`, and opening endpoint capabilities | Supervisor |
| FunctionFS data endpoint I/O | Worker |
| Runtime USB control meaning and firmware dispatch | Worker |
| CTAP, CCID, Trezor, or YubiHSM endpoint traffic | Worker |
| Private keys, wallet state, policy, display, buttons | Worker |

The disk profile contains no USB descriptors. The worker publishes one complete
typed configuration at startup and may publish a replacement later.

## Plain device workers

A `mode = "device"` profile uses the same root-owned launch policy and
credential drop. Its one character device, with `fd = 3`, is opened as root
and inherited across exec. The parent closes its copy after launch. The worker
uses the device directly; it needs no USB controller or control protocol.
Driver setup and device protocol remain outside the supervisor. Device workers
run once, with restart policy supplied by systemd; SIGHUP stops the worker and
reloads the root-owned profile. The USB lifecycle below applies to USB mode.

## Worker-side USB description

The shared Rust crate defines `UsbPersonality`, one
`UsbPersonalityBuilder`, Serde CBOR encoding, and a generic discovery parser:

```text
discover(max_speed, control_transfer_callback) -> UsbPersonality
```

The parser directly performs the discovery a USB stack needs: device,
configuration, language and string descriptors, plus recognized Microsoft OS
1.0 and WebUSB capabilities. It feeds every discovered result into the same
builder used by native workers. Its result is a semantic configuration object,
not a list of cached setup-packet responses.

The two construction paths are therefore:

- `discover(control_transfer_callback) -> UsbPersonality`, where genuine
  firmware answers the descriptor requests; and
- incremental native construction, where device and interface configuration
  calls populate `UsbPersonalityBuilder` before `finish()`.

The C ABI exposes an opaque Rust-owned builder for the second path. A static C
worker can populate it from constant data; it does not need a second C
personality model or a bespoke one-shot descriptor-bundle format. Both paths
share builder validation and CBOR serialization. The schema is the internal
protocol contract; the project does not promise byte-for-byte deterministic
CBOR for embedded blobs.

## Supervisor projection

Before changing the serving gadget, the supervisor decodes and validates the
entire candidate. It extracts device identity, configuration attributes,
endpoint topology, strings, Microsoft compatible IDs/properties, and WebUSB
metadata. It then builds:

- ConfigFS device/configuration attributes and strings;
- ConfigFS Microsoft OS 1.0 and WebUSB attributes;
- a FunctionFS v2 descriptor blob and string table; and
- the ordered endpoint map used when transferring FunctionFS FDs to the worker.

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
       |
       +-- empty --> ready without USB -> await nonempty Configure
       |
       +-- nonempty --> decode and validate CBOR -> Build generation 1
  create ConfigFS + FunctionFS
  publish descriptors
  open FunctionFS data endpoints
  send endpoint capabilities
       |
       v
Await Serving(1, request)
  bind UDC
       |
       v
Serving
  worker may send a replacement or empty Configure
       |
       +-- invalid --> reject; generation continues serving
       |
       +-- valid --> unbind -> Quiesce/remove -> build N+1 -> dwell -> bind
       |
       +-- empty --> unbind -> Quiesce/remove -> await nonempty Configure
```

The worker survives a valid USB reconfiguration. It closes the old generation
only after `Quiesce` and receives new FDs after the supervisor has rebuilt the
kernel objects. This is the software equivalent of firmware-driven disconnect,
personality change, and host re-enumeration.
Every replacement waits until the UDC has been detached for at least 250 ms
before binding the next generation. Initial attachment has no artificial
delay. The common bind boundary enforces this for live reconfiguration,
SIGHUP, and worker recovery without stacking path-specific sleeps.

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

The worker receives only the already-opened, direction-specific FunctionFS data
endpoints. The supervisor retains `ep0`, the mount, and all ConfigFS authority,
but stays out of the packet path. The worker cannot open adjacent endpoints or
reconfigure the gadget; its endpoint FDs are precise capabilities.

## Idle behavior

The control socket and GPIO event handles are pollable. A native worker may use
the FunctionFS files directly from its own endpoint threads. A firmware worker
can bridge each blocking FunctionFS file to a local packet-preserving queue,
leaving its virtual controller and firmware loop single-threaded and pollable.
This reproduces the interrupt-or-timer wait that prevents a busy loop on real
hardware while still servicing automatic lock, animations, retries, and other
firmware deadlines.

Host suspend is not a generation boundary. The worker, firmware state,
configuration, and endpoint files survive `SUSPEND`/`RESUME`; a reset-like
`DISABLE`/`ENABLE` sequence resets and reconfigures only the virtual USB
controller. The complete mapping is in [USB lifecycle](usb-lifecycle.md).

## UDC and Raspberry Pi

The supervisor discovers controllers through `/sys/class/udc`, sorts them, and
selects the first unless the root-owned profile specifies an exact `udc` name. One UDC exposes one
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
