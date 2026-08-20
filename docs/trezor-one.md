# Trezor One Worker

## Scope

The Virtual Trezor worker runs the upstream Trezor One (`T1B1`) legacy C
firmware logic as a native Linux process. It is not an STM32 emulator and does
not execute a signed production firmware image. The worker is intended for
protocol, UI, and integration development; it does not provide the physical
security, firmware authenticity, or entropy guarantees of a hardware wallet.

The complete worker implementation, build, deployment, and hardware notes live
in [`virtual-trezor`](https://github.com/qpernil/virtual-trezor). This page
defines the boundary relevant to the supervisor.

## USB boundary

The profile contains the FunctionFS descriptor and string blobs for the main
Trezor vendor interface. The supervisor validates those blobs, derives one OUT
and one IN endpoint, publishes the blobs to FunctionFS, and opens:

```text
ep0, OUT, IN
```

It transfers the three descriptors to the unprivileged worker in the fixed
pre-bind resource bundle. The worker reports `PREPARED`, the supervisor binds
the UDC, and the worker reports `SERVING` after FunctionFS signals that the
interface is enabled. Normal USB packets then move directly between the host,
the FunctionFS endpoint descriptors, and the upstream Trezor message decoder;
the supervisor does not proxy or interpret them.

The current profile exposes only the main vendor interface. DebugLink and the
separate U2F HID interface are not exposed.

## Display and buttons

The upstream firmware owns its 128 by 64, 1,024-byte monochrome framebuffer and
all layout, drawing, animation, and button-state logic. The Linux platform code
sends that existing framebuffer through supervisor-opened resources and samples
active-low GPIO buttons.

Current profiles select one of these display arrangements:

- SH1106 over SPI, with GPIO Data/Command and reset;
- SSD1306 or SH1106 over I2C at address `0x3c`; or
- ST7789 over SPI, scaling the unchanged framebuffer into a centered 240 by 120
  image on a 240 by 240 panel.

The worker inherits the declared I2C, SPI, and GPIO descriptors across `exec`.
Their descriptor numbers are named by `USB_GADGET_RESOURCE_<NAME>_FD`
environment variables. It never opens the corresponding device paths and does
not need ownership of those device nodes.

An orderly worker exit blanks and powers off the selected display. `SIGKILL`
cannot run process cleanup; the replacement worker clears the panel during
display initialization.

## Lifecycle

`usbReconnect()` exits the worker. Control-socket EOF tells the still-running
supervisor to unbind the UDC, remove the complete gadget incarnation, create a
new worker with fresh descriptors, and bind again. Stopping the supervisor
service performs the same teardown without starting another incarnation. Only
the supervisor writes the UDC attribute.

## Process boundary

The Trezor worker remains a separate executable. This preserves crash
isolation, privilege separation, independent build and licensing boundaries,
and a narrow capability surface: after privilege drop, the worker can access
only the open descriptors, state directory, runtime directory, arguments, and
environment explicitly supplied by the supervisor.
