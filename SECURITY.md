# Security policy

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub's
[private vulnerability reporting](https://github.com/Seggys116/frust/security/advisories/new)
instead of a public issue. Include the version, platform, and a reproduction if you have one.

You can expect an acknowledgement within a few days. Fixes ship in a new release, and the advisory credits the
reporter unless they prefer otherwise.

## Scope

Of particular interest: anything that lets one build or user read, poison or corrupt another's compile cache or
target directory, path handling in the shared `FRUST_HOME`, and the install scripts.

## Supported versions

Only the latest release receives fixes.
