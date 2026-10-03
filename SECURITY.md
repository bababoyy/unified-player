# Security Policy

## Reporting a Vulnerability

When this repository becomes public, report suspected vulnerabilities through
GitHub Private Vulnerability Reporting:

1. Open the repository's **Security** tab.
2. Open **Advisories**.
3. Select **Report a vulnerability**.

This private reporting route will be enabled as part of the public-visibility
transition. Until then, the repository is private and has no public security
intake channel.

Never put vulnerability details in a public issue, discussion, pull request,
or comment. If **Report a vulnerability** is unavailable after the repository
becomes public, open a minimal public issue titled `Private security reporting
route unavailable` whose body says only `I need a private channel to report a
potential security issue.` Do not include the vulnerability, impact,
reproduction, credentials, account details, logs, URLs, captures, or other
sensitive evidence in that fallback issue.

## What to Include

- affected version or commit
- operating system and enabled Cargo features
- impact and a minimal reproduction
- whether the issue involves credentials, browser state, signed URLs, or
  diagnostic output

Redact tokens, cookies, authorization headers, account identifiers, local paths,
HAR files, and raw provider responses. A sanitized diagnostic summary is safer
than a log archive.

## Supported Versions

Until the first public release policy is announced, security fixes are handled
on the active development branch. Release support windows and disclosure
timelines will be published with the first public release.

## Design Commitments

- Provider and backend modules do not write directly to the terminal.
- Diagnostics are bounded and allowlisted rather than raw provider dumps.
- Private capture and replay features remain opt-in and are not part of normal
  support bundles.
- New network or persistence behavior must document retention, redaction, and
  failure behavior.
