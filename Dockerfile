# Pin Rust version to match rust-version in Cargo.toml
FROM rust:1.89-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends libopus-dev && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
# Create stubs so Cargo.toml parses (benches/ excluded by .dockerignore)
RUN mkdir -p src benches \
    && echo 'fn main() {}' > src/main.rs \
    && echo 'fn main() {}' > benches/codec_bench.rs \
    && echo 'fn main() {}' > benches/srtp_bench.rs \
    && echo 'fn main() {}' > benches/routing_bench.rs \
    && echo 'fn main() {}' > benches/pcap_bench.rs \
    && cargo build --release \
    && rm -rf src

COPY . .
RUN touch src/main.rs && cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends libopus0 libssl3 ca-certificates && rm -rf /var/lib/apt/lists/* \
    && useradd -r -s /sbin/nologin rtpbridge \
    && mkdir -p /var/lib/rtpbridge/recordings /var/lib/rtpbridge/media /var/lib/rtpbridge/cache \
    && chown -R rtpbridge:rtpbridge /var/lib/rtpbridge
COPY --from=builder /app/target/release/rtpbridge /usr/local/bin/rtpbridge
EXPOSE 9100
USER rtpbridge
# Recommended: run with minimal capabilities in production
# docker run --cap-drop=ALL --cap-add=NET_BIND_SERVICE --read-only rtpbridge
# In Kubernetes:
#   securityContext:
#     readOnlyRootFilesystem: true
#     capabilities:
#       drop: [ALL]
#       add: [NET_BIND_SERVICE]  # only if binding to ports < 1024
ENTRYPOINT ["rtpbridge"]
