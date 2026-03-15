# Multi-stage build for minimal Docker image
FROM rust:1.85-alpine AS builder

RUN apk add --no-cache musl-dev perl make cmake gcc g++

WORKDIR /build
COPY . .
RUN cargo build --release -p apprise-cli

# Runtime image
FROM alpine:3.21

RUN apk add --no-cache ca-certificates

COPY --from=builder /build/target/release/apprise /usr/local/bin/apprise

# Default environment variables (matching Python apprise)
ENV APPRISE_STORAGE_PATH=/data/cache

ENTRYPOINT ["apprise"]
