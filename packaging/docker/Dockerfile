# syntax=docker/dockerfile:1.7

# The WeftDB server container image (1.0 plan PAC-9).
#
# Build it with scripts/packaging/build-image.sh, which passes the version, revision and
# created labels and then runs scripts/packaging/smoke-container.sh against the result. The
# short version:
#
#   scripts/packaging/build-image.sh --tag weftdb:local
#   scripts/packaging/smoke-container.sh --image weftdb:local
#
# Why the builder is the manylinux image rather than `rust:`. The release publishes Linux
# binaries built in the pinned manylinux_2_28 baseline, and `scripts/check-glibc.sh` fails a
# build whose artifacts need glibc newer than 2.28 (PAC-2) — that is the promise about the
# oldest host a WeftDB binary starts on. Building the image anywhere else would put a binary
# with a different (higher) floor behind the same name, so the image builds in the same image
# the release builds in, from the same pinned toolchain file, with the same `--profile dist`
# (PAC-1b: stripped, LTO) and the same `--locked` lockfile. The smoke test reads the floor back
# out of the binary inside the finished image rather than trusting this comment.
#
# Why the runtime is Debian slim rather than distroless or `scratch`. The binary is linked
# dynamically against glibc, so the runtime has to provide the same C library family it was
# linked against, at a version at or above the 2.28 it was linked to (Debian 12 is 2.36). Slim is
# the smallest official image that has it plus `bash`, which is what the health check runs on.
# Nothing is installed at build time: no apt run in this stage at all, so the image depends on no
# mirror, carries no package-manager state, and ships no HTTP client — the health check opens its
# socket with bash's `/dev/tcp`, and the smoke test speaks HTTP from the host. An operator who
# wants a client inside a running container is being asked to debug a deployment they control, and
# can `docker exec` one in; the published image stays a server.
#
# Secure by default, in the way SEC-8's exposure matrix requires. The image binds
# `0.0.0.0:8080`, because a server bound to loopback inside a container is unreachable through
# a published port. Plan C8 then requires authentication on a non-loopback bind, so the
# out-of-the-box experience is deliberate: with no keys file mounted the server refuses to
# start and says why. Mount one (SEC-6) and it serves:
#
#   docker run -p 8080:8080 -v "$PWD/keys:/run/weftdb:ro" -v weftdb-data:/var/lib/weftdb \
#     -e WEFT_KEYS_FILE=/run/weftdb/api-keys.json weftdb:local
#
# A deployment behind a gateway that owns authentication sets `WEFT_AUTH=disabled` instead,
# which is that deployment's explicit opt-in, never this image's default. TLS, when the
# termination is not upstream, is `WEFT_TLS_CERT_FILE` + `WEFT_TLS_KEY_FILE`. Every variable
# the image sets or recommends is a name in `weft-server/src/settings.rs`'s registry: PAC-5b's
# strict configuration stops the server on an unregistered `WEFT_*`, so nothing here invents
# one.
#
# Single architecture, on purpose. The builder compiles for its own architecture, so the image
# matches the host that built it. The multi-arch image is PAC-10, which also decides the
# registry; this slice builds and proves one image and publishes none.

# The build baseline. This is the same image, tag and digest as the x86_64 Linux leg of
# .github/workflows/release.yml: the glibc floor of a published binary is a function of this
# line, so a change here is a change to what a release promises.
ARG BUILDER_IMAGE=quay.io/pypa/manylinux_2_28_x86_64:2026.10.25-1@sha256:e2808351a165aa85913d79f1f8457b4f6d74047b47fba07b5b9a5e6c0055a017

# The runtime base: Debian 12 (glibc 2.36, well above the 2.28 the binary is linked against).
# Pinned by digest for the same reason the builder is: the running binary's environment is part
# of what was tested.
ARG RUNTIME_IMAGE=docker.io/library/debian:12-slim@sha256:99c0b25c46d0214d5a2091092e7908907dcaecd6a962cde0ac700acee222b6f3

# The uid and gid the server runs as. 10001 is above the range `useradd` allocates to a host's
# users on Debian, so a bind mount created by the container's own uid cannot collide with a
# host user's files the way uid 1000 so often does.
ARG WEFT_UID=10001
# The Rust target triple for cross-compilation. Defaults to x86_64; override for arm64 builds.
ARG RUST_TARGET=x86_64-unknown-linux-gnu

