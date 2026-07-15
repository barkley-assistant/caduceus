# Security Policy

## Reporting a Vulnerability

Do not open public issues for suspected security vulnerabilities. Email
`barkleyassistant@gmail.com` with "Caduceus" in the subject line instead.

Include a clear description, affected version, reproduction steps or a minimal
proof of concept, and the potential impact. Please say whether the report has
already been shared elsewhere.

We will assess reports privately and may request further details. Please avoid
public disclosure until we have had a reasonable opportunity to investigate and
prepare a fix.

## Scope

This policy covers the Caduceus repository and its released components,
including the Rust daemon, Python bridge template, Hermes plugin, configuration,
GitHub integration, and scheduling wrapper.

The worker harness configured by an operator, GitHub, and Hermes Agent are
outside this project's scope. Report vulnerabilities in those products to their
respective maintainers.

## Security Guidance

Treat Caduceus state, claims, transcripts, configuration, and credentials as
sensitive operational data. Follow the recovery procedures in
[docs/state-recovery.md](docs/state-recovery.md); do not edit daemon-owned state
files directly.

The public-comment filter is a security-relevant control. Report any bypass
that exposes internal tools, credentials, or other sensitive information through
the private contact above.
