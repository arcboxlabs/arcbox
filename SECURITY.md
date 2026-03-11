# Security Policy

## Supported Versions

| Version | Supported |
|---------|-----------|
| 0.1.x   | Yes       |

## Reporting a Vulnerability

**Please do not open a public GitHub issue for security vulnerabilities.**

Instead, email **security@arcbox.dev** with:

- A description of the vulnerability
- Steps to reproduce
- Affected versions
- Any potential mitigations you've identified

We will acknowledge your report within **48 hours** and aim to provide a fix or mitigation plan within **7 days**.

## Scope

Given that ArcBox is a virtualization and container runtime, we take the following areas especially seriously:

- VM escape or guest-to-host breakout
- Container isolation bypass
- Privilege escalation
- Memory safety issues in `unsafe` code
- Denial of service against the daemon
- Supply chain (dependency) vulnerabilities
