# syntax=docker/dockerfile:1
# Build a static musl binary, then ship it on the empty scratch image.

# Build stage: Rust toolchain on Alpine (musl libc).
FROM rust:1.99.1-alpine AS build
WORKDIR /build
# Manifests first, so dependency layers can be reused when only sources change.
COPY Cargo.toml Cargo.lock ./
# Sources, embedded assets, and migrations are needed at build time.
COPY src ./src
COPY assets ./assets
COPY migrations ./migrations
COPY README.md LICENSE reseller.example.toml ./
# Cache the cargo registry and target directory between builds, one cache per
# platform and one build at a time: a multi-arch build runs its platforms in
# parallel, and two of them unpacking into one registry fail ("File exists").
# --locked pins the versions recorded in Cargo.lock.
ARG TARGETPLATFORM
RUN --mount=type=cache,id=cargo-registry-$TARGETPLATFORM,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=cargo-target-$TARGETPLATFORM,target=/build/target,sharing=locked \
    cargo build --locked --release && cp target/release/reseller /reseller
# Create the data directory for the non-root runtime user (UID 10001).
RUN mkdir -p /data && chown 10001:10001 /data

# Runtime stage: no shell, package manager, or other files beyond the binary.
FROM scratch
LABEL org.opencontainers.image.source="https://github.com/reseller-rs/reseller" \
      org.opencontainers.image.description="OpenAI-compatible API reseller platform" \
      org.opencontainers.image.licenses="MIT"
# TLS roots for upstream HTTPS. The data directory must be writable by the runtime user.
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build --chown=10001:10001 /data /data
COPY --from=build /reseller /usr/local/bin/reseller
# Run as a non-root user with the data directory as the working directory.
USER 10001:10001
WORKDIR /data
# Runtime defaults. Override them with docker run -e or in compose.yaml.
ENV PATH=/usr/local/bin HOST=0.0.0.0 PORT=56787 DB_FILE=/data/reseller.db
# Documents the HTTP port. Publish it with -p or the ports key.
EXPOSE 56787
VOLUME ["/data"]
# Probe the server with the built-in healthcheck subcommand, because scratch has no curl.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/usr/local/bin/reseller","healthcheck"]
# Start the server by default. Other subcommands, such as init or config, can be passed instead.
ENTRYPOINT ["reseller"]
CMD ["serve"]
