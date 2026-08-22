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

The disk profile contains no USB bytes. At startup the worker initializes the
genuine legacy `libopencm3` USB stack against a virtual controller. The shared
discovery parser issues control requests through that controller, so the real
firmware code returns its device, configuration, string, Microsoft OS 1.0, and
WebUSB descriptors. The parser returns one typed CBOR personality.

The supervisor validates that object, derives both firmware interfaces and
their four data endpoints, and publishes the corresponding FunctionFS
representation:

```text
ep0, main OUT, main IN, U2F OUT, U2F IN
```

It retains `ep0` and passes the four actual FunctionFS data endpoint files to
the worker; `ep0` lifecycle and setup traffic is translated to control records.
After the worker reports `Serving`, the supervisor binds the UDC. Setup requests
are fed through the same virtual controller and genuine firmware control
engine. Worker-owned endpoint helpers connect normal packets directly between
FunctionFS and the upstream Trezor message decoder.

The discovered normal configuration exposes the main vendor interface and the
separate U2F HID interface, each with one 64-byte interrupt OUT endpoint and
one 64-byte interrupt IN endpoint. DebugLink remains disabled.

The FunctionFS blob also carries the upstream-compatible Microsoft OS 1.0
features for interface zero: compatible ID `WINUSB` and
`DeviceInterfaceGUIDs={0263b512-88cb-4136-9613-5c8e109d8ef5}`. The profile
sets ConfigFS signature `MSFT100` with vendor request code `0x21`, so Windows
can bind its inbox WinUSB driver while the USB interface remains vendor class.
The independent WebUSB BOS capability uses version 1.0 and request code
`0x01`, with no automatic landing page. WebUSB supports browser discovery and
permissioned access; it is not the Windows driver.

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

The supervisor appends the declared display-bus and GPIO descriptors to the
pre-bind `SCM_RIGHTS` bundle in profile order. The worker never opens the
corresponding device paths and does not need ownership of those device nodes.

An orderly worker exit blanks and powers off the selected display. `SIGKILL`
cannot run process cleanup; the replacement worker clears the panel during
display initialization.

## Lifecycle

`usbReconnect()` rediscovers and republishes the firmware personality. The
supervisor asks the same worker to quiesce, unbinds and rebuilds the gadget,
passes replacement endpoint handles, and binds again. The firmware process and
its state survive this USB re-enumeration. Host `SUSPEND`/`RESUME` also preserves
the generation; a bus reset is represented by `DISABLE`/`ENABLE`. Worker
failure remains the broader reset boundary and starts a fresh process. Only
the supervisor writes the UDC attribute.

## Process boundary

The Trezor worker remains a separate executable. This preserves crash
isolation, privilege separation, independent build and licensing boundaries,
and a narrow capability surface: after privilege drop, the worker can access
only the open descriptors, state directory, runtime directory, and arguments
explicitly supplied by the supervisor.
