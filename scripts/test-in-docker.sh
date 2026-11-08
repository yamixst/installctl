#!/bin/sh
set -e

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_DIR"

docker build -t installctl-test \
    --build-arg TESTER_UID="$(id -u)" \
    --build-arg TESTER_GID="$(id -g)" \
    -f docker/Dockerfile.test .

docker run --rm --privileged \
    -v "$REPO_DIR:/work" \
    -v installctl-cargo-registry:/home/tester/.cargo/registry \
    -e CARGO_TARGET_DIR=/work/target-docker \
    installctl-test \
    sh -c '[ -e /dev/fuse ] || mknod -m 666 /dev/fuse c 10 229; su tester -c "cd /work && INSTALLCTL_REQUIRE_PRIV_TESTS=1 cargo test $*"' dummy "$@"
