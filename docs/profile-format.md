# Profile Format

## Purpose

A profile is a declarative, root-owned description of one host-visible USB
device and its worker. Profiles keep device identities and descriptors in their
own projects while allowing the supervisor to remain protocol-neutral.

Profiles use TOML because the documents are operator-readable and the schema
is strictly deserialized with unknown fields rejected.

## Example

```toml
schema = 1
name = "yubikey-5-fido-ccid"

[usb]
vendor_id = 0x1050
product_id = 0x0406
bcd_usb = 0x0200
bcd_device = 0x0580
max_speed = "full-speed"
device_class = 0
device_subclass = 0
device_protocol = 0
manufacturer = "Virtual USB Gadget"
product = "Virtual Yubico YubiKey FIDO+CCID"
# Omit serial to expose no USB iSerialNumber string.
max_power_ma = 30

[worker]
command = "/absolute/path/to/virtual-yubikey-worker"
arguments = ["--serial", "12345678", "--log-level", "info"]
run_as = "worker-account"
readiness_timeout_ms = 10000
state_directory = "/var/lib/virtual-yubikey"
runtime_directory = "/run/virtual-yubikey"

[[resources]]
type = "character-device"
name = "display-i2c"
path = "/dev/i2c-1"
access = "read-write"

[[resources]]
type = "gpio-lines"
name = "buttons"
path = "/dev/gpiochip0"
offsets = [5, 26, 13]
direction = "input"
active_low = true
bias = "pull-up"
edge = "both"

[[functions]]
type = "hid"
name = "fido"
protocol = 0
subclass = 0
report_length = 64
report_descriptor_hex = """
06 d0 f1 09 01 a1 01 09 20 15 00 26 ff 00 75 08
95 40 81 02 09 21 15 00 26 ff 00 75 08 95 40 91 02
c0
"""
device = "/dev/hidg0"

[[functions]]
type = "functionfs"
name = "ccid"
mount = "/dev/ffs-virtual-yubikey"
descriptors_hex = """
# Complete USB_FUNCTIONFS_DESCRIPTORS_MAGIC_V2 blob.
"""
strings_hex = """
# Complete USB_FUNCTIONFS_STRINGS_MAGIC blob.
"""
```

Function order is significant because ConfigFS assigns interface numbers in
link order.

For every FunctionFS entry, `descriptors_hex` and `strings_hex` are required.
They are the exact byte blobs the supervisor writes to `ep0`. The supervisor
parses them to validate their structure and derive the ordered endpoint FD
bundle; endpoint direction determines whether each node is opened for reading
or writing. Descriptor contents remain versioned with the device project even
though publication is a supervisor operation.

## Local hardware resources

`[[resources]]` entries let the root supervisor acquire local hardware before
starting the unprivileged worker. Every entry has an explicit `type`, and every
declared resource is required. Missing or inaccessible hardware prevents
startup, keeping the fixed descriptor layout unambiguous.

A `character-device` resource opens one non-symlink device under `/dev`.
`access` is `read`, `write`, or `read-write`. This is appropriate for I2C and
SPI bus descriptors.

A `gpio-lines` resource asks the Linux GPIO v2 API for exclusive ownership of
an ordered group of 1 to 64 offsets on one GPIO chip. `direction` is `input` or
`output`. Input groups may set `active_low`, `bias` (`pull-up`, `pull-down`, or
`disabled`), and `edge` (`rising`, `falling`, or `both`). Output groups require
one boolean `initial_values` item per offset so all lines enter an explicit
safe state as the group is claimed. Output groups cannot set input bias or edge
detection.

The order of `offsets` becomes the bit order of GPIO value operations: offset
zero is bit zero, and so on. Multiple disjoint groups may use the same GPIO
chip, but the profile validator rejects a line claimed by more than one group.
The supervisor passes the returned line-request handle—not the GPIO-chip
handle—to the worker. Consequently the worker can read, write, and poll only
the exact lines it inherited and cannot request additional lines.

All acquired descriptors are appended to `PREBIND_RESOURCES` in profile order.
A Virtual Trezor profile can therefore receive one display-bus descriptor, one
display-control output handle, and one pollable button input handle without
requiring broad I2C, SPI, or GPIO group membership.

## Trezor One example

Virtual Trezor uses the same schema with one FunctionFS vendor interface. The
device repository owns the complete descriptor and string blobs; this excerpt
shows the profile shape without duplicating those byte tables.

