# syntax=docker/dockerfile:1
# Build a static musl binary, then ship it on the empty scratch image.
FROM rust:1.98.1-alpine AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY assets ./assets
COPY migrations ./migrations
COPY README.md LICENSE ./
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --locked --release && cp target/release/reseller /reseller
RUN mkdir -p /data && chown 10001:10001 /data

FROM scratch
LABEL org.opencontainers.image.source="https://github.com/reseller-rs/reseller" \
      org.opencontainers.image.description="OpenAI-compatible API reseller platform" \
      org.opencontainers.image.licenses="MIT"
# TLS roots for upstream HTTPS; the data directory must be writable by the runtime user.
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build --chown=10001:10001 /data /data
COPY --from=build /reseller /usr/local/bin/reseller
USER 10001:10001
WORKDIR /data
ENV PATH=/usr/local/bin HOST=0.0.0.0 PORT=56787 DB_FILE=/data/reseller.db
EXPOSE 56787
VOLUME ["/data"]
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/usr/local/bin/reseller","healthcheck"]
ENTRYPOINT ["reseller"]
CMD ["serve"]
