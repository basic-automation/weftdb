#!/bin/bash
# The container health check for the WeftDB server image (1.0 plan PAC-9).
#
# It asks the server's own probe endpoint over loopback, inside the container's own network
# namespace:
#
#   /ready  the store is open and requests are being answered (weft-server/src/routes.rs,
#           Scope::Probe — anonymous, never shed by admission, ROB-10)
#
# `/health` only says the process is up, which a store that failed to open also satisfies for a
# moment; `/ready` is the answer an orchestrator should gate traffic on, so it is the one asked.
#
# Why bash and not curl: the published image carries no HTTP client on purpose (see
# packaging/docker/Dockerfile) — one less apt package, and no mirror to trust at build time.
# Bash's /dev/tcp is enough to open the socket and send one request, and bash is already in the
# base image. It is a smaller attack surface, not a security boundary: bash can open a socket to
# anywhere, so what the image actually refuses to carry is a client an operator would reach for
# in a hurry.
#
# TLS is the one deployment this check cannot serve. With WEFT_TLS_CERT_FILE and
# WEFT_TLS_KEY_FILE set the listener speaks TLS only (weft-server/tls.rs), and this probe speaks
# plain HTTP, so a TLS container reports unhealthy no matter how well it is serving. Override the
# check (`--no-healthcheck`, or Compose `healthcheck: {disable: true}`) and gate traffic on your
# own probe until the server offers a readiness check that needs no network — filed as a follow-up
# against PAC-9. Making the built-in check TLS-capable would mean an HTTP+TLS client in the image,
# which is the package this file exists to avoid.
#
# The port comes from WEFT_SERVER_ADDR, because that is the address the server actually binds:
# an operator who runs `-e WEFT_SERVER_ADDR=0.0.0.0:9000` moves the server and the check
# together. A bare port (no colon) is used as-is. Loopback is hardcoded so the check never
# depends on routing or on an address the operator may have rebound.
#
# The Host header is sent as `127.0.0.1` with no port: under the default loopback bind the
# server's Host guard (SEC-8) applies to loopback peers, and a bare loopback name is what it
# accepts. Under a non-loopback bind the guard is off, so the same header stays correct.
#
# Exit status: 0 when /ready answers 200; 1 for anything else, including a connection that fails
# or a status line that does not arrive. Docker records the non-zero status and the last lines of
# output, so `docker inspect --format '{{json .State.Health}}'` shows which of those it was.
set -eu

addr=${WEFT_SERVER_ADDR:-127.0.0.1:8080}
case $addr in
	*:*) port=${addr##*:} ;;
	*) port=$addr ;;
esac
# A non-numeric port is a typo in an operator's environment, not something to guess at.
[[ $port =~ ^[0-9]{1,5}$ ]] || {
	echo "weft-health: WEFT_SERVER_ADDR=\`$addr' has no numeric port" >&2
	exit 1
}

# -t 2 bounds the read: a wedged server that accepts the socket and never answers reads
# unhealthy on the next check rather than hanging until Docker's own --timeout.
if ! exec 3<>"/dev/tcp/127.0.0.1/$port"; then
	echo "weft-health: cannot open 127.0.0.1:$port" >&2
	exit 1
fi
printf 'GET /ready HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n' >&3
if ! IFS= read -r -t 2 status_line <&3; then
	echo "weft-health: no status line from 127.0.0.1:$port within 2s" >&2
	exit 1
fi
# The status line ends with CRLF, so the carriage return comes off before the match: `read -r`
# keeps it, and an exact `HTTP/1.1 200` against `HTTP/1.1 200 OK\r` never matches — a health
# check that reports unhealthy on a server answering 200 is the worst possible failure mode,
# because an orchestrator then kills a perfectly good node.
status_line=${status_line%$'\r'}
case $status_line in
"HTTP/1.1 200" | "HTTP/1.1 200 "* | "HTTP/1.0 200" | "HTTP/1.0 200 "*) exit 0 ;;
*)
	echo "weft-health: /ready answered \`$status_line'" >&2
	exit 1
	;;
esac
