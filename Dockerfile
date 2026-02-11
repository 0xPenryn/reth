# syntax=docker.io/docker/dockerfile:1.7-labs

FROM lukemathwalker/cargo-chef:latest-rust-1 AS chef
WORKDIR /app

LABEL org.opencontainers.image.source=https://github.com/paradigmxyz/reth
LABEL org.opencontainers.image.licenses="MIT OR Apache-2.0"

# Install system dependencies
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates curl gnupg gpgv lsb-release pkg-config; \
    if ! apt-get install -y --no-install-recommends clang-21 llvm-21 llvm-21-dev libclang-21-dev libpolly-21-dev; then \
        install -d /etc/apt/keyrings; \
        curl -fsSL https://apt.llvm.org/llvm-snapshot.gpg.key | gpg --dearmor -o /etc/apt/keyrings/llvm-archive-keyring.gpg; \
        codename="$(. /etc/os-release && echo "${VERSION_CODENAME}")"; \
        echo "deb [signed-by=/etc/apt/keyrings/llvm-archive-keyring.gpg] https://apt.llvm.org/${codename}/ llvm-toolchain-${codename}-21 main" > /etc/apt/sources.list.d/llvm.list; \
        apt-get -o APT::Key::gpgvcommand=/usr/bin/gpgv update; \
        apt-get install -y --no-install-recommends clang-21 llvm-21 llvm-21-dev libclang-21-dev libpolly-21-dev; \
    fi; \
    rm -rf /var/lib/apt/lists/*
ENV LLVM_SYS_210_PREFIX=/usr/lib/llvm-21
ENV LLVM_SYS_180_PREFIX=/usr/lib/llvm-21
ENV LLVM_CONFIG_PATH=/usr/lib/llvm-21/bin/llvm-config
ENV LIBCLANG_PATH=/usr/lib/llvm-21/lib
ENV PATH="/usr/lib/llvm-21/bin:${PATH}"

# Builds a cargo-chef plan
FROM chef AS planner
COPY --exclude=.git --exclude=dist . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json

# Build profile, release by default
ARG BUILD_PROFILE=maxperf
ENV BUILD_PROFILE=$BUILD_PROFILE

# Extra Cargo flags
ARG RUSTFLAGS=""
ENV RUSTFLAGS="$RUSTFLAGS"

# Extra Cargo features
ARG FEATURES=""
ENV FEATURES=$FEATURES

# Builds dependencies
RUN cargo chef cook --profile $BUILD_PROFILE --features "$FEATURES" --recipe-path recipe.json

# Build application
COPY --exclude=dist . .
RUN cargo build --profile $BUILD_PROFILE --features "$FEATURES" --locked --bin reth

# ARG is not resolved in COPY so we have to hack around it by copying the
# binary to a temporary location
RUN cp /app/target/$BUILD_PROFILE/reth /app/reth

# Use Ubuntu as the release image
FROM ubuntu AS runtime
WORKDIR /app

# Copy reth over from the build stage
COPY --from=builder /app/reth /usr/local/bin

# Copy licenses
COPY LICENSE-* ./

EXPOSE 30303 30303/udp 9001 8545 8546
ENTRYPOINT ["/usr/local/bin/reth"]
