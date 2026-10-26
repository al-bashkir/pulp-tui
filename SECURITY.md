# Security policy

## Supported versions

Only the latest release gets security fixes.

## Report a vulnerability

Do not open a public issue. Report it privately through
[GitHub private vulnerability reporting](https://github.com/al-bashkir/pulp-tui/security/advisories/new).

Include:

* the pulp-tui version (`pulp-tui --version`) and the Pulp version,
* the steps to reproduce,
* the impact (for example: a secret in a log, a TLS check that is skipped, a change to a
  Pulp object without its ownership labels).

Remove real secrets, host names and repository data from the report.

You get an answer within 7 days. After a fix is released, the advisory is published with
credit to you, if you want it.

## Scope

In scope: pulp-tui itself (this repository), its release binaries and its container image.

Out of scope: vulnerabilities in Pulp or its plugins. Report them to the
[Pulp project](https://pulpproject.org/security/).
