# Raspberry Pi Validation

Run this checklist for each device profile on its target Pi kernel.

## Preflight

```sh
/opt/usb-gadget-supervisor/usb-gadget-supervisor --check-profile \
  --profile /opt/usb-gadget-supervisor/profiles/virtual-yubikey.toml
ls /sys/class/udc
sudo systemctl stop usb-gadget-supervisor@virtual-yubikey.service
```

Confirm that no `g_*` gadget module or unrelated ConfigFS gadget owns the
controller. Preserve the device state directory unless the test explicitly
requires a factory reset.

## Start and enumerate

```sh
sudo systemctl start usb-gadget-supervisor@virtual-yubikey.service
systemctl --no-pager --full status \
  usb-gadget-supervisor@virtual-yubikey.service
journalctl -u usb-gadget-supervisor@virtual-yubikey.service -b --no-pager
cat /sys/class/udc/*/state
mount | grep ffs-virtual-yubikey
stat /dev/hidg0
```

With a data-capable cable attached, the selected UDC should reach
`configured`. For Virtual YubiKey, confirm full-speed `1050:0406`, product
`Virtual Yubico YubiKey FIDO+CCID`, no USB serial string, FIDO HID as
interface 0, and
CCID as interface 1. Capture `lsusb -v` and verify the device, configuration,
interface, endpoint, CCID, and HID descriptors against the profile.

Exercise host-level FIDO registration/assertion, Management, and PIV operations.
For Virtual Trezor, exercise enumeration and wallet commands with `trezorctl`
and Trezor Suite.

## Resource boundary

Inspect the worker process and confirm:

- it runs as the configured non-root account;
- it has the control socket and expected USB endpoint/HID FDs;
- it has no FunctionFS or HID path environment variables;
- FunctionFS mounts and HID nodes have not been made worker-owned; and
- optional I2C/SPI/GPIO access exists only through configured inherited FDs.

## Incarnation recovery

While attached, send `SIGKILL` to only the worker. The same supervisor process
must:

1. detect worker exit or control EOF;
2. unbind the UDC promptly;
3. remove the old FunctionFS and ConfigFS objects;
4. start a fresh worker incarnation; and
5. re-enumerate without a systemd service restart.

Also verify:

- firmware `usbReconnect()` produces the same fresh-incarnation cycle;
- stopping the service produces a host-visible disconnect and final teardown;
- a second supervisor fails on the global UDC lifecycle lock;
- malformed descriptors and wrong FD counts fail before exposure; and
- repeated stop/start and worker-crash cycles leave no stale gadget tree or
  FunctionFS mount.
