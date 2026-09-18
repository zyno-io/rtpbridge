# Pin Rust version to match rust-version in Cargo.toml
ARG BUILD_VERSION=canary-unknown
FROM rust:1.94-trixie AS builder
ARG BUILD_VERSION
ENV BUILD_VERSION=${BUILD_VERSION}

RUN apt-get update && apt-get install -y --no-install-recommends libopus-dev && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY scripts/build-openssl.sh /tmp/build-openssl.sh
RUN sh /tmp/build-openssl.sh /opt/rtpbridge-openssl
ENV OPENSSL_DIR=/opt/rtpbridge-openssl OPENSSL_STATIC=1
COPY Cargo.toml Cargo.lock build.rs ./
# Create stubs so Cargo.toml parses (benches/ excluded by .dockerignore)
RUN mkdir -p src benches \
    && echo 'fn main() {}' > src/main.rs \
    && echo 'fn main() {}' > benches/codec_bench.rs \
    && echo 'fn main() {}' > benches/srtp_bench.rs \
    && echo 'fn main() {}' > benches/routing_bench.rs \
    && echo 'fn main() {}' > benches/pcap_bench.rs \
    && echo 'fn main() {}' > benches/mixing_bench.rs \
    && cargo build --release \
    && rm -rf src

COPY . .
RUN touch src/main.rs && cargo build --release

# Prepare the one native library not supplied by the distroless C/C++ base.
# Preserve dpkg metadata so image scanners can identify its exact version.
FROM debian:trixie-slim AS runtime-files
RUN apt-get update && apt-get install -y --no-install-recommends libopus0 && rm -rf /var/lib/apt/lists/* \
    && dpkg-query -s libopus0 > /libopus-status \
    && mkdir -p /var/lib/rtpbridge/recordings /var/lib/rtpbridge/media /var/lib/rtpbridge/cache

FROM gcr.io/distroless/cc-debian13:nonroot
ARG BUILD_VERSION
LABEL org.opencontainers.image.version="${BUILD_VERSION}"
LABEL org.opencontainers.image.openssl.version="3.6.4"
COPY --from=runtime-files /usr/lib/*-linux-gnu/libopus.so.0* /usr/lib/
COPY --from=runtime-files /libopus-status /var/lib/dpkg/status.d/libopus0
COPY --from=runtime-files --chown=65532:65532 /var/lib/rtpbridge /var/lib/rtpbridge
COPY --from=builder /app/target/release/rtpbridge /usr/local/bin/rtpbridge
EXPOSE 9100
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/rtpbridge"]
