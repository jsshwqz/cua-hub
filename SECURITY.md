# Security Policy

## Supported Versions

We release patches for security vulnerabilities on the latest minor version.
Older versions may receive fixes at the maintainers' discretion.

| Version | Supported          |
| ------- | ------------------ |
| 0.1.x   | :white_check_mark: |
| < 0.1   | :x:                |

## Reporting a Vulnerability

**Do not open a public issue for security vulnerabilities.**

Instead, please report privately:

- Open a [GitHub Security Advisory](https://github.com/jsshwqz/cua-hub/security/advisories/new)
  (preferred), or
- Email the maintainers via the address listed on the repository's Security tab.

Please include:

- A description of the vulnerability and its impact
- Steps to reproduce (proof of concept if possible)
- The affected version(s) and commit SHA
- Any suggested mitigation

## What to expect

- We will acknowledge receipt within 72 hours.
- We will keep you informed of progress toward a fix.
- Once fixed, we will publish a GitHub Security Advisory and credit you
  (unless you prefer to remain anonymous).

## Scope notes

cua-hub is a computer-use agent: it can see the screen, move the mouse, and
type. Please be especially careful to describe vulnerabilities that could lead
to unintended actions on a user's machine. Reports about model-prompt
manipulation that depend on a specific third-party model are best filed
upstream with the model provider, unless cua-hub's own handling is at fault.
