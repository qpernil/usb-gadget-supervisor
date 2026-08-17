# Worker Protocol

## Goals

The worker protocol coordinates gadget lifecycle without carrying ordinary USB
payloads. It needs to be small enough for C and Rust workers, deterministic at
startup, and capable of failing closed.

The transport is an inherited local `AF_UNIX` `SOCK_SEQPACKET` descriptor. It is
not stdin or stdout: those remain available for normal process behavior and
diagnostics. Packet boundaries make the protocol unambiguous without a streaming
decoder.

Revision 1 is implemented by both `usb-gadget-supervisor` and
`virtual-yubikey-worker`. Its semantics remain alpha until the Trezor worker has
also exercised it, but its byte fixture is explicit and tested in both
repositories.

## Revision 1 encoding

Every `SOCK_SEQPACKET` record is exactly eight bytes:

| Offset | Size | Meaning |
| --- | --- | --- |
| 0 | 4 | ASCII magic `UGSP` |
| 4 | 1 | Protocol version, currently `1` |
| 5 | 1 | Message type |
| 6 | 2 | Big-endian flags, currently zero |

Message type values are `0x01`–`0x04` for supervisor messages and
`0x81`–`0x84` for worker messages, in the order shown below. Unknown versions,
types, flags, truncated records, and EOF fail closed.

## Startup

1. The supervisor validates the profile, creates the unbound ConfigFS gadget,
   mounts FunctionFS, and prepares the runtime directory.
2. It opens every available profile-declared local character device, creates a
   `SOCK_SEQPACKET` pair, and starts the worker with those descriptors inherited.
   Descriptor numbers are supplied through the environment.
3. Before `exec`, the supervisor clears supplementary groups, sets the target
   GID and UID, enables `PR_SET_NO_NEW_PRIVS`, and installs a parent-death
   signal.
4. The supervisor sends `RESOURCES_READY`. The worker opens FunctionFS `ep0`,
   writes its descriptors and strings, and opens the data endpoints required
   before attachment.
5. The worker sends `FUNCTIONFS_READY`.
6. The supervisor links profile functions in deterministic order, binds the UDC,
   and prepares post-bind nodes such as `/dev/hidgN`.
7. The supervisor sends `USB_ATTACHED`, optionally with file descriptors using
   `SCM_RIGHTS`.
8. The worker begins its endpoint service loops.

## Messages

### Supervisor to worker

| Message | Meaning |
| --- | --- |
| `RESOURCES_READY` | Pre-bind paths and inherited resources are available; optional when entirely supplied at exec |
| `USB_ATTACHED` | UDC is bound and post-bind resources are usable |
| `USB_DETACHED` | UDC has been unbound for reconnect or shutdown |
| `SHUTDOWN` | Stop accepting work, flush safe state, and exit promptly |

### Worker to supervisor

| Message | Meaning |
| --- | --- |
| `FUNCTIONFS_READY` | FunctionFS descriptors are published and required endpoints are open |
| `RECONNECT_REQUEST` | Perform a controlled UDC unbind/rebind cycle |
| `STOPPED` | Graceful shutdown is complete |
| `FATAL` | Worker cannot continue; supervisor must unbind and tear down |

The current message values are:

| Message | Value |
| --- | --- |
| `RESOURCES_READY` | `0x01` |
| `USB_ATTACHED` | `0x02` |
| `USB_DETACHED` | `0x03` |
| `SHUTDOWN` | `0x04` |
| `FUNCTIONFS_READY` | `0x81` |
| `RECONNECT_REQUEST` | `0x82` |
| `STOPPED` | `0x83` |
| `FATAL` | `0x84` |

## Inherited environment

The supervisor clears the worker environment, then supplies only its resource
contract:

| Variable | Meaning |
| --- | --- |
| `USB_GADGET_CONTROL_FD` | Decimal inherited control descriptor |
| `USB_GADGET_STATE_DIRECTORY` | Profile-owned persistent state directory |
| `USB_GADGET_RUNTIME_DIRECTORY` | Volatile worker runtime directory |
| `USB_GADGET_FUNCTIONFS_<NAME>` | Named FunctionFS mount path |
| `USB_GADGET_HID_<NAME>` | Named ConfigFS HID device path |
| `USB_GADGET_RESOURCE_<NAME>_FD` | Decimal inherited descriptor for a profile-declared local character device |

Function and resource names are uppercased and non-alphanumeric characters
become `_`. For example, `display-i2c` becomes
`USB_GADGET_RESOURCE_DISPLAY_I2C_FD`. An unavailable optional resource has no
environment variable. A C worker can consume the same environment and fixed
structure without a Rust dependency.

The supervisor opens declared resources before `setgroups`, `setgid`, and
`setuid`, then clears `FD_CLOEXEC` only for the approved descriptors. Device
nodes may therefore remain root-only and the worker needs no supplementary
`i2c` or `gpio` group. The supervisor neither issues I2C/GPIO ioctls nor knows
display addresses, GPIO offsets, button meanings, or framebuffer formats.

Descriptor confinement is at device-node granularity. An inherited I2C bus
descriptor may address other devices on that bus, and an inherited GPIO-chip
descriptor may request other lines on that chip. Profiles should expose the
narrowest suitable device nodes, and workers must still constrain addresses
and lines internally.

## Data path

USB traffic does not use the control channel:

```text
host OUT transfer -> FunctionFS endpoint file -> worker
host IN transfer  <- FunctionFS endpoint file <- worker
```

ConfigFS HID functions may expose `/dev/hidgN` only after UDC bind. A future
revision can open the node and pass the descriptor with `USB_ATTACHED`, avoiding
global `chown` and device-number races. Revision 1 retains the migration path:
the profile declares `/dev/hidgN`, the supervisor changes that node's ownership
after bind, and the worker opens the inherited path after `USB_ATTACHED`.

## Reconnect

Some firmware APIs expose a logical USB reconnect. The worker sends
`RECONNECT_REQUEST`; it never writes the UDC attribute itself.

The supervisor:

1. Unbinds the UDC.
2. Sends `USB_DETACHED`.
3. Waits for endpoints to settle if required by the kernel driver.
4. Rebinds the same validated gadget.
5. Sends `USB_ATTACHED` with any replaced descriptors.

Changing to a different device profile is not a reconnect message. It is a full
worker stop and gadget reconstruction.

## Failure behavior

- Startup has a bounded readiness timeout.
- EOF, malformed messages, or worker exit while attached trigger immediate UDC
  unbind.
- The supervisor does not blindly restart a worker while stale endpoints remain
  bound.
- Cleanup runs in reverse ownership order and preserves the first material
  error for diagnostics.
- Sensitive USB payloads are never included in supervisor logs.
- Worker trace logging remains a device-project policy and may expose secrets;
  it is disabled by default.
