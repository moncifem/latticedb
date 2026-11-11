# Runtime image for latticed and the lattice CLI. The release workflow builds the binaries once per
# architecture and copies them in, so the image holds exactly what the release archives hold.
# Build context layout: bin/amd64/{latticed,lattice} and bin/arm64/{latticed,lattice}, plus docker/.
# The image runs as the unprivileged user `lattice` (uid 10001). Its data directory and its
# credential directory are created owned by that user, so a new named volume starts writable.
# See docs/alpha.md#run-in-a-container for the commands; tests/image/smoke.sh runs them in CI.
FROM ubuntu:26.04@sha256:6a06111c3b0a8600feadb270aec771dc239bf407b25625f93f44ab00e11f4d3c

ARG TARGETARCH
RUN groupadd --system --gid 10001 lattice \
 && useradd --system --uid 10001 --gid 10001 --home-dir /var/lib/lattice --create-home lattice \
 && install -d -o lattice -g lattice -m 0700 /var/lib/lattice /etc/lattice
COPY --chmod=0755 bin/${TARGETARCH}/latticed bin/${TARGETARCH}/lattice /usr/local/bin/
COPY --chmod=0755 docker/entrypoint.sh /usr/local/bin/latticed-entrypoint
USER 10001:10001
WORKDIR /var/lib/lattice
VOLUME /var/lib/lattice
EXPOSE 5432
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/latticed-entrypoint"]
