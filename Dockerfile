# Runtime image for latticed and the lattice CLI. The release workflow builds the binaries once per
# architecture and copies them in, so the image holds exactly what the release archives hold.
# Build context layout: bin/amd64/{latticed,lattice} and bin/arm64/{latticed,lattice}.
FROM ubuntu:25.10@sha256:7cc5e35f6567ee8c66d2abb4aab0fd866669e6207c237c3a8f0947a5c7f17092

ARG TARGETARCH
RUN useradd --system --uid 10001 --home-dir /var/lib/lattice --create-home lattice
COPY --chmod=0755 bin/${TARGETARCH}/latticed bin/${TARGETARCH}/lattice /usr/local/bin/
USER lattice
WORKDIR /var/lib/lattice
VOLUME /var/lib/lattice
ENTRYPOINT ["/usr/local/bin/latticed"]
