# Profile format

## Scope

The installed TOML profile describes only the privileged launch boundary: the
worker, its mode, optional USB controller and FunctionFS mount, and the local hardware resources the supervisor
must open before dropping privileges. USB identity and descriptors are runtime
state owned by the worker and are not duplicated in this file.

Profiles are root-owned TOML documents. Schema 1 rejects unknown fields and
does not accept the former `[usb]` or `[[functions]]` sections.

## Selecting a profile and mode

`--profile NAME` resolves `/opt/usb-gadget-supervisor/profiles/NAME.toml`.
An absolute path selects an explicit file; relative paths are rejected. At
launch the file must be a root-owned, non-symlink regular file without set-ID
bits or group/other write permission. `--check-profile` validates the schema
without root or hardware. Launch settings have no CLI overrides.

`mode = "usb"` is the default. `mode = "device"` launches an ordinary
executable with one character device explicitly mapped as `fd = 3`. Device
mode omits `udc`, `functionfs_mount`, and `worker.readiness_timeout_ms`, and skips USB setup and all worker control records. It retains the
same account checks, credential drop, environment clearing, and private
state/runtime directories. It needs neither a USB controller nor a worker
protocol implementation.

```toml
schema = 1
mode = "device"
name = "example-device"

[worker]
command = "/absolute/path/to/example-worker"
arguments = ["--inherited-device"]
run_as = "per"
state_directory = "/var/lib/example-device"
runtime_directory = "/run/example-device"

[[resources]]
type = "character-device"
name = "target"
path = "/dev/example-target"
access = "read-write"
fd = 3
```

The resource list and explicit `fd` fields can describe multiple descriptor
assignments. The current implementation validates exactly one character-device
resource with `fd = 3`; this is an implementation limit, not a different file
format. Extending descriptor mapping can preserve existing profiles.
The executable consumes that handle by convention; it does not read the
profile. There is no readiness handshake. Worker exit ends the run, reporting
failure for a nonzero exit; systemd supplies restart policy. Stop sends SIGTERM
with bounded time to exit before SIGKILL. SIGHUP stops the worker and reloads
the root-owned profile. Mode/name changes require a service restart. Device
profiles use separate locks and do not acquire the USB controller lock.
Driver loading is separate; profiles contain no privileged shell hooks.

In USB mode, optional top-level `udc = "fe980000.usb"` selects an exact entry
in `/sys/class/udc`. Omit it to select the first controller in sorted order.
UDC names cannot contain paths or whitespace. Reload validates and resolves the
replacement selection before stopping the existing worker and rebinding.

## USB example

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
descriptor 3 in USB mode. Device mode instead inherits the declared device
on FD 3. Persistent device state belongs under the state directory;
temporary worker files belong under the runtime directory.

## Local hardware resources

Every resource is mandatory and has a unique name. Resource names and open
descriptors are sent together in profile order in the initial control record
in USB mode, where `fd` must be omitted. Device mode sends no control record.

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

In USB mode, after receiving its local resources, the worker sends a `Configure` record
whose body is the schema-1 CBOR `UsbPersonality`. That object contains:

- maximum USB speed;
- the standard device descriptor;
- one complete configuration descriptor;
- the referenced USB string descriptors;
- an optional typed Microsoft OS 1.0 declaration; and
- an optional typed WebUSB declaration.

The shared `usb-gadget-worker` crate defines this object, its single
`UsbPersonalityBuilder`, and its Serde/CBOR encoding. A firmware-backed worker
can call the discovery parser with a control-transfer callback, which asks the
firmware for the same descriptors a real USB stack would request. A native
worker instead populates the same Rust-owned builder incrementally from its
USB configuration calls. A static C worker can populate that builder from
constant data without maintaining a separate serialized-bundle ABI.

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
