FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

# Install build dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    fakeroot \
    curl \
    ca-certificates \
    debhelper \
    dpkg-dev \
    libfuse3-dev \
    pkg-config \
    uidmap \
    sudo \
    && rm -rf /var/lib/apt/lists/*

# Install modern Rust stable toolchain via rustup
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:${PATH}

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal \
    && chmod -R a+w /usr/local/cargo /usr/local/rustup \
    && chmod -R a+rwX /usr/local/cargo /usr/local/rustup

# Allow unprivileged user to execute sudo without password (needed to configure parent dir permissions at container launch)
RUN echo "ALL ALL=(ALL:ALL) NOPASSWD: ALL" > /etc/sudoers.d/builder && chmod 0440 /etc/sudoers.d/builder

# Create entrypoint to ensure root directory has sticky permissions (1777)
# for dpkg-buildpackage which outputs artifacts to the parent directory (..)
RUN printf '#!/bin/sh\nset -e\nif command -v sudo >/dev/null 2>&1; then\n    sudo chmod 1777 / 2>/dev/null || true\nfi\nexec "$@"\n' > /entrypoint.sh \
    && chmod 0755 /entrypoint.sh

# Prepare /work directory
RUN mkdir -p /work && chmod 777 /work

WORKDIR /work

ENTRYPOINT ["/entrypoint.sh"]
