# Runtime image for latticed and the lattice CLI. The release workflow builds the binaries once per
# architecture and copies them in, so the image holds exactly what the release archives hold.
# Build context layout: bin/amd64/{latticed,lattice} and bin/arm64/{latticed,lattice}.
FROM ubuntu:24.04@sha256:24737cef17f4e22b1977df7d9a05e77c4e28b7da5b8911f8d8ccda136bf3573c

ARG TARGETARCH
RUN useradd --system --uid 10001 --home-dir /var/lib/lattice --create-home lattice
COPY --chmod=0755 bin/${TARGETARCH}/latticed bin/${TARGETARCH}/lattice /usr/local/bin/
USER lattice
WORKDIR /var/lib/lattice
VOLUME /var/lib/lattice
ENTRYPOINT ["/usr/local/bin/latticed"]
