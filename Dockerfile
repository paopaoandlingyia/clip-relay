##############################
# Frontend build (Next export)
##############################
FROM node:20-alpine AS frontend
WORKDIR /app

COPY package.json package-lock.json ./
RUN apk add --no-cache python3 make g++ \
 && npm ci

COPY next.config.ts ./
COPY tsconfig.json ./
COPY postcss.config.mjs ./
COPY eslint.config.mjs ./
COPY components.json ./
COPY public ./public
COPY src ./src

ENV NODE_ENV=production
RUN npm run build && rm -rf .next && npm cache clean --force

COPY scripts ./scripts
RUN node ./scripts/precompress.mjs /app/out --write-br --no-gz

##############################
# Rust build
##############################
FROM rust:1-alpine AS rust-builder
WORKDIR /app

# Install build dependencies (perl for ring crate, used by rustls)
RUN apk add --no-cache musl-dev build-base perl

# Copy root files
COPY rust-server ./rust-server

# We remove Cargo.lock to ensure a fresh, consistent build inside the container
RUN rm -f rust-server/Cargo.lock

# Build the real application
RUN cargo build --manifest-path rust-server/Cargo.toml --release

##############################
# Runtime image (Alpine + Rust server only)
##############################
FROM alpine:3.20 AS runtime
WORKDIR /app

# Install ca-certificates
RUN apk add --no-cache ca-certificates && update-ca-certificates

COPY --chown=0:0 --from=frontend /app/out /app/out
COPY --chown=0:0 --from=rust-builder /app/rust-server/target/release/clip-relay /usr/local/bin/clip-relay

RUN chmod a+rx /usr/local/bin/clip-relay \
 && mkdir -p /app/data /app/data/uploads /app/logs /app/tmp \
 && chgrp -R 0 /app/data /app/logs /app/tmp \
 && chmod -R 0777 /app/data /app/logs \
 && chmod 1777 /app/tmp

ENV RUST_LOG=info \
    STATIC_DIR=/app/out \
    DATA_DIR=/app/data \
    PORT=8087 \
    HOME=/tmp

VOLUME ["/app/data"]

EXPOSE 8087

CMD ["/bin/sh","-c","umask 0002; exec /usr/local/bin/clip-relay"]
