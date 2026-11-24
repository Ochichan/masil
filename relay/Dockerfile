FROM rust:1.96-bookworm AS builder
WORKDIR /src
COPY crates/masil-wire ./crates/masil-wire
COPY relay ./relay
RUN cargo build --locked --release --manifest-path relay/Cargo.toml

FROM gcr.io/distroless/cc-debian12
COPY --from=builder /src/relay/target/release/masil-relay /usr/local/bin/masil-relay
ENV MASIL_RELAY_LISTEN=0.0.0.0:8787
EXPOSE 8787
ENTRYPOINT ["/usr/local/bin/masil-relay"]
