# ARM64 supervisor binary

This convenience build supports small ARM64 targets without a Rust toolchain.
It includes both USB and device profile modes. Install it at the same root-owned
path as a source build, following the repository README.

Built from clean source commit `047da3c4c6d5e6732b2a733305660cab48cd13ee`
on `ubuntu4`, Ubuntu 26.04.1 LTS ARM64, with Rust/Cargo 1.98.1 and glibc 2.43.
ELF version inspection shows a maximum requirement of `GLIBC_2.39`, compatible
with the Raspberry Pi OS Debian 13 targets using glibc 2.41.

Verify before installation:

```sh
(cd prebuilt/aarch64 && sha256sum -c SHA256SUMS)
```

GitHub `main` remains the source of truth. On a capable build machine, use
`cargo build --release --locked`.
