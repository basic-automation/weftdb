# Security Policy

## Supported versions

WeftDB is pre-1.0. Only the latest published `0.x` release receives fixes.

| Version | Supported |
|---------|-----------|
| 0.2.x   | yes       |
| < 0.2   | no        |

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

WeftDB is pre-beta. `weft-server` can serve TLS (`WEFT_TLS_CERT_FILE`,
`WEFT_TLS_KEY_FILE`) and authenticate API keys with roles (`WEFT_KEYS_FILE`, minted by
`weft-server keygen`), and a non-loopback bind refuses to start without a keys file unless
`WEFT_AUTH=disabled` says a gateway in front does the authenticating. What it does not have
yet: the security review the roadmap requires before 1.0. (Key reload without a restart
and the fsynced audit log have landed since: `WEFT_AUDIT_FILE`, default
`<store root>/audit.jsonl`.) Reports against TLS or authentication are in scope; the missing pieces
above are known gaps tracked on the roadmap.

Bound to a loopback address (the default), `weft-server` only answers requests
addressed to a loopback host and refuses state-changing requests from non-loopback
web origins, so a web page from another site, open in a browser on the same machine,
cannot drive it. That is a guard for the default deployment, not authentication: any
local process can still call the API, and a page served by another local web server
(any loopback origin, on any port) is still trusted while authentication is off. A local
reverse proxy that forwards a different `Host`, or a browser UI's
non-loopback `Origin`, needs `WEFT_ALLOW_ANY_HOST=1`, which turns the guard off.

What *is* in scope: memory-safety problems, panics or crashes reachable from untrusted
input (a malformed request body, a corrupt `.weftseg` segment, a hostile Line Protocol
payload), data corruption or silent precision loss, and anything that lets a caller read
or write data outside the aspect they addressed.
