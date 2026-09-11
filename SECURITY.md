# Security policy

## Supported versions

Before the first stable release, only the latest commit on `main` receives
security fixes. After release, this table will name every supported line.

| Version | Supported |
| --- | --- |
| Unreleased | Yes |

## Reporting a vulnerability

Please use GitHub's private vulnerability reporting for this repository. If
that option is unavailable, contact the repository owner privately through the
contact method on the owner's GitHub profile. Do not disclose the issue in a
public issue, discussion, or pull request before a fix is available.

Include the affected version or commit, platform, reproduction steps, expected
impact, and any suggested mitigation. Avoid attaching personal files, directory
listings, access tokens, or diagnostic archives that have not been reviewed for
sensitive paths.

The maintainer will acknowledge a complete report as soon as practical,
coordinate validation and remediation, and credit the reporter unless anonymity
is requested. No bounty or response deadline is promised.

## Security boundaries

DiskPie reads user-selected filesystem metadata and exposes confirmed shell
actions. It must run without elevation, avoid following reparse points by
default, avoid hydrating cloud placeholders during metadata scans, and never
delete outside test-owned fixtures in automated tests. See `AGENTS.md` for the
full engineering boundary.

## Current file-cleanup contract

The owner-approved product uses the Recycle Bin only. If safe recycling cannot
be established (for example an oversized item or unsupported provider), refuse
without permanent-delete fallback or a native dialog offering it. Emptying the
Recycle Bin is not a DiskPie feature. Review and native target identity checks
remain required. Legacy delete/empty-bin code still exists below the UI; this
policy update does not claim it was removed or that the new UI flow was tested.

Windows' official uninstall interface is a separate user-controlled system
flow. DiskPie must not silently uninstall apps or erase application directories.
Leftover file cleanup follows the same recycle-only policy.
