# Profile Format

## Purpose

A profile is a declarative, root-owned description of one host-visible USB
device and its worker. Profiles keep device identities and descriptors in their
own projects while allowing the supervisor to remain protocol-neutral.

Revision 1 uses TOML because the documents are operator-readable and the schema
is strictly deserialized with unknown fields rejected.

## Draft structure

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
manufacturer = "Yubico"
product = "YubiKey FIDO+CCID"
# Omit serial to expose no USB iSerialNumber string.
max_power_ma = 30

[worker]
command = "/home/per/virtual-yubikey/target/release/virtual-yubikey-worker"
arguments = ["--serial", "12345678", "--log-level", "info"]
run_as = "per"
readiness_timeout_ms = 10000
state_directory = "/var/lib/virtual-yubikey"
runtime_directory = "/run/virtual-yubikey"

[[resources]]
name = "display-i2c"
path = "/dev/i2c-1"
access = "read-write"
optional = true

[[resources]]
name = "buttons-gpio"
path = "/dev/gpiochip0"
access = "read-write"
optional = true

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
```

Function order is significant because ConfigFS assigns interface numbers in
link order.

## Local hardware resources

`[[resources]]` entries let the root supervisor open a character device before
starting the unprivileged worker. `access` is `read`, `write`, or `read-write`.
If `optional` is false or omitted, missing or inaccessible hardware prevents
startup. If it is true, a missing node is logged and omitted from the worker
environment, allowing the same profile to run headlessly.

The supervisor verifies that each resource is a non-symlink character device
under `/dev`. It passes the open descriptor as
`USB_GADGET_RESOURCE_<NORMALIZED_NAME>_FD`; all device-specific operations stay
in the worker. A future Virtual YubiKey/Trezor OLED profile can therefore share
root-only `/dev/i2c-1` and a selected `/dev/gpiochipN` without putting either
worker in broad hardware groups.

## Trezor One sketch

The selected upstream Trezor release remains the source of truth for the exact
USB identity and descriptors; these illustrative values must be verified when a
release is pinned.

```toml
schema = 1
name = "trezor-one"

[usb]
vendor_id = 0x1209
product_id = 0x53c1
bcd_usb = 0x0210
bcd_device = 0x0100
max_speed = "full-speed"
device_class = 0
device_subclass = 0
device_protocol = 0
manufacturer = "SatoshiLabs"
product = "TREZOR"
# The initial schema supports an omitted or static USB serial string.
max_power_ma = 100

[worker]
command = "/home/per/virtual-trezor/build/trezor-one-pi"
run_as = "per"
readiness_timeout_ms = 10000
state_directory = "/var/lib/virtual-trezor"
runtime_directory = "/run/virtual-trezor"

[[functions]]
type = "functionfs"
name = "trezor"
mount = "/dev/ffs-virtual-trezor"

[[functions]]
type = "hid"
name = "u2f"
protocol = 0
subclass = 0
report_length = 64
# Use the exact descriptor bytes from the pinned Trezor firmware release.
report_descriptor_hex = """
06 d0 f1 09 01 a1 01 09 20 15 00 26 ff 00 75 08
95 40 81 02 09 21 15 00 26 ff 00 75 08 95 40 91 02
c0
"""
device = "/dev/hidg0"
```

Whether the main, debug, and U2F interfaces are all published by one FunctionFS
function or split between FunctionFS and ConfigFS HID will be decided by the
first hardware spike. The profile schema must support deterministic interface
ordering either way.

## Validation

The supervisor must reject profiles that violate any of these conditions:

- Unknown schema version or unknown fields.
- Empty, relative, or traversal-containing runtime paths.
- A profile or file-backed descriptor writable by the target worker.
- A worker command not owned by root or the target worker, with set-ID or world
  write bits, writable by a group other than the worker's primary group, or
  without an applicable execute bit.
- Invalid VID/PID, USB version, endpoint size, power, class, or protocol values.
- Duplicate function names or mount paths.
- Duplicate resource names, normalized environment keys, or device paths.
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