# ---------------------------------------------------------------------------
# builder: the release toolchain, in the release baseline
# ---------------------------------------------------------------------------
FROM ${BUILDER_IMAGE} AS builder

ARG RUST_TARGET=x86_64-unknown-linux-gnu

# The dependency tree needs cmake and nasm beyond the baseline's cc — the same two packages the
# release leg installs, for the same reason (see release.yml, "Install the build tools the
# baseline image lacks").
RUN dnf install -y cmake nasm curl && dnf clean all

# The release compiler is the one CI's stable jobs test, pinned in one file. Copy the file
# alone first so a source edit does not re-download a toolchain.
COPY .github/release-toolchain /tmp/release-toolchain
# PATH is extended, never replaced: the baseline puts its gcc-toolset first, and that is the
# `cc` every C dependency must see. Replacing PATH fell back to the system gcc 8, which cannot
# compile any of simsimd's aarch64 backends (no arm_sve.h, no bf16/i8mm target pragmas), so its
# build script produced no library and the arm64 image failed to link `-lsimsimd`.
ENV CARGO_HOME=/usr/local/cargo \
	RUSTUP_HOME=/usr/local/rustup \
	PATH=/usr/local/cargo/bin:${PATH}
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path \
		--profile minimal --default-toolchain "$(tr -d '[:space:]' < /tmp/release-toolchain)" \
		--target "${RUST_TARGET}" \
	&& rustc --version && cargo --version

WORKDIR /src
COPY . .

# One package, one binary: the image is a server, so it carries `weft-server` and not the TUI
# or the bench harness. `--profile dist` is the archive profile (stripped, LTO, no debug).
#
# The cargo caches are BuildKit cache mounts, so a rebuild after a source edit reuses the
# compiled dependency tree instead of paying the full build again. They live outside the image,
# which is also why the image never contains a `target` directory.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
	--mount=type=cache,target=/usr/local/cargo/git \
	--mount=type=cache,target=/src/target \
	set -eu; \
	cargo build --locked --profile dist -p weft-server --bin weft-server; \
	# The target directory is a cache mount, so the binary has to be copied out of it into the
	# image layer while the mount is still visible.
	cp target/dist/weft-server /weft-server; \
	test -x /weft-server; \
	/weft-server --help >/dev/null

# ---------------------------------------------------------------------------
# runtime: the server, a non-root user, and nothing else
# ---------------------------------------------------------------------------
FROM ${RUNTIME_IMAGE} AS runtime

ARG WEFT_UID=10001
ARG VERSION=dev
ARG REVISION=
ARG CREATED=

# A system user with no login shell, owning the data directory. The store creates its root when
# it is missing and probes it for write access before opening (weft-server/store_open.rs), so
# the one thing the directory must be is writable by this uid — which is why the chown is here
# and why a named volume inherits it (Docker copies the image's directory ownership into a fresh
# volume) while a bind mount must already be owned or world-writable.
RUN set -eux; \
	groupadd --gid "${WEFT_UID}" --system weft; \
	useradd --uid "${WEFT_UID}" --gid "${WEFT_UID}" --system \
		--home-dir /var/lib/weftdb --shell /usr/sbin/nologin weft; \
	install --directory --owner weft --group weft --mode 0750 /var/lib/weftdb

