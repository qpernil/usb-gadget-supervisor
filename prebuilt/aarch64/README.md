# ARM64 supervisor binary

This release build supports small ARM64 targets without a Rust toolchain. It
includes USB and device profile modes, including managed BSC targets. Install
it at the root-owned path described in the repository README.

Built from clean source commit `3ab6ace30d0ec266a0f6459f0b1df194f55e535a` on
`ubuntu4`, Ubuntu 26.04.1 LTS ARM64, with Rust/Cargo 1.98.1 and glibc 2.43.
The `raspberry-pi-i2c-target` path dependency was at
`925d083e4969d4f19acf79fc3cdb13fda4cab512`. ELF inspection shows a maximum requirement
of `GLIBC_2.39`, compatible with Raspberry Pi OS Debian 13 using glibc 2.41.

Verify before installation:

```sh
(cd prebuilt/aarch64 && sha256sum -c SHA256SUMS)
```

On a capable ARM64 machine, rebuild with:

```sh
cargo build --release --locked --bin usb-gadget-supervisor
```

Publish the binary, checksum and provenance through the canonical Git repository.
Preserve profile enablement, update installed binaries from the target checkout,
and restart only active profiles when a changed installed binary requires it.
