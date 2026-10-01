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
    && rm -rf /var/lib/apt/lists/*

# Install modern Rust stable toolchain via rustup
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:${PATH}

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal \
    && chmod -R a+rwX /usr/local/cargo /usr/local/rustup

# Dedicated world-writable build directory for arbitrary unprivileged UIDs
# dpkg-buildpackage will write intermediate artifacts into parent dir (/build)
RUN mkdir -p /build && chmod 1777 /build

WORKDIR /build/installctl
