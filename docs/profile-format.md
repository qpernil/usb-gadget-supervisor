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
name = "display-i2c"
path = "/dev/i2c-1"
access = "read-write"

[[resources]]
name = "buttons-gpio"
path = "/dev/gpiochip0"
access = "read-write"

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

`[[resources]]` entries let the root supervisor open a character device before
starting the unprivileged worker. `access` is `read`, `write`, or `read-write`.
Every declared resource is required; missing or inaccessible hardware prevents
startup. This keeps the fixed descriptor layout unambiguous.

The supervisor verifies that each resource is a non-symlink character device
under `/dev`. It appends the open descriptors to `PREBIND_RESOURCES` in profile
order; all device-specific operations stay in the worker. A Virtual
YubiKey/Trezor OLED profile can therefore use root-only `/dev/i2c-1` and a
selected `/dev/gpiochipN` without putting either worker in broad hardware
groups.

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
  or string tables.
- Duplicate function names or mount paths.
- Duplicate resource names or device paths.
- Resource paths outside `/dev` or resources that are not character devices.
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
