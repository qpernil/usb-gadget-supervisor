# Worker process harness

`crates/usb-gadget-harness` is a reusable Unix test library, separate from the
privileged Linux supervisor. It launches a caller-supplied worker `Command`,
installs its control channel at FD 3, and passes named resource handles through
the production `InitialResources` message. It does not parse deployment profiles
or implement CTAP, CCID, YubiHSM, cryptography, authentication, or device state.

The caller defines resources with `Resource::Sink`, `Resource::DigitalInput`,
or `Resource::File`; resource names have no meaning to the harness. The test
then accepts the worker's CBOR personality with `configure`, obtains the
discovered endpoint addresses, and sends opaque bytes with
`endpoint(address).send` / `receive`. `bus_event`, `control`, `quiesce`, and the
low-level `send` / `receive` methods exercise the normal worker control contract.
Fresh endpoint handles accompany each accepted generation. Alternate interface
settings other than zero are rejected.

`TestDirectory` supplies private state and runtime directories. Keep it alive
across launches to test persistence. `Harness` bounds control, endpoint, and
exit waits, captures worker output in a private log, and kills/reaps an unfinished
child when dropped, including during test failure. Supplied local-resource
capabilities and sink peers remain alive for the worker incarnation. Drop the
harness before its directory. Dropping `TestDirectory` removes its state and logs.

## Handle compatibility

The supervisor and harness share the control envelope and descriptor-ownership
code in `usb-gadget-worker::control`. Workers consume the same version-1 records.
Production uses Unix
`SOCK_SEQPACKET`; the harness uses that socket kind on Linux and `SOCK_DGRAM`
on macOS, preserving record boundaries and `SCM_RIGHTS` in both cases. Message
kinds, generation/request identifiers, resource tables, and personality encoding
are unchanged. Received descriptors are close-on-exec and owned on error paths.

`usb-gadget-worker-io` provides compatible file, GPIO and display APIs. Worker
call sites and protocol/lifecycle flow remain intact; platform gates, imports,
and dependencies select the portable implementation. Production Linux links
the real APIs directly. The device repositories' `worker-harness` build feature
selects fake implementations on Linux; macOS uses the compatible APIs.
There is no runtime backend selection:

- Real Linux APIs and fake APIs are separate build-selected implementations.
  The fake file/GPIO APIs consume the harness handle protocol; display calls
  have inert compatible implementations.
- Simulated handles carry a kind declaration. Endpoint records contain tagged
  opaque transfers or enable/disable events. Empty data represents a USB ZLP;
  partial file reads retain the rest of a transfer. Transfers are bounded to
  65,536 bytes. The harness sends cancellation on disable/unbind/quiesce, as the
  kernel does for FunctionFS, so worker lifecycle logic needs no special branch.
- Digital inputs start released. The test sends a framed zero or one for an
  explicit level. The adapter exposes the GPIO edge layout and shared current
  level to the existing button loop. Touch authorization still follows the real
  worker's presence policy; the harness never grants it automatically.
- Fake display APIs provide an inert renderer. The worker's indicator and
  presence scheduling still runs; production Linux uses the real display APIs.

Deployment builds omit `worker-harness`; `--all-features` includes the fake APIs.
Both platforms run the ordinary worker executable without a runtime harness flag.
The source changes in workers are platform gates, imports, and dependencies;
existing file/GPIO/display calls, protocol handlers, and lifecycle flow are shared
with production. Direct embedded core APIs are unchanged and do not depend on
this harness.

## Validation scope

Run the harness library tests from this workspace:

```sh
cargo test --locked --workspace
```

With sibling checkouts of the first-party dependencies, run the device scenarios
from each device repository:

```sh
cargo test --locked --features worker-harness --test worker_harness
```

The virtual YubiKey tests exercise HID framing, U2F/CTAP, CCID/APDU routing,
USB control requests, explicit touch, persisted credential handles, bus events,
ejection/reinsertion, configuration rejection, and orderly shutdown. Virtual
YubiHSM tests exercise real bulk frames, ZLPs, malformed requests, session
lifecycle, ejection/reinsertion, persisted device identity, configuration
rejection, stale generations, and shutdown. Their CI runs these process tests
on macOS and Linux.

The harness does not enumerate a USB device on the host. It does not qualify
ConfigFS, FunctionFS kernel cancellation, UDC timing, actual GPIO/display
hardware, host USB drivers, PC/SC, browser integration, or physical-device
compatibility. Those remain separate Linux and hardware qualification workflows.
