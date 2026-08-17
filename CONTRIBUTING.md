# Contributing

Thank you for helping improve USB Gadget Supervisor.

The repository is currently alpha quality. Please discuss substantial
architecture or protocol changes in an issue before implementing them. Small
code and documentation corrections can go directly to a pull request.

## Design constraints

Contributions should preserve these boundaries:

- the supervisor owns privileged Linux gadget setup and worker lifecycle;
- workers own USB protocol behavior, cryptography, state, and device UI;
- endpoint payloads do not pass through the supervisor;
- profiles are root-owned, declarative, and strictly validated;
- one physical UDC exposes one selected device identity at a time; and
- a worker failure causes prompt UDC unbind and deterministic cleanup.

If a proposal relaxes one of these constraints, explain the security and
compatibility impact explicitly.

## Pull requests

Keep changes focused and include:

1. the problem being solved;
2. the security-boundary or USB-compatibility implications;
3. tests or validation steps appropriate to the change; and
4. documentation updates for externally visible behavior.

By contributing, you agree that your contribution is licensed under the MIT
License used by this project.
