FROM rust:1.88-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN cargo build --locked --release --bin nenia

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 nenia \
    && useradd --uid 10001 --gid nenia --no-create-home --home-dir /data nenia \
    && mkdir /data \
    && chown nenia:nenia /data
COPY --from=builder /build/target/release/nenia /usr/local/bin/nenia
USER 10001:10001
ENV HOME=/data
EXPOSE 7433
ENTRYPOINT ["/usr/local/bin/nenia"]
CMD ["--vault", "/data", "serve", "--listen", "0.0.0.0:7433"]
