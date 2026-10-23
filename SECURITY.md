# Security policy

## Reporting a vulnerability

Please report suspected vulnerabilities through a private GitHub security
advisory for this repository. Do not include credentials, prompts, responses,
OAuth tokens, private model names, internal addresses, or other sensitive
operator data in a public issue.

Include the affected version or commit, deployment shape, provider kind,
request endpoint, expected behavior, and enough redacted reproduction detail
to investigate. Reports will be acknowledged on a best-effort basis; this is
an independently maintained project and does not currently carry a response
time guarantee.

## Security boundary

Infernaut protects only inference traffic that clients actually send
through it. It does not inspect an agent's filesystem, terminal, tools,
browser, memory, network connections, or requests sent directly to another
provider. Privacy locks, route realm constraints, plugins, diagnostics, and
authentication are useful controls, but they do not turn the gateway into a
sandbox or a complete data-loss-prevention system.

The default example binds to loopback. Before listening on `0.0.0.0` or a LAN
address, configure a strong administrative key and distinct client keys,
restrict network access with a firewall, and use a trusted TLS reverse proxy
when traffic crosses an untrusted network.

## Sensitive local files

The following files are intentionally excluded from version control and Cargo
packages:

- `.env` and `.env.*` other than `.env.example`
- `infernaut.toml` and timestamped configuration backups
- `sniffer.toml` and local plugin evidence
- build, log, and runtime output

Treat OAuth credential stores, inspection-plugin payload captures, and copied
configuration backups as secrets even when they live outside this repository.
