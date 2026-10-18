<!--
SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
SPDX-License-Identifier: Apache-2.0
-->

# Security

Use the latest release when reporting a vulnerability. Include the affected version and
component (server or collector), reproduction steps, impact and a minimal example with fictional
data. Do not include live passwords, session keys or data from your hosts.

If the repository's [Security page](https://github.com/mariojcr/atalaya/security) offers **Report a
vulnerability**, use that private channel. If it is unavailable, open an issue containing only a
request for a private reporting channel, without vulnerability details, and wait for a maintainer
to provide one. Do not publish exploit details in a public issue.

For access to the server (optional OpenID Connect sign-in, or a private network or authenticating
proxy without it) and the privileges the collector runs with, see
[deployment](docs/deployment.md), [sign-in](docs/configuration.md#sign-in) and the collector's
[privileges](docs/collector.md#privileges).
