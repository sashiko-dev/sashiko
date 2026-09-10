# Stage 1: Build
FROM rust:1.88-bookworm AS builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
    clang \
    lld \
    pkg-config \
    libssl-dev \
    wget \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src/sashiko
COPY . .

## Download Linux kernel bundle (~2.5GB) during build time
## This makes the image large but ensures fast startup in Cloud Run
#RUN wget -c https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/clone.bundle -O /usr/src/sashiko/linux-kernel.bundle

# Build for release
RUN cargo build --release

# Stage 2: Runtime
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    procps \
    git \
    libssl3 \
    ca-certificates \
    wget \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Configure Git to rewrite git:// to https:// for hosts known to support Smart HTTP
RUN git config --global url."https://git.kernel.org/".insteadOf "git://git.kernel.org/" && \
    git config --global url."https://github.com/".insteadOf "git://github.com/" && \
    git config --global url."https://gitlab.com/".insteadOf "git://gitlab.com/" && \
    git config --global url."https://gitlab.freedesktop.org/".insteadOf "git://gitlab.freedesktop.org/"


WORKDIR /app

# Copy binaries from builder
COPY --from=builder /usr/src/sashiko/target/release/sashiko /usr/local/bin/sashiko
COPY --from=builder /usr/src/sashiko/target/release/sashiko-cli /usr/local/bin/sashiko-cli

## Copy the pre-downloaded kernel bundle
#COPY --from=builder /usr/src/sashiko/linux-kernel.bundle /opt/linux-kernel.bundle

# Copy default settings and assets
COPY Settings.toml /app/Settings.toml
COPY sashiko.dev/email_policy.toml /app/email_policy.toml
COPY third_party/prompts /app/third_party/prompts
COPY static /app/static

# Copy entrypoint script
COPY scripts/docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

# Data directory for database, worktrees and logs
RUN mkdir -p /data/db /data/logs /tmp/sashiko_worktrees /app/third_party/linux

# Default environment variables
ENV SASHIKO__DATABASE__URL=/data/db/sashiko.db \
    SASHIKO__GIT__REPOSITORY_PATH=/app/third_party/linux \
    SASHIKO__REVIEW__WORKTREE_DIR=/tmp/sashiko_worktrees \
    SASHIKO__SERVER__HOST=0.0.0.0 \
    SASHIKO__SERVER__PORT=8080

EXPOSE 8080

ENTRYPOINT ["docker-entrypoint.sh"]
