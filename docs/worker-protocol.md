# Worker protocol

## Transport

The supervisor creates an `AF_UNIX` `SOCK_SEQPACKET` pair and duplicates the
worker end onto file descriptor 3. Packet boundaries preserve records and
`SCM_RIGHTS` carries local-resource and FunctionFS endpoint capabilities.

Every record starts with this 20-byte header:

| Offset | Size | Meaning |
| ---: | ---: | --- |
| 0 | 4 | ASCII `UGSP` (`USB Gadget Supervisor Protocol`) |
| 4 | 1 | version `1` |
| 5 | 1 | message kind |
| 6 | 2 | big-endian attached-FD count |
| 8 | 4 | big-endian USB generation |
| 12 | 4 | big-endian request ID |
| 16 | 4 | big-endian body length |

The body immediately follows. Its maximum length is 1 MiB. Wrong lengths,
unknown kinds, truncated ancillary data, and mismatched FD counts fail closed.

## Messages

| Direction | Kind | Value | Body / descriptors |
| --- | --- | ---: | --- |
| supervisor → worker | `InitialResources` | `0x01` | named local resources + matching FDs |
| supervisor → worker | `UsbEndpoints` | `0x02` | endpoint map + one FunctionFS FD per entry |
| supervisor → worker | `UsbBusEvent` | `0x03` | lifecycle event + endpoint activation |
| supervisor → worker | `UsbControlRequest` | `0x04` | setup packet + optional OUT data |
| supervisor → worker | `Quiesce` | `0x11` | empty |
| supervisor → worker | `ConfigurationRejected` | `0x12` | diagnostic UTF-8 text |
| worker → supervisor | `Configure` | `0x80` | schema-1 `UsbPersonality` CBOR |
| worker → supervisor | `UsbControlResponse` | `0x81` | disposition + optional IN data |
| worker → supervisor | `Serving` | `0x82` | empty |
| worker → supervisor | `Quiesced` | `0x84` | empty |

Worker configuration requests and supervisor control requests use nonzero
request IDs. Replies carry the matching ID. The generation is zero before the
first accepted configuration of a worker process and increments whenever the
supervisor creates replacement USB state.

## Initial resources

`InitialResources` uses generation and request ID zero. Its body is:

```text
u16 name_count
repeat name_count times:
    u16 UTF-8 name length
    bytes name
```

The record carries exactly one FD per name in profile order. A GPIO entry
contributes its line-request FD, not its GPIO-chip FD.

## USB personality

The `Configure` body is CBOR produced from the typed `UsbPersonality` in the
shared `usb-gadget-worker` crate. It represents the USB-facing configuration
that formerly lived in the installed text profile; it is not a transcript of
control requests.

The public C surface is intentionally small:

```c
bool ugsp_discover_usb_personality(
    uint8_t speed, ugsp_control_transfer_fn transfer, void *context,
    uint8_t **output, size_t *output_length);

void ugsp_personality_cbor_free(uint8_t *bytes, size_t length);
```

Discovery issues standard, Microsoft OS, and WebUSB control transfers through
the callback, parses the answers, and returns the final CBOR object. A native
worker may instead construct the Rust object or retain a static CBOR blob.

## Data endpoints

`UsbEndpoints` contains this body and exactly one FunctionFS data-endpoint FD
per entry:

```text
u16 endpoint_count
repeat endpoint_count times:
    u8 endpoint_address
    u8 transfer_type
    u16 max_packet_size
```

The FDs are the actual blocking endpoint files opened from the supervisor-owned
FunctionFS mount. OUT endpoints are readable and IN endpoints are writable.
There is no supervisor data protocol, framing, buffering, acknowledgement, or
packet inspection. The worker uses the same `read` and `write` interface it
would have if it opened FunctionFS itself, without receiving path or ConfigFS
authority.

FunctionFS and the USB gadget core provide transfer boundaries, backpressure,
and cancellation. `Suspend` retains the files and pending endpoint state.
`Disable` makes outstanding endpoint operations complete or fail with the
kernel's unconfigured-device result; the same endpoint files are usable after
the next `Enable`. A worker may dedicate blocking helper threads to these FDs
and expose a pollable virtual-controller interface to single-threaded firmware.

## Control endpoint

The supervisor owns FunctionFS `ep0`. A `UsbControlRequest` body contains the
eight setup bytes followed by exactly `wLength` bytes for an OUT request, or no
data for an IN request. The response begins with one disposition byte:

| Value | Meaning | Remaining body |
| ---: | --- | --- |
| `0` | stall | empty |
| `1` | acknowledge an OUT request | empty |
| `2` | answer an IN request | response bytes, clipped to `wLength` |

If the kernel cancels a pending setup because a newer setup or lifecycle event
supersedes it, the supervisor discards only that transaction and continues
serving. Cancellation is not a worker or gadget failure.

## Bus lifecycle

`UsbBusEvent` has this body:

```text
u8 event
u64 big-endian endpoint_activation
```

The event is one of the seven stable values below. The supervisor
translates the FunctionFS event stream into this abstraction and remains the
only process that reads `ep0`.

| Value | Event | Worker meaning |
| ---: | --- | --- |
| `0` | `Bind` | controller function has been bound; reset controller state |
| `1` | `Unbind` | gadget generation is being removed |
| `2` | `Enable` | host selected a configuration/interface; endpoints may transfer |
| `3` | `Disable` | configuration disappeared; reset and stop endpoint use |
| `4` | `Setup` | represented by `UsbControlRequest`, never sent as `UsbBusEvent` |
| `5` | `Suspend` | retain configuration and state; invoke firmware suspend handling |
| `6` | `Resume` | resume the same configuration; invoke firmware resume handling |

Suspend does not close the endpoint files or change the endpoint activation or
USB generation. `Disable` cancels the current activation and `Enable` starts a
fresh one using the same FunctionFS descriptors.
See [USB lifecycle](usb-lifecycle.md) for host sleep,
reset, disconnect, and power-loss behavior.

## Startup and reconfiguration

Startup is:

1. Supervisor starts the unprivileged worker and sends `InitialResources`.
2. Worker publishes `Configure(generation=0, request_id>0)`.
3. Supervisor validates and logs the personality, builds ConfigFS/FunctionFS,
   and opens the FunctionFS data endpoints.
4. Supervisor sends `UsbEndpoints(generation=1)` with those endpoint FDs.
5. Worker installs them and returns `Serving`; supervisor binds the UDC.
6. Runtime bus and control traffic uses records; data traffic uses FunctionFS
   directly in the worker.

For `usbReconnect()` or a personality change, the same worker sends another
complete `Configure`. After validating it, the supervisor unbinds, requests
`Quiesce`, removes the old generation, builds the next generation, sends new
endpoint FDs, waits for `Serving`, and rebinds. The host sees a physical-style
disconnect and full re-enumeration while firmware state survives.

`SIGHUP` deliberately has the broader meaning: re-read the root-owned profile,
unbind and quiesce the current generation, close the control channel, fully
reap the worker before unmounting FunctionFS, and start a fresh worker. Worker
exit or control-channel EOF takes the same fresh-incarnation recovery path.
For an intentional replacement or service stop, the supervisor closes its
control-channel endpoint and waits for the worker's normal EOF-driven exit.
`SIGTERM` and then `SIGKILL` are bounded fallbacks only for a wedged worker.

## Data path

```text
host OUT -> UDC/kernel -> FunctionFS FD -> worker
host IN  <- UDC/kernel <- FunctionFS FD <- worker
```

The supervisor is absent from this path and cannot parse Trezor, CCID, CTAP, or
YubiHSM payloads.
