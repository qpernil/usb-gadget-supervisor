# USB lifecycle

FunctionFS exposes seven event types. The supervisor owns `ep0`, drains that
event stream continuously, and sends the worker abstract lifecycle or control
records. FunctionFS coalesces pending lifecycle events, so the worker observes
the kernel's retained state transitions rather than a guaranteed history of
every transient intermediate event.

## Event review

| FunctionFS event | Typical cause | Supervisor action | Worker action |
| --- | --- | --- | --- |
| `BIND` | Function attached to a ConfigFS gadget | forward `Bind` | reset virtual controller; remain unconfigured |
| `UNBIND` | Gadget/function generation removed | forward `Unbind`; stop endpoint use | reset virtual controller |
| `ENABLE` | Host selects configuration or an interface altsetting is activated | forward `Enable` with a fresh activation | configure firmware once; start endpoint service |
| `DISABLE` | Host selects configuration zero, bus reset, cable disconnect, or gadget disable | forward `Disable`; retain endpoint handles | let FunctionFS cancel endpoint operations; discard controller state and reset |
| `SETUP` | Class, vendor, or forwarded standard control request | obtain OUT data, query worker, complete/stall `ep0` | run the real or native control engine |
| `SUSPEND` | Host suspends the configured USB link, commonly during host sleep | forward only; leave generation and endpoints intact | preserve state and invoke suspend callback |
| `RESUME` | Host resumes the suspended link | forward only | invoke resume callback and continue |

`ENABLE` is idempotent at the worker boundary. FunctionFS can activate more
than one interface or repeat activation around `SET_INTERFACE`; that must not
make firmware process `SET_CONFIGURATION(1)` twice. A genuine `DISABLE`
clears the enabled state, so the next `ENABLE` performs configuration again.
It also starts a fresh endpoint activation. Blocking endpoint operations see
FunctionFS's native unconfigured-device behavior. The worker retires its old
controller-side queues before the next activation without replacing the
FunctionFS descriptors.

## Host sleep

With independent Pi power, ordinary host sleep should be the smallest path:

```text
configured -> SUSPEND -> RESUME -> configured
```

The worker process, USB generation, descriptor personality, endpoint files,
firmware state, and display timer survive. USB transfers simply stop while the
host is asleep. If the host or hub resets the link on wake, the valid path is:

```text
SUSPEND -> DISABLE -> ENABLE
```

The worker remains alive but its virtual USB controller is reset and
reconfigured. If the link is physically disconnected while the Pi remains
powered, the device normally sees `DISABLE`; reattachment causes enumeration
and `ENABLE` again.

If the Pi itself is powered only by USB VBUS and the host removes VBUS, the Pi
loses power before software can promise any event handling. Normal system boot
and service startup recreate the gadget when power returns. The design does
not depend on a Mac, PC, hub, or firmware keeping VBUS present during sleep.

## Events that are not FunctionFS events

- USB bus reset has no separate FunctionFS event. It appears through
  `DISABLE`, followed by `ENABLE` if the host configures the device again.
- Opening or closing a host application handle does not change USB lifecycle
  state and produces no event.
- `SET_ADDRESS`, `SET_CONFIGURATION`, and `SET_INTERFACE` are largely handled
  by the composite gadget core. Their observable result is binding or endpoint
  activation; class/vendor requests selected by the personality reach `SETUP`.
- Endpoint halt, clear-halt, FIFO status/flush, remote wakeup, SOF, link power
  states beyond suspend/resume, and transfer cancellation are not lifecycle
  events. Current Trezor One needs none of those endpoint-control operations.
  A future worker that exposes alternate settings, remote wakeup, or explicit
  endpoint halt control will need typed protocol support rather than Linux FDs.
- A pending `SETUP` data phase can be superseded by a newer setup or lifecycle
  event. FunctionFS reports `EIDRM`; the supervisor drops that one transaction
  and continues with the queued event.

## Generation boundaries

Host lifecycle events do not replace a USB generation. A generation changes
only when the worker publishes a new personality (including
`usbReconnect()`), the profile is reloaded with `SIGHUP`, or fault recovery
starts a new worker. Generation replacement unbinds the UDC so blocked endpoint
calls are released, asks the worker to quiesce, closes the old FunctionFS
files, and then creates an entirely new ConfigFS/FunctionFS projection and
endpoint set. Before binding that replacement, the supervisor waits until the
UDC has been detached for at least 250 ms. This single rule covers
worker-requested reconfiguration, SIGHUP, and fault recovery; initial
attachment is immediate because it has no preceding detach.

An empty worker `Configure` splits that operation at the detached boundary.
The supervisor removes the current generation after quiescence and waits with
no FunctionFS generation until the same worker supplies a nonempty
personality. That later configuration binds immediately: the worker-controlled
interval replaces, rather than stacks with, the fixed replacement dwell.

The replacement may advertise the same VID, PID, serial, and descriptors, but
it is a new host attachment with new interface and endpoint objects. Handles
opened against the detached generation are stale and must fail; host software
must discover and open the replacement device. The dwell makes that detach
reliably observable—it does not manufacture a different USB identity.
