# Raspberry Pi Validation

This checklist is the remaining acceptance gate for the initial
`virtual-yubikey` extraction. Run it on the Pi after installing both projects as
described in the Virtual YubiKey README.

## Preflight

```sh
usb-gadget-supervisor --check-profile \
  --profile /etc/usb-gadget-supervisor/profiles/virtual-yubikey.toml
ls /sys/class/udc
sudo systemctl stop virtual-yubikey.service
```

Confirm that no legacy `g_*` gadget module or another ConfigFS gadget owns the
controller. Preserve `/var/lib/virtual-yubikey` when testing an upgrade so the
same FIDO and PIV state files are exercised.

## Start and enumerate

```sh
sudo systemctl start virtual-yubikey.service
systemctl --no-pager --full status virtual-yubikey.service
journalctl -u virtual-yubikey.service -b --no-pager
cat /sys/class/udc/*/state
mount | grep ffs-virtual-yubikey
stat /dev/hidg0
```

With a data-capable cable attached, the UDC should reach `configured`. On the
host, confirm full-speed `1050:0406`, product `YubiKey FIDO+CCID`, no USB serial
string, FIDO HID as interface 0, and CCID as interface 1. Save `lsusb -v` output
from before and after migration and compare the device, configuration,
interface, endpoint, CCID, and HID descriptors.

Exercise the same host-level FIDO registration/assertion, Management, and PIV
tests used before extraction. Existing files under `/var/lib/virtual-yubikey`
must load without migration or ownership errors.

## Failure containment

While attached, send `SIGKILL` to only the `virtual-yubikey-worker` process. The
supervisor must notice the exit, unbind the UDC promptly, clean FunctionFS and
ConfigFS, and let systemd restart a fresh instance. Also verify:

- stopping the service produces a host-visible disconnect;
- a second supervisor instance fails on the global lifecycle lock;
- an invalid or group-writable profile is rejected before gadget creation; and
- restart after an unclean supervisor exit removes only its known gadget tree.

Do not declare the migration released until descriptor comparison and these
failure tests pass on the target Pi kernel.

## Validation record: 2026-08-17

The extracted supervisor and worker were built and installed on
`raspberrypi-3`, an aarch64 Raspberry Pi running Debian with Raspberry Pi kernel
`6.18.39+rpt-rpi-v8`. The controller was `fe980000.usb`.

Passed:

- profile validation against the installed root-owned profile;
- root supervisor plus unprivileged `per:per` worker process ownership;
- FunctionFS mounted at `/dev/ffs-virtual-yubikey`;
- UDC state `configured` and ConfigFS identity `1050:0406`, BCD `0x0580`;
- macOS full-speed enumeration as `YubiKey FIDO+CCID`;
- loading the existing 1,073-byte FIDO and 2,732-byte PIV state files;
- host `ykman` Management and FIDO2 information queries by serial `12345678`;
- host CCID activation, PIV selection, and a full `yubico-piv-tool -a status`
  read of the preserved P-256, Ed25519, X25519, and RSA-2048 slots;
- rejection of a second supervisor by the global lifecycle lock; and
- `SIGKILL` of only the worker causing supervisor failure, cleanup, and a clean
  systemd restart after two seconds with unchanged state-file hashes.

Still required before release:

- capture and compare complete pre/post `lsusb -v` descriptors from a Linux
  host;
- complete a host-level FIDO registration/assertion with touch and cancellation;
- run the state-mutating Yubico PIV regression workflow; and
- test stop/restart and crash cleanup repeatedly with the future I2C/GPIO UI
  resources enabled.
