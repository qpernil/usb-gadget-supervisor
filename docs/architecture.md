# Architecture

## Boundary

Linux USB gadget construction requires root; USB device protocol handling does
not. `usb-gadget-supervisor` therefore owns construction, authority, and
lifecycle, while one unprivileged worker owns protocol behavior, secrets, UI,
and persistent device state.

```text
host computer
     |
     | physical USB
     v
UDC <-> Linux composite gadget
          |                    |
          | FunctionFS FDs     | HID gadget FD
          +----------+---------+
                     |
                     v
            unprivileged worker

root supervisor: profile, ConfigFS, FunctionFS publication/open, UDC, process
worker: ep0 events, endpoint traffic, protocol, cryptography, policy, UI, state
```

The supervisor is not a USB payload proxy. Once construction is complete, the
kernel moves traffic directly between the host controller and the file
descriptors held by the worker.

## Division of responsibility

| Resource or decision | Owner |
| --- | --- |
| Root-owned schema-1 profile | Device project, enforced by supervisor |
| USB identity and descriptor bytes | Device profile |
| Descriptor structural validation | Supervisor |
| ConfigFS objects and function order | Supervisor |
| FunctionFS mount and descriptor publication | Supervisor |
| FunctionFS endpoint and HID node opening | Supervisor |
| UDC discovery, bind, and unbind | Supervisor |
| Worker credentials and process lifecycle | Supervisor |
| `ep0` runtime events and class/vendor SETUP handling | Worker |
| CTAP, CCID, Trezor, or vendor-bulk bytes | Worker |
| Private keys, wallet state, policy, display, buttons | Worker |

This split keeps descriptor *content* beside the device implementation without
requiring the device process to open or configure privileged kernel paths.

## Descriptor-driven resources

Each FunctionFS profile entry includes complete v2 descriptor and string blobs.
Before a worker exists, the supervisor:

1. validates the blob header, total length, speed-set counts, individual USB
   descriptor lengths, endpoint addresses, endpoint directions, and string
   table;
2. requires identical endpoint topology across full/high/super speed sets;
3. mounts FunctionFS root-only;
4. writes descriptors and strings to `ep0`;
5. opens the generated `ep1..epN` files as read-only for OUT endpoints and
   write-only for IN endpoints.

The kernel remains the final USB semantic validator. The supervisor parser adds
early diagnostics, derives the exact resource bundle, and prevents a mismatch
between the profile and the endpoints it hands to a worker.

Local hardware follows the same capability model. The supervisor opens I2C or
SPI character devices and uses the GPIO v2 API to claim profile-declared line
groups. It transfers exact GPIO line-request handles rather than GPIO-chip
handles, so a worker can operate or poll only its assigned lines. Profile
offset order defines the handle's bit order, and overlapping GPIO claims are
rejected before the worker starts.

ConfigFS HID functions are different: their report descriptor is written to
the ConfigFS function before binding, but `/dev/hidgN` appears only after the
UDC is bound. Those FDs therefore form a second, post-bind bundle.

## Incarnation state machine

```text
Preparing
  create ConfigFS gadget
  mount/publish/open FunctionFS
       |
       v
Awaiting PREPARED
  start fresh worker
  transfer pre-bind FDs
       |
       v
Binding
  link functions
  bind selected UDC
  open/transfer HID FDs
       |
       v
Serving (after SERVING)
       |
       | worker exit or control EOF
       v
Cleaning
  unbind first
  close socket and reap worker
  unmount FunctionFS
  remove gadget
       |
       +----> Preparing (new incarnation)

service stop: Cleaning -> supervisor exits
```

There is no lighter reconnect state. A firmware reconnect request is expressed
by worker exit. A fresh Unix process gives a complete reset of application
threads, buffers, endpoint state, and received capabilities, while the
supervisor service and its exclusive UDC lock remain alive.

The worker receives its USB resources once at startup and never mutates that
set while serving. FunctionFS `ENABLE`, `DISABLE`, and `UNBIND` events on the
inherited `ep0` replace redundant attach/detach control messages.

## UDC discovery

Linux exposes registered USB Device Controllers as entries in
`/sys/class/udc`. The supervisor sorts the names and selects the first, or
requires an exact `--udc NAME` override. More than one may exist with
`dummy_hcd`, virtualization, custom carrier hardware, or an additional physical
device controller. One selected UDC exposes one profile at a time.