```toml
schema = 1
name = "virtual-trezor"

[usb]
vendor_id = 0x1209
product_id = 0x53c1
bcd_usb = 0x0210
bcd_device = 0x0100
max_speed = "full-speed"
device_class = 0
device_subclass = 0
device_protocol = 0
manufacturer = "Virtual Trezor"
product = "Virtual Trezor"
serial = "virtual-trezor-one"
max_power_ma = 100

[usb.microsoft_os_1]
vendor_code = 0x21
signature = "MSFT100"

[usb.webusb]
version = 0x0100
vendor_code = 0x01
landing_page = ""

[worker]
command = "/absolute/path/to/virtual-trezor-worker"
run_as = "worker-account"
readiness_timeout_ms = 30000
state_directory = "/var/lib/virtual-trezor"
runtime_directory = "/run/virtual-trezor"

[[functions]]
type = "functionfs"
name = "trezor"
mount = "/dev/ffs-virtual-trezor"
descriptors_hex = """
# Complete v2 FunctionFS descriptor blob from the device implementation.
"""
strings_hex = """
# Complete FunctionFS string blob from the device implementation.
"""
```

The current Virtual Trezor profile exposes only the main vendor interface.
DebugLink and the separate U2F HID interface are not present. Profile order
remains the deterministic interface order whenever more than one function is
declared.

`usb.microsoft_os_1` enables the Microsoft OS 1.0 string in ConfigFS. A
FunctionFS blob in the same profile must set `FUNCTIONFS_HAS_MS_OS_DESC` and
carry at least one compatible-ID or extended-properties descriptor. The
supervisor requires both halves together, validates the referenced interface
numbers, and links configuration `c.1` into the gadget's `os_desc` group. The
Trezor profile uses compatible ID `WINUSB` for interface zero and publishes
the upstream Trezor `DeviceInterfaceGUIDs` value, allowing Windows to bind its
inbox WinUSB driver without changing the interface from vendor-specific USB.

`usb.webusb` enables the ConfigFS WebUSB BOS platform capability. Version
`0x0100` and a nonzero vendor request code are required; `landing_page` may be
empty, as it is for the current Trezor profile, or an ASCII HTTP(S) URL. WebUSB
is browser discovery/access metadata and does not replace a Windows driver.
Profiles using it need `usb.bcd_usb` of at least `0x0201` so hosts request the
BOS descriptor. Startup fails clearly if the running kernel lacks ConfigFS
WebUSB support.

## Validation

The supervisor must reject profiles that violate any of these conditions:

- Unknown schema version or unknown fields.
- Empty, relative, or traversal-containing runtime paths.
- A profile or file-backed descriptor writable by the target worker.
- A worker command not owned by root or the target worker, with set-ID or world
  write bits, writable by a group other than the worker's primary group, or
  without an applicable execute bit.
- Invalid VID/PID, USB version, endpoint size, power, class, or protocol values.
- Invalid FunctionFS v2 headers, counts, descriptor lengths, endpoint topology,
  Microsoft OS feature descriptors, or string tables.
- Microsoft OS ConfigFS settings without matching FunctionFS OS descriptors,
  or FunctionFS OS descriptors without the global settings.
- Duplicate function names or mount paths.
- Duplicate resource names, duplicate character-device paths, duplicate GPIO
  offsets within a group, or overlapping GPIO claims on one chip.
- Resource paths outside `/dev`, resources that are not character devices, or
  invalid GPIO direction/bias/edge/initial-value combinations.
- FunctionFS mounts outside an approved `/dev/ffs-*` namespace.
- A root worker account.
- A profile that declares no functions.
- A requested UDC that is not present in `/sys/class/udc`.

The supervisor should resolve the selected worker UID and GID before creating
runtime paths. It must verify that existing state/runtime paths are real
directories rather than symlinks before changing ownership or permissions.

## Source of truth

Each device project installs one profile into
`/opt/usb-gadget-supervisor/profiles`. The profile should contain HID report
descriptor bytes inline with `report_descriptor_hex`, so no separately
installed descriptor asset is needed. `report_descriptor` remains available
for an absolute, root-owned file when an inline descriptor is impractical;
exactly one of the two keys is required for each HID function.

Tests in the device project must compare the profile's advertised identity and
capabilities with the worker's logical device profile so USB metadata cannot
drift from implemented behavior.

HID descriptors contain whitespace-separated hexadecimal bytes. The supervisor
decodes them before writing ConfigFS `report_desc`.

The supervisor validates structure and safety; it does not decide whether a
particular YubiKey or Trezor identity is semantically correct.
