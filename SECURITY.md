# Security Policy

USB Gadget Supervisor is security-sensitive infrastructure, but it is not yet
implemented or released. The current repository contains design documentation
only.

## Reporting a vulnerability

Please do not open a public issue for a suspected vulnerability. Use GitHub's
private vulnerability reporting feature for this repository. Include the
affected component, expected impact, reproduction details, and any suggested
mitigation.

Reports about device-protocol behavior belong in the relevant worker project
unless the issue crosses the supervisor's privilege, lifecycle, ConfigFS,
FunctionFS, or UDC boundary.

## Security expectations

This project is intended for development and protocol compatibility. A
software-backed security token or wallet on a Raspberry Pi does not provide the
physical tamper resistance, secure element, or trusted display guarantees of
the corresponding hardware device.
