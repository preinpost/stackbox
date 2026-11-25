# Security Policy

## Reporting a vulnerability

Please **do not** open a public issue, pull request, or discussion for a security
vulnerability in stackbox.

Report it privately through GitHub Security Advisories:

<https://github.com/preinpost/stackbox/security/advisories/new>

Include, if you can:

- the affected version or commit (`stackbox --version` or the release tag),
- the component involved (coordinator, portal, agent, relay, or the libkrunfw kernel pipeline),
- steps to reproduce and the expected impact.

We aim to acknowledge a report within a few business days and will keep you informed while
we investigate and prepare a fix. Please give us a reasonable window to ship a patch before any
public disclosure.

## Supported versions

Security fixes are made for the latest released version (the highest `vX.Y.Z` tag on
`main`). Older releases do not receive backports; upgrade to the latest release to get fixes.

## Deployment notes

- The coordinator and the web portal accept API requests without authentication when
  `STACKBOX_API_TOKEN` is empty. Set a token before exposing the service beyond a
  loopback address.
- The provided `compose.yaml` publishes the portal on `127.0.0.1` only.