COPY --from=builder /weft-server /usr/local/bin/weft-server
# Prove the runtime can actually load the binary: a missing shared library is an image-build
# failure here, not a container that restarts forever on someone's host. `keygen --help` is the
# cheapest subcommand that exercises the real loader and parser and writes nothing.
RUN /usr/local/bin/weft-server keygen --help >/dev/null
# The health check lives in a file rather than inline because a Dockerfile `CMD` cannot do shell
# parameter substitution past the parser, and the check has to read the bind address.
COPY --from=builder /src/packaging/docker/healthcheck.sh /usr/local/bin/weft-health
RUN chmod 0755 /usr/local/bin/weft-health
# Apache-2.0 section 4, in the same shape the release archives carry it (scripts/release/
# stage.sh's DOCS list): the notices for the code adapted from Chimp and DDSketch, plus the
# dependency licence bundle generated by scripts/licenses/generate.sh.
COPY --from=builder /src/NOTICE /usr/share/doc/weftdb/NOTICE
COPY --from=builder /src/THIRD-PARTY-LICENSES.md /usr/share/doc/weftdb/THIRD-PARTY-LICENSES.md
COPY --from=builder /src/weft-physical-type/THIRD-PARTY-NOTICES /usr/share/doc/weftdb/THIRD-PARTY-NOTICES-weft-physical-type
COPY --from=builder /src/weft-reduce/THIRD-PARTY-NOTICES /usr/share/doc/weftdb/THIRD-PARTY-NOTICES-weft-reduce
COPY --from=builder /src/LICENSE-APACHE /usr/share/doc/weftdb/LICENSE-APACHE
COPY --from=builder /src/LICENSE-MIT /usr/share/doc/weftdb/LICENSE-MIT

# The data directory: the store root plus the backups the scheduled daemon writes
# (WEFT_BACKUP_DIR defaults to `<store root>/backups`). Declaring it a volume is what makes
# `docker run -v weftdb-data:/var/lib/weftdb` the whole persistence story.
VOLUME ["/var/lib/weftdb"]

# 0.0.0.0 because loopback in a container is unreachable from outside it; see the header for
# what that obligates the deployment to bring (a keys file, or a gateway + WEFT_AUTH=disabled).
ENV WEFT_DATA_DIR=/var/lib/weftdb \
	WEFT_SEGMENT_STORE_ROOT=/var/lib/weftdb/store \
	WEFT_SERVER_ADDR=0.0.0.0:8080 \
	HOME=/var/lib/weftdb
EXPOSE 8080

USER ${WEFT_UID}:${WEFT_UID}

# The health check asks the probe endpoints, which are `Scope::Probe`: they answer without a key
# (SEC-6's anonymous role) and are never shed by admission (ROB-10), which is exactly what a
# health check needs. `/health` says the process is up; `/ready` says the store opened, so a
# container that is starting but not yet serving reads unhealthy. The port is taken from
# WEFT_SERVER_ADDR so an operator who rebinds the server does not silently disable the check.
# `start-period` covers the first-run case: an empty volume means the store is created and
# checked before the first request can be answered.
#
# One limitation, stated where it will be read: the probe speaks plain HTTP, so a container
# configured with WEFT_TLS_CERT_FILE serves TLS only and reads unhealthy. See the header of
# packaging/docker/healthcheck.sh for why, and for what to do instead.
HEALTHCHECK --interval=30s --timeout=3s --start-period=20s --retries=3 \
	CMD ["/usr/local/bin/weft-health"]

# SIGTERM is what `docker stop` sends, and the serve path handles it: drain live connections for
# the shutdown grace (WEFT_SHUTDOWN_GRACE_SECS, C9/D-S14), then drain tasks, then close the
# store. A kill -9 on a container is survivable too — the store's crash recovery settles any
# ambiguous commit on the next open — but a graceful stop is the documented way out.
STOPSIGNAL SIGTERM

ENTRYPOINT ["/usr/local/bin/weft-server"]

# No arguments is `serve`. Every other subcommand is reachable by passing its word
# (`docker run --rm weftdb:local keygen --help`), which is how the smoke test mints the keys
# file it then mounts.
CMD []

LABEL org.opencontainers.image.title="weftdb" \
	org.opencontainers.image.description="WeftDB: a time-series store for change streams. The weft-server container image." \
	org.opencontainers.image.vendor="basic-automation" \
	org.opencontainers.image.source="https://github.com/basic-automation/weftdb" \
	org.opencontainers.image.documentation="https://github.com/basic-automation/weftdb/blob/main/README.md" \
	org.opencontainers.image.licenses="Apache-2.0 OR MIT" \
	org.opencontainers.image.version="${VERSION}" \
	org.opencontainers.image.revision="${REVISION}" \
	org.opencontainers.image.created="${CREATED}" \
	ai.weftdb.role="server" \
	ai.weftdb.glibc-floor="2.28"
