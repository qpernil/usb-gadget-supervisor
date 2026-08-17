# Migration from Virtual YubiKey

## Existing boundary

`virtual-yubikey` already contains a root supervisor and an unprivileged worker
as separate processes. The root process re-executes the same binary with an
internal worker descriptor after permanently dropping UID/GID and supplementary
groups.

The extraction changes packaging and ownership more than fundamental behavior.
The first requirement is that the existing YubiKey continues to enumerate and
behave identically throughout the move.

## Source mapping

| Current source | Destination |
| --- | --- |
| `src/gadget.rs` ConfigFS, mount, UDC, lock, cleanup, child supervision | `usb-gadget-supervisor` |
| Supervisor branch in `src/main.rs` | `usb-gadget-supervisor` binary |
| Worker branch in `src/main.rs` | `virtual-yubikey-worker` binary |
| `src/functionfs.rs` | Remains in `virtual-yubikey` |
| `src/usb_identity.rs` | Becomes YubiKey profile/descriptor assets and worker consistency tests |
| `src/ctaphid.rs`, `src/ccid.rs`, `src/smartcard.rs` | Remain in `virtual-yubikey` |
| `virtual-yubikey-core`, `virtual-yubikey-crypto` | Remain in `virtual-yubikey` |
| Root systemd service | Starts the generic supervisor with a YubiKey profile |

## Phases

### 1. Freeze current behavior

- Record current device, configuration, interface, endpoint, HID report, and
  FunctionFS descriptor bytes in tests.
- Record startup, Ctrl-C cleanup, crash cleanup, stale-state recovery, and
  exclusive-lock behavior.
- Keep the current binary as the reference during extraction.

### 2. Introduce a profile inside the current repository

- Replace hard-coded gadget constants with a typed in-memory YubiKey profile.
- Preserve the exact current interface ordering: FIDO HID first, CCID second.
- Keep the existing self-exec worker temporarily.

This validates the profile model without a cross-repository protocol change.

### 3. Extract the supervisor

- Move generic ConfigFS, FunctionFS mounting, UDC selection, privilege drop,
  readiness, lifecycle, and cleanup code to this repository.
- Install a root-owned YubiKey profile with the existing descriptor assets.
- Start the existing YubiKey worker as an external command.
- Replace the current stream readiness byte with the versioned worker protocol
  while preserving startup ordering.

### 4. Make the YubiKey worker a dedicated binary

- Remove public root-supervisor options from `virtual-yubikey-worker`.
- Refuse UID 0.
- Accept only the inherited control descriptor and validated resource paths or
  descriptors.
- Keep all state, USB protocol, touch IPC, and diagnostics in the YubiKey
  project.

### 5. Add the Trezor One worker

- Install a separate Trezor profile and worker executable.
- Exercise direct FunctionFS endpoints, worker-requested USB reconnect, I2C
  display, and GPIO buttons.
- Use differences between YubiKey and Trezor to revise the profile and worker
  protocol before declaring either stable.

### 6. Add YubiHSM only after the contracts settle

The YubiHSM worker should consume the established profile and lifecycle
contracts. Its object, session, capability, audit, and cryptographic behavior
belongs entirely outside the supervisor.

## Acceptance criteria

- The supervisor contains no YubiKey, Trezor, YubiHSM, CTAP, CCID, APDU,
  protobuf, wallet, or cryptographic implementation.
- A worker never needs root and cannot write ConfigFS or the UDC attribute.
- Endpoint traffic travels directly between FunctionFS/HID files and the worker.
- Worker exit while attached unbinds the gadget promptly.
- Ctrl-C and systemd stop remove only resources owned by the active instance.
- Stale cleanup remains conservative and protected by an exclusive lock.
- YubiKey USB descriptors and host behavior are byte-for-byte or semantically
  equivalent to the pre-extraction implementation.
- Switching profiles produces a real disconnect/reconnect and never combines
  unrelated identities into one composite device.
- The initial cross-project protocol remains explicitly unstable until both the
  YubiKey and Trezor workers pass integration tests.
