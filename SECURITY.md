# Security Policy

## Supported Versions

tylertoo is pre-1.0 and moves fast. Security fixes are made against the
latest `0.7.x` release; earlier `0.x` releases and the pre-rename
`gpq-tiles` releases are not supported — please upgrade before reporting
an issue against an older version.

| Version                 | Supported          |
| ------------------------ | ------------------- |
| 0.7.x                    | :white_check_mark:  |
| < 0.7.0                  | :x:                  |
| gpq-tiles (pre-rename)   | :x:                  |

## Reporting a Vulnerability

Please report suspected security vulnerabilities **privately**, using
GitHub's private vulnerability reporting:

1. Go to the [Security tab](https://github.com/geoparquet-io/tylertoo/security) of this repository.
2. Click **Report a vulnerability**.
3. Include as much detail as you can: affected version, reproduction
   steps, and the potential impact.

Please do not open a public issue for suspected vulnerabilities — private
reporting lets us assess and fix the problem before details are public.

We'll acknowledge your report and follow up as we investigate. There is
no fixed SLA at this stage of the project, but we treat security reports
as high priority.

## Automated Dependency Auditing

In addition to reported vulnerabilities, a weekly scheduled workflow
(`.github/workflows/security.yml`) runs `cargo-audit`, `cargo-deny`, and
`pip-audit` against the project's dependencies and files a GitHub issue
automatically when it finds an advisory.
