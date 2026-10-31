#!/bin/sh
# The reference host's cold hook for `weft-xbench --cold-hook` (#146).
#
#   cold-hook.sh restart ENGINE     restart ENGINE's server container (a no-op for the in-process
#                                   engines, weftdb and duckdb, whose arms reopen their own files)
#   cold-hook.sh drop-caches ENGINE sync and drop the host's page cache, dentries and inodes
#
# Containers are named `${XBENCH_CONTAINER_PREFIX:-weft-xbench-}<name>`, with the TimescaleDB one
# called `timescale`. Dropping the page cache needs root: this runs `echo 3 > drop_caches` in a
# throwaway privileged container (the host's /proc/sys is the container's), so the harness itself
# runs unprivileged and no password is typed. Set XBENCH_DROP_CACHES to a command of your own to
# drop them another way.
set -eu

action=${1:?usage: cold-hook.sh restart|drop-caches ENGINE}
engine=${2:?usage: cold-hook.sh restart|drop-caches ENGINE}
prefix=${XBENCH_CONTAINER_PREFIX:-weft-xbench-}

case "$action" in
restart)
	case "$engine" in
	weftdb | duckdb) ;;
	timescaledb) docker restart --time 30 "${prefix}timescale" >/dev/null ;;
	clickhouse | questdb | influxdb3 | iotdb) docker restart --time 30 "${prefix}${engine}" >/dev/null ;;
	*)
		echo "cold-hook.sh: no container for engine '$engine'" >&2
		exit 2
		;;
	esac
	;;
drop-caches)
	sync
	if [ -n "${XBENCH_DROP_CACHES:-}" ]; then
		sh -c "$XBENCH_DROP_CACHES"
	else
		docker run --rm --privileged alpine:3.20 sh -c 'sync; echo 3 > /proc/sys/vm/drop_caches'
	fi
	;;
*)
	echo "cold-hook.sh: unknown action '$action'" >&2
	exit 2
	;;
esac
