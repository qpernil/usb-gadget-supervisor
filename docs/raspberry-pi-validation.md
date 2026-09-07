# Raspberry Pi Validation

This checklist is tested on both 64-bit Ubuntu and 64-bit Raspberry Pi OS.
It does not depend on a named deployment host or login account.

The USB checklist below applies to each USB profile on its target Pi kernel.
For `mode = "device"`, use the device-profile checks at the end.

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
```

With a data-capable cable attached, the selected UDC should reach
`configured`. For Virtual YubiKey, confirm full-speed `1050:0406`, product
`Virtual Yubico YubiKey FIDO+CCID`, no USB serial string, FIDO HID as
interface 0, and CCID as interface 1.
Capture `lsusb -v` and verify the device, configuration, interface, endpoint,
CCID, and HID descriptors against the worker's accepted CBOR personality in
`/run/usb-gadget-supervisor`.

Exercise host-level FIDO registration/assertion, Management, and PIV operations.
For Virtual Trezor, exercise enumeration and wallet commands with `trezorctl`
and Trezor Suite.

## Resource boundary

Inspect the worker process and confirm:

- it runs as the configured non-root account;
- it has the control socket and expected direction-specific FunctionFS/HID FDs;
- it uses fixed descriptor 3 for control and has no descriptor-number or USB
  path environment variables;
- FunctionFS mounts and HID nodes have not been made worker-owned; and
- required I2C/SPI/GPIO access exists only through the configured pre-bind FDs.

## Reconfiguration and process recovery

While attached, send `SIGKILL` to only the worker. The same supervisor process
must:

1. detect worker exit or control EOF;
2. unbind the UDC promptly;
3. remove the old FunctionFS and ConfigFS objects;
4. start a fresh worker incarnation; and
5. re-enumerate without a systemd service restart.

Also verify:

- an initial empty `Configure` leaves the healthy worker running indefinitely
  with no gadget generation, and its first nonempty `Configure` attaches
  generation one;
- a valid worker reconfiguration quiesces the old endpoint generation,
  re-enumerates, and leaves the worker process alive;
- every replacement remains detached for at least 250 ms before the next UDC
  bind, while initial service startup has no artificial dwell;
- an empty `Configure` quiesces and removes the active generation without
  stopping the worker, remains detached indefinitely, and a later nonempty
  `Configure` binds without adding the replacement dwell;
- an invalid CBOR replacement receives `ConfigurationRejected` while the
  current USB generation continues serving;
- firmware `usbReconnect()` uses the live-worker reconfiguration path;
- stopping the service produces a host-visible disconnect and final teardown;
- a second supervisor fails on the global UDC lifecycle lock;
- malformed descriptors and wrong FD counts fail before exposure; and
- repeated stop/start and worker-crash cycles leave no stale gadget tree or
  FunctionFS mount.

## Host power management

With the Pi independently powered, suspend and resume the attached host. The
worker PID and USB generation must remain unchanged, endpoint traffic must
continue after wake, and the log must show `SUSPEND`/`RESUME` or the valid
reset-like `SUSPEND`/`DISABLE`/`ENABLE` sequence. Unplug/replug while the Pi
stays powered and confirm `DISABLE` followed by fresh enumeration. If the host
also supplies the Pi's only power, repeat only after accepting that VBUS loss
will cold-boot the Pi rather than produce a software lifecycle event.

## Device profiles

The generic privileged integration test uses `/dev/null` and requires no USB
or I2C hardware:

```sh
sudo python3 tests/device_mode.py target/release/usb-gadget-supervisor
```

It checks inherited FD 3, non-root credentials with no supplementary groups,
`no_new_privs`, private state directories, root-owned profile permissions,
symlink rejection, worker failure, graceful stop, and supervisor parent death.

The hardware launch path is also exercised with `virtual-yubihsm-i2c` on two
Pi 3B+ targets. Each worker runs as `per` and continues serving through FD 3
while `/dev/bsc-target0` is root-owned mode `0600`. SIGHUP replaces the worker;
stopping the external `target-driver` launcher stops the worker and supervisor
before unloading the module and overlay. Protocol qualification is documented
in the [HSM hardware validation](https://github.com/qpernil/virtual-yubihsm/blob/main/docs/i2c.md#hardware-validation).
