# Security Policy

## Supported versions

WeftDB is pre-1.0. Only the latest published `0.x` release receives fixes.

| Version | Supported |
|---------|-----------|
| 0.1.x   | yes       |
| < 0.1   | no        |

## Reporting a vulnerability

Please **do not open a public issue** for a security problem.

Report it through GitHub's private vulnerability reporting on this repository
(Security → Report a vulnerability), which opens a channel visible only to the
maintainers.

Please include what you have: affected version or commit, a reproducer, and the impact
you think it has. You will get an acknowledgement within a week. Since this is a small
project, a fix timeline depends on severity, and we will tell you what to expect rather
than leave you guessing.

## Scope, honestly

WeftDB is pre-beta and **has no authentication, authorization or transport security
yet** — `weft-server` exposes an unauthenticated HTTP API. That is a known gap tracked
on the roadmap, not a vulnerability report we need. Do not expose `weft-server`
directly to an untrusted network; put it behind something that does terminate TLS and
authenticate callers.

What *is* in scope: memory-safety problems, panics or crashes reachable from untrusted
input (a malformed request body, a corrupt `.weftseg` segment, a hostile Line Protocol
payload), data corruption or silent precision loss, and anything that lets a caller read
or write data outside the aspect they addressed.
