# Worker Protocol

## Purpose

The worker protocol gives an unprivileged device worker an immutable set of already-open
USB resources. The worker never opens FunctionFS mounts, `/dev/hidgN`, ConfigFS,
or the UDC attribute. Normal USB payloads travel through the transferred file
descriptors, not through the control socket.

Supervisor, profile, and worker are deployed as one matched set.

## Transport and encoding

The supervisor creates a local `AF_UNIX` `SOCK_SEQPACKET` socket pair. It
duplicates the worker end onto fixed file descriptor 3 before `exec`. Packet boundaries remove
the need for a stream decoder, and `SCM_RIGHTS` ancillary data carries open file
descriptions independently of the eight-byte normal-data record.

| Offset | Size | Meaning |
| --- | --- | --- |
| 0 | 4 | ASCII magic `UGSP` |
| 4 | 1 | Protocol version `1` |
| 5 | 1 | Message type |
| 6 | 2 | Big-endian exact count of attached file descriptors |

The count is part of the validation contract. Truncated packets, ancillary
truncation, unexpected ancillary types, unknown messages, wrong counts, and EOF
during startup fail closed.

## Messages

| Direction | Message | Value | FD count |
| --- | --- | ---: | ---: |
| Supervisor → worker | `PREBIND_RESOURCES` | `0x01` | Profile-derived |
| Supervisor → worker | `POSTBIND_RESOURCES` | `0x02` | Profile-derived |
| Worker → supervisor | `PREPARED` | `0x81` | 0 |
| Worker → supervisor | `SERVING` | `0x82` | 0 |

There are no shutdown, detach, fatal, stopped, or reconnect messages. After
`SERVING`, the socket remains open only as a liveness relationship:

- supervisor EOF tells the worker to exit;
- worker EOF or process exit tells the supervisor to rebuild the incarnation;
- a firmware `usbReconnect()` operation ends the worker process, producing the
  same clean rebuild as any other worker exit.

## Fixed resource order

The magic/version selects one fixed layout. No per-descriptor identifiers or
nullable slots are encoded. Profile resources are therefore mandatory. A
future optional resource would need a separate explicitly versioned message
rather than changing the meaning of an existing slot.

`PREBIND_RESOURCES` contains, in profile function order:

1. for every FunctionFS function, its `ep0` descriptor;
2. immediately after it, `ep1` through `epN` in descriptor declaration order.
3. after all FunctionFS descriptors, every `[[resources]]` descriptor in
   profile order. A `gpio-lines` entry contributes the exact line-request
   descriptor, not its GPIO-chip descriptor.

The supervisor parses the FunctionFS v2 descriptor blob to derive `N`, endpoint
order, direction, and therefore the safe open mode. The worker and its installed
profile are one versioned device contract, so the worker knows the semantic
meaning of each position.

`POSTBIND_RESOURCES` contains one open HID gadget descriptor for every ConfigFS
HID function, again in profile order. HID nodes exist only after UDC binding,
which is why this second transfer is necessary.

Current layouts are:

| Worker | Pre-bind FDs | Post-bind FDs |
| --- | --- | --- |
| Virtual YubiKey | CCID `ep0`, bulk OUT, bulk IN, interrupt IN | FIDO HID |
| Virtual Trezor | main `ep0`, OUT, IN, display bus, display-control lines, button lines | none |

Profile-declared I2C/SPI devices and exact GPIO line groups are acquired by the
supervisor and transferred in the same pre-bind `SCM_RIGHTS` packet. GPIO line
order becomes value-bit order, and an input group with edge detection is itself
pollable. The worker receives authority to only these open resources and never
receives permission to open their paths or claim other GPIO lines.

## Startup sequence

1. The supervisor validates the root-owned schema-1 profile and FunctionFS
   blobs.
2. It creates the unbound ConfigFS gadget and root-only FunctionFS mounts.
3. It writes each function's descriptors and strings to `ep0`.
4. It opens every resulting endpoint with direction-appropriate access and
   every required local hardware resource with its declared access mode.
5. It starts the unprivileged worker and sends `PREBIND_RESOURCES` with
   `SCM_RIGHTS`.
6. The worker validates the exact layout, initializes state, and sends
   `PREPARED`.
7. The supervisor links functions and binds the selected UDC.
8. It opens post-bind HID nodes and sends `POSTBIND_RESOURCES` (including an
   explicit zero-FD packet when there are none).
9. The worker sends `SERVING` and begins its transport loops.

The worker retains FunctionFS `ep0` because it still receives runtime
`BIND`, `ENABLE`, `DISABLE`, `UNBIND`, `SUSPEND`, `RESUME`, and `SETUP` events.
It does not use `ep0` to publish descriptors; that setup operation is already
complete before the FD is transferred.

## Incarnations and cleanup

The supervisor process owns the long-lived service. A worker process is one
short-lived incarnation with an immutable resource bundle:

```text
prepare -> worker PREPARED -> bind -> worker SERVING -> serving
   ^                                                |
   +------ unbind, clean, create new process <------+ worker exit/EOF
```

On worker exit or control EOF, the supervisor unbinds first, closes its control
socket, waits briefly for worker termination, unmounts FunctionFS, removes the
ConfigFS gadget, and constructs a fresh incarnation. A systemd stop performs
the same incarnation cleanup and then ends the supervisor service. This uses
process creation as the complete reset boundary instead of trying to repair
endpoint state inside an old process.

`SIGHUP` transactionally rereads and validates the profile, then requests the
same clean incarnation rebuild while leaving the supervisor process running.
An invalid replacement is rejected without disturbing the serving
incarnation. Worker exit and control EOF continue to rebuild from the already
accepted in-memory profile.

## Bootstrap and environment

The control socket is always descriptor 3. The supervisor clears the
environment and supplies only the two ordinary path settings:

| Variable | Meaning |
| --- | --- |
| `USB_GADGET_STATE_DIRECTORY` | Persistent worker-owned state directory |
| `USB_GADGET_RUNTIME_DIRECTORY` | Volatile worker runtime directory |

There are no descriptor-number, FunctionFS, HID, or local-device path
environment variables.

## Data path

```text
host OUT -> UDC/kernel -> open OUT fd -> worker protocol decoder
host IN  <- UDC/kernel <- open IN fd  <- worker protocol encoder
```

The root supervisor is absent from this data path. It handles descriptor
metadata and lifecycle but never proxies CTAP, CCID, Trezor, or vendor-bulk
payloads.
