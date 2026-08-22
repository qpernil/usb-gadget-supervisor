# Profile format

## Scope

The installed TOML profile describes only the privileged launch boundary: the
worker, its FunctionFS mount, and the local hardware resources the supervisor
must open before dropping privileges. USB identity and descriptors are runtime
state owned by the worker and are not duplicated in this file.

Profiles are root-owned TOML documents. Schema 1 rejects unknown fields and
does not accept the former `[usb]` or `[[functions]]` sections.

## Example

```toml
schema = 1
name = "virtual-trezor-st7789"
functionfs_mount = "/dev/ffs-virtual-trezor-st7789"

[worker]
command = "/opt/virtual-trezor/bin/virtual-trezor-worker"
arguments = ["--display=st7789-spi"]
run_as = "virtual-trezor"
readiness_timeout_ms = 30000
state_directory = "/var/lib/virtual-trezor"
runtime_directory = "/run/virtual-trezor"

[[resources]]
type = "character-device"
name = "display-spi"
path = "/dev/spidev0.0"
access = "read-write"

[[resources]]
type = "gpio-lines"
name = "display-control"
path = "/dev/gpiochip0"
offsets = [25, 27, 24]
direction = "output"
initial_values = [false, true, false]

[[resources]]
type = "gpio-lines"
name = "buttons"
path = "/dev/gpiochip0"
offsets = [5, 26, 13]
direction = "input"
active_low = true
bias = "pull-up"
edge = "both"
```

`name` is both the ConfigFS gadget name and the FunctionFS function name.
`functionfs_mount` must be an absolute `/dev/ffs-*` path. The supervisor owns
that mount; the worker receives only the already-opened data endpoint files and
never opens the path itself.

## Worker

The worker command must be absolute and safe for execution under `run_as`.
The account must not be root. `readiness_timeout_ms` bounds every startup and
USB-reconfiguration handshake, not ordinary USB traffic.

The supervisor clears the inherited environment, supplies `STATE_DIRECTORY`
and `RUNTIME_DIRECTORY`, and places its `SOCK_SEQPACKET` control socket on file
descriptor 3. Persistent device state belongs under the state directory;
temporary worker files belong under the runtime directory.

## Local hardware resources

Every resource is mandatory and has a unique name. Resource names and open
descriptors are sent together in profile order in the initial control record.

A `character-device` resource opens one non-symlink device under `/dev` with
`access` set to `read`, `write`, or `read-write`. This is suitable for I2C and
SPI device nodes.

A `gpio-lines` resource asks the Linux GPIO v2 API for exclusive ownership of
an ordered group of 1 to 64 offsets. Input groups may set `active_low`, `bias`
(`pull-up`, `pull-down`, or `disabled`), and `edge` (`rising`, `falling`, or
`both`). Output groups require one boolean `initial_values` entry per offset
and cannot specify input bias or edge detection.

Offset order becomes GPIO value-bit order. The supervisor passes the returned
line-request handle, not the GPIO-chip descriptor, so the worker cannot claim
additional lines. Overlapping claims on one chip are rejected.

## USB configuration

After receiving its local resources, the worker sends a `Configure` record
whose body is the schema-1 CBOR `UsbPersonality`. That object contains:

- maximum USB speed;
- the standard device descriptor;
- one complete configuration descriptor;
- the referenced USB string descriptors;
- an optional typed Microsoft OS 1.0 declaration; and
- an optional typed WebUSB declaration.

The shared `usb-gadget-worker` crate defines this object and its Serde/CBOR
encoding. A native Rust worker can construct it directly. A static worker can
embed a previously generated CBOR byte string. A firmware-backed worker can
call the shared discovery parser with a control-transfer callback, which asks
the firmware for the same descriptors a real USB stack would request.

The supervisor decodes and logs the object, validates it, derives ConfigFS and
FunctionFS state, and saves the exact accepted CBOR at
`/run/usb-gadget-supervisor/<profile>.cbor` with root-only permissions.

## Validation

The supervisor rejects:

- unknown schema versions or fields;
- relative, empty, or traversal-containing paths;
- unsafe worker executable ownership or mode;
- a root worker account;
- duplicate resources, device paths, or GPIO claims;
- resources outside `/dev` or resources of the wrong type;
- invalid GPIO direction, bias, edge, or initial-value combinations; and
- FunctionFS mount paths outside `/dev/ffs-*`.

USB configuration is validated separately when the worker publishes it. An
invalid replacement receives `ConfigurationRejected` and leaves the serving
USB generation untouched.
