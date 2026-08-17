# Worker Protocol

## Goals

The worker protocol coordinates gadget lifecycle without carrying ordinary USB
payloads. It needs to be small enough for C and Rust workers, deterministic at
startup, and capable of failing closed.

The transport is an inherited local `AF_UNIX` `SOCK_SEQPACKET` descriptor. It is
not stdin or stdout: those remain available for normal process behavior and
diagnostics. Packet boundaries make the protocol unambiguous without a streaming
decoder.

This document defines semantic messages first. The byte-level encoding remains
unstable until the YubiKey extraction and Trezor worker have both exercised it.

## Startup

1. The supervisor validates the profile, creates the unbound ConfigFS gadget,
   mounts FunctionFS, and prepares the runtime directory.
2. It creates a `SOCK_SEQPACKET` pair and starts the worker with one endpoint
   inherited. The descriptor number is supplied through a dedicated environment
   variable or argument.
3. Before `exec`, the supervisor clears supplementary groups, sets the target
   GID and UID, enables `PR_SET_NO_NEW_PRIVS`, and installs a parent-death
   signal.
4. The worker opens FunctionFS `ep0`, writes its descriptors and strings, and
   opens the data endpoints required before attachment.
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

The protocol must include a version during its first encoded revision. Unknown
message types or incompatible versions terminate startup rather than being
silently ignored.

## Data path

USB traffic does not use the control channel:

```text
host OUT transfer -> FunctionFS endpoint file -> worker
host IN transfer  <- FunctionFS endpoint file <- worker
```

ConfigFS HID functions may expose `/dev/hidgN` only after UDC bind. The
supervisor can open the node and pass the descriptor with `USB_ATTACHED`, which
avoids global `chown`, device-number races, and worker access to unrelated HID
gadget nodes. The existing YubiKey implementation may initially retain its
current chown-and-open behavior during migration.

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
