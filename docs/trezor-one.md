# Trezor One Worker

## Target

The pictured and selected device is a Trezor Model One, internally `T1B1`. It
uses Trezor's legacy C firmware, not the MicroPython/C/Rust Trezor Core stack
used by Model T and Safe devices.

The Pi worker is a native Linux port of the upstream firmware sources. It is not
an STM32 instruction-set emulator and does not execute the signed production
firmware binary.

## Firmware and HAL boundary

The upstream legacy firmware owns a static 128 by 64 monochrome framebuffer:

```text
128 * 64 / 8 = 1024 bytes
```

The common `legacy/oled.c` code composes every pixel, character, bitmap,
dialogue, and animation. It exposes `oledGetBuffer()` and calls
`oledRefresh()` when the completed buffer should become visible.

The upstream Unix emulator supplies `oledInit()` and `oledRefresh()` from
`legacy/emulator/oled.c`. Current upstream code expands the one-bit framebuffer
into an SDL3 texture and presents a window. The Pi HAL instead transmits that
same firmware-owned buffer to an SSD1306 over Linux I2C.

```text
upstream layout and wallet code
              |
              v
       1024-byte framebuffer
              |
          oledRefresh()
              |
      /dev/i2c-1 at 0x3c
              |
         SSD1306 OLED
```

Likewise, common `legacy/buttons.c` owns press, release, and hold behavior. The
platform supplies only `buttonRead()`. The Pi implementation maps two active-low
GPIO lines to Trezor's No/Yes bits.

## Pi HAL

The external library is expected to export the hardware-facing symbols required
by the firmware build:

```text
libtrezor-pi-hal.a
  oled_pi.c
    oledInit
    oledRefresh
    emulatorPoll

  buttons_pi.c
    buttonRead

  usb_pi.c
    usbInit
    usbPoll
    usbReconnect
    usbTiny
    waitAndProcessUSBRequests
    usbFlush

  storage_pi.c
  timer_pi.c
  rng_pi.c
  setup_pi.c
```

The build links this HAL instead of the upstream SDL/UDP emulator objects. The
common wallet, signing, protocol, UI-layout, framebuffer, and button-state code
remains upstream.

## Direct USB

UDP exists in the desktop emulator only because a normal workstation process
does not own a USB device controller. The Raspberry Pi does, so production Pi
mode uses real FunctionFS endpoints directly:

```text
host OUT transfer -> FunctionFS OUT fd -> usbPoll -> Trezor message decoder
host IN transfer  <- FunctionFS IN fd  <- msg_out_data
```

The Trezor worker publishes the selected release's main vendor/WebUSB interface,
optional debug interface, and optional U2F HID interface. The supervisor owns
ConfigFS and UDC lifecycle but does not proxy ordinary packets.

`usbReconnect()` sends `RECONNECT_REQUEST` over the lifecycle control channel;
only the supervisor may unbind or rebind the UDC.

## Physical UI

The initial hardware target is:

- Raspberry Pi 4 or Raspberry Pi 5.
- Adafruit 128x64 OLED Bonnet with SSD1306 at I2C address `0x3c`.
- Bonnet button A on BCM GPIO 5 mapped to Trezor No/left/cancel.
- Bonnet button B on BCM GPIO 6 mapped to Trezor Yes/right/confirm.
- Reliable USB-C gadget power/data arrangement.
- Active cooling for Raspberry Pi 5.

The Bonnet's joystick is not part of the Trezor interface. It may later select
an appliance profile before USB attachment or request a controlled shutdown.

The final Trezor worker needs no SDL, X11, Wayland, visible window, UDP socket,
or reimplemented UI.

## Storage and entropy

The selected upstream emulator storage model is replaced or wrapped with a
profile-specific file under `/var/lib/virtual-trezor`. The worker owns the file;
the supervisor prepares only the containing directory and permissions.

Linux `getrandom()` is preferred over the upstream development PRNG, but it
does not make the resulting appliance equivalent to physical Trezor hardware.
Storage and seeds remain ordinary software-accessible data on a general-purpose
computer.

## Process and licensing boundary

The Trezor worker remains a separate executable launched by the supervisor. It
is not linked into the permissively licensed Rust supervisor or YubiKey worker.
This preserves upstream build independence, crash isolation, privilege
separation, and a clearer licensing boundary. Distribution still requires
normal compliance with all upstream licenses.

## Development sequence

1. Pin an upstream legacy release and build its native emulator on ARM64.
2. Link a stub Pi HAL without modifying common firmware sources.
3. Replace SDL OLED output with direct SSD1306 I2C writes.
4. Replace SDL keyboard input with GPIO button reads.
5. Publish Trezor FunctionFS descriptors and serve the main endpoints directly.
6. Connect the worker readiness/reconnect protocol to the supervisor.
7. Validate enumeration and commands with `trezorctl` and Trezor Suite.
8. Add optional U2F and debug interfaces only after the main wallet transport is
   stable.

This worker is for compatibility development and experimentation. It must not
be represented as providing Trezor's physical extraction resistance, firmware
authenticity, entropy guarantees, or security certification.
