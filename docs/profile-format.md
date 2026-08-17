# Profile Format

## Purpose

A profile is a declarative, root-owned description of one host-visible USB
device and its worker. Profiles keep device identities and descriptors in their
own projects while allowing the supervisor to remain protocol-neutral.

TOML is proposed for the first implementation because the documents are
operator-readable and the schema can be strictly deserialized with unknown
fields rejected.

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
serial = "none"
max_power_ma = 60

[worker]
command = "/usr/libexec/virtual-yubikey/virtual-yubikey-worker"
run_as = "virtual-devices"
readiness_timeout_ms = 10000
state_directory = "/var/lib/virtual-yubikey"
runtime_directory = "/run/virtual-yubikey"

[[functions]]
type = "hid"
name = "fido"
protocol = 0
subclass = 0
report_length = 64
report_descriptor = "/usr/share/virtual-yubikey/fido-hid-report.bin"

[[functions]]
type = "functionfs"
name = "ccid"
mount = "/dev/ffs-virtual-yubikey"
```

Function order is significant because ConfigFS assigns interface numbers in
link order.

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
serial = "worker"
max_power_ma = 100

[worker]
command = "/usr/libexec/virtual-trezor/trezor-one-pi"
run_as = "virtual-devices"
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
report_descriptor = "/usr/share/virtual-trezor/u2f-hid-report.bin"
optional = true
```

Whether the main, debug, and U2F interfaces are all published by one FunctionFS
function or split between FunctionFS and ConfigFS HID will be decided by the
first hardware spike. The profile schema must support deterministic interface
ordering either way.

## Validation

The supervisor must reject profiles that violate any of these conditions:

- Unknown schema version or unknown fields.
- Empty, relative, or traversal-containing runtime paths.
- Worker command, profile, or descriptor files writable by the target worker.
- Invalid VID/PID, USB version, endpoint size, power, class, or protocol values.
- Duplicate function names or mount paths.
- FunctionFS mounts outside an approved `/dev/ffs-*` namespace.
- A root worker account.
- A profile that declares no functions.
- A requested UDC that is not present in `/sys/class/udc`.

The supervisor should resolve the selected worker UID and GID before creating
runtime paths. It must verify that existing state/runtime paths are real
directories rather than symlinks before changing ownership or permissions.

## Source of truth

Each device project installs its own profile and descriptor assets. Tests in
that project must compare the profile's advertised identity and capabilities
with the worker's logical device profile so USB metadata cannot drift from
implemented behavior.

The supervisor validates structure and safety; it does not decide whether a
particular YubiKey or Trezor identity is semantically correct.
