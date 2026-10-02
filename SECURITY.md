# Security policy

## Supported versions

Security fixes are made on the latest release only.

| Version | Supported |
|---|---|
| 0.1.x (latest) | yes |
| older | no |

## Reporting a vulnerability

**Please do not open a public issue.** Report vulnerabilities privately with
[GitHub private vulnerability reporting](https://github.com/echaouchna/runway/security/advisories/new)
(Security tab → "Report a vulnerability").

Include what you found, how to reproduce it and its impact. We acknowledge
reports within 3 working days and aim to ship a fix or mitigation within 30
days, coordinating disclosure with you. We are happy to credit you.

## Scope

In scope: the runway CLI, its container image and its handling of
credentials, secrets and IAM (for example granting more access than
configured, leaking secret values, or acting on resources runway does not
own).

runway never reads or prints secret values, uses your Application Default
Credentials and does not send telemetry. Vulnerabilities in Google Cloud
itself should be reported to [Google](https://bughunters.google.com/).
