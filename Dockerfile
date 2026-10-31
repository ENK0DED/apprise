# Multi-stage build for minimal Docker image
FROM rust:1.96-alpine AS builder

RUN apk add --no-cache musl-dev perl make gcc g++ upx

WORKDIR /build
COPY . .
# The image carries every non-desktop feature, PGP and AMQP 1.0 included
RUN cargo build --release -p apprise-cli --features pgp,amqp && \
    upx --best target/release/apprise

# Runtime image
FROM alpine:3.24

# tzdata gives ?tz= and the asset's timezone their named zones
RUN apk add --no-cache ca-certificates tzdata

COPY --from=builder /build/target/release/apprise /usr/local/bin/apprise
COPY LICENSE /usr/share/licenses/apprise/LICENSE

# Default environment variables (matching Python apprise)
ENV APPRISE_STORAGE_PATH=/data/cache

ENTRYPOINT ["apprise"]
