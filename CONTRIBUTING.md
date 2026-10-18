<!--
SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
SPDX-License-Identifier: Apache-2.0
-->

# Contributing to Atalaya

Bug reports, documentation fixes, dashboards and reproducible examples are welcome.
For a substantial behavior or architecture change, open an issue first to discuss the use case.

## Report a problem

Use the [issue templates](https://github.com/mariojcr/atalaya/issues/new/choose). Include the
version (image tag or commit), whether it is the server or the collector, how it is deployed,
reproduction steps and expected versus actual behavior. Remove passwords, host names and internal
addresses from logs and configuration. For vulnerabilities, follow [SECURITY.md](SECURITY.md).

## Develop and check a change

Rust is pinned in `rust-toolchain.toml` (rustup picks it up). The tests need no ClickHouse. Read
the [architecture](docs/architecture.md) and the guide for the part you are changing. Before opening
a pull request, run what CI runs:

```sh
cargo fmt --all --check
cargo clippy --release --all-targets --locked -- -D warnings
cargo test --release --locked
```

To try a change end to end, build the images (`docker build --target server .` and
`--target collector`) and use them in [examples/compose](examples/compose/README.md).

Keep pull requests focused. Describe the user-visible change and the checks you ran; update the
relevant docs and include a regression test when fixing behavior. Commit messages follow
[Conventional Commits](https://www.conventionalcommits.org/), which drive versions and the
changelog (see [releasing](docs/releasing.md)). Use fictional data in examples and screenshots. By
contributing, you agree that your contributions use the project's Apache-2.0 license.

## File licensing

Keep the SPDX copyright and Apache-2.0 identifier in comment headers on project-owned source and
configuration files, using the comment syntax for the file type. The bundled uPlot files in
`server/static/` keep their upstream MIT license header.
