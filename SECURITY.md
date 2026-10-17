# Security policy

Security fixes target the latest `main` revision. Released versions do not yet
have a separate long-term support commitment.

Please [report a vulnerability privately](https://github.com/hudson-infinity/hudson/security/advisories/new).
Include the affected revision, reproduction steps, likely impact, and a minimal
example without real credentials or customer data. Do not open a public issue
or PR containing an unpatched vulnerability or working exploit.

Maintainers will assess the report and coordinate a fix and disclosure with you.
This volunteer project does not promise a response deadline or bounty.

Hudson does not provide sandbox isolation for application tools. Consult the
[implementation audit](docs/implementation-status.md) and configure only trusted
tools. Never commit API keys, tokens, database credentials, or customer data.