Binding is the final write of that name to the gadget's ConfigFS `UDC`
attribute. Writing an empty value unbinds it and appears to the host as a
physical disconnect.

## Pi 4 and Pi 5

The deployment is tested on both 64-bit Ubuntu and 64-bit Raspberry Pi OS.
The gadget architecture and DWC2 peripheral-mode setup are the same on both.

The software architecture is the same on Raspberry Pi 4 and 5: DWC2 device
mode, ConfigFS, FunctionFS, and the USB-C gadget connection. Typical UDC names
differ (`fe980000.usb` versus `1000480000.usb`), which is why discovery uses
sysfs instead of a hard-coded name. The Pi 5's ordinary RP1 USB host ports are
not alternative gadget controllers.

I2C target/peripheral support is unrelated. USB gadget mode depends on the USB
device controller; it does not depend on the SoC exposing an I2C target.

## Unix capability model

An open file descriptor is both a channel and a capability. The supervisor can
open a root-only endpoint or device node, transfer a duplicate with
`SCM_RIGHTS`, and leave the path inaccessible to the worker. The worker can use
only the operations allowed by that already-open file description.

macOS also supports Unix-domain `SCM_RIGHTS` descriptor passing, which can be
tested with a local datagram socket. It does not support the exact Linux
`AF_UNIX/SOCK_SEQPACKET` transport used here. ConfigFS, FunctionFS, and UDC
gadget mode are Linux-specific as well.

Local I2C, SPI, and GPIO descriptors use the same mechanism: the supervisor
opens them and appends them to the pre-bind `SCM_RIGHTS` bundle in profile
order. Device-specific ioctls and policy remain in the worker.

## Unix architecture, Linux mechanism

The supervisor is a Unix-style architecture built on Linux-specific device
facilities. Unix provides the process model: small programs with narrow
responsibilities, standard process lifecycle, users and permissions, local
sockets, and file descriptors that act as both communication channels and
capabilities. A fresh worker process is a meaningful reset boundary, and an
open descriptor is a concrete grant of authority rather than merely a path to
reopen later.

Linux provides the particular mechanisms that make this virtual USB device
possible: UDC gadget mode, ConfigFS, FunctionFS, HID gadget nodes, the GPIO
character-device API, and the I2C and SPI device interfaces. Another Unix-like
system could preserve the supervisor/worker process architecture, but would
need a different implementation of this device-facing layer.

In short: Unix makes the design natural; Linux makes this particular device
possible.

## Operational transparency

The supervisor is intended to automate an understandable manual procedure, not
to create a second hidden system. Profiles are ordinary root-owned files,
workers are ordinary executables, services are ordinary systemd units, and the
supervisor can be invoked directly from a shell. ConfigFS objects, processes,
open descriptors, mounts, and logs remain inspectable through normal Linux
interfaces.

The same rule applies to surrounding automation. CI should invoke repository
build and test commands rather than contain a private build implementation;
deployment tools should perform the documented install and service operations
rather than become a runtime requirement. If CI or configuration automation is
unavailable, an operator should still be able to build, install, start, stop,
inspect, and debug the system manually.

## Supported worker shapes

| Device | Kernel surface | Worker data plane |
| --- | --- | --- |
| Virtual YubiKey | ConfigFS HID plus FunctionFS CCID | FIDO HID FD; CCID OUT/IN/interrupt FDs; CCID `ep0` events |
| Virtual Trezor | FunctionFS vendor interface | main OUT/IN FDs; `ep0` events |
| Virtual YubiHSM | FunctionFS vendor bulk interface | bulk OUT/IN FDs; `ep0` events |

HID is not inherently shareable. The host's HID/FIDO stack and CCID/PCSC stack
bind to separate USB interfaces, so FIDO can remain available while another
application holds an exclusive CCID session. That independence comes from USB
interface and host-driver separation, not from special multi-client behavior
inside `/dev/hidgN`.

## Trust statement

The supervisor is trusted to validate installed metadata, configure the kernel,
and launch the approved worker identity. It must not parse APDUs, CTAP messages,
Trezor protobufs, PINs, seeds, or private keys. Process separation reduces
privilege exposure; it does not make a Raspberry Pi tamper-resistant hardware.
