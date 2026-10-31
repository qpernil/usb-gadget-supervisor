# ARM64 supervisor binary

Hostnames and device/account identifiers below are illustrative; replace them with your own configured values.

This release build is retained only for `i2c-target-1` and `i2c-target-2`.
Every other lab host builds and installs the supervisor locally. It
includes USB and device profile modes, including managed BSC targets. Install
it at the root-owned path described in the repository README.

Built on `build-host`, Ubuntu 26.04.1 LTS ARM64, with Rust 1.99.1, Cargo 1.99.1,
and glibc 2.43, from these clean source revisions:

| Build input | Commit |
| --- | --- |
| `usb-gadget-supervisor` | `7562838fe10f60b1faa2715f76ca81308db67405` |
| `raspberry-pi-i2c-target` | `5c8939fdf0e594ba0b782e17ec22d729dfc2040d` |

The source commit's `Cargo.lock` includes libc 0.2.191. ELF inspection shows
a maximum requirement of `GLIBC_2.39`, compatible with Raspberry Pi OS Debian 13
using glibc 2.41.

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
