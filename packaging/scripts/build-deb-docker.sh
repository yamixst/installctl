#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

# 1. Verify docker is available
if ! command -v docker >/dev/null 2>&1; then
    echo "Error: docker command not found. Please install Docker." >&2
    exit 1
fi

# 2. Build Docker builder image
echo "Building Docker image installctl-builder:noble..."
docker build -t installctl-builder:noble -f "$REPO_ROOT/packaging/docker/Dockerfile.deb" "$REPO_ROOT/packaging/docker"

# 3. Run packaging build inside container
echo ""
echo "Running packaging build inside Ubuntu 24.04 container..."
docker run --rm \
    --user "$(id -u):$(id -g)" \
    -e HOME=/tmp \
    -v "$REPO_ROOT":/build/installctl \
    -w /build/installctl \
    installctl-builder:noble \
    bash packaging/scripts/build-deb-local.sh

# 4. Verify generated package ownership on host
echo ""
echo "=== Docker Build Verification ==="
shopt -s nullglob
dist_debs=("$REPO_ROOT"/packaging/dist/*.deb)
if [ ${#dist_debs[@]} -eq 0 ]; then
    echo "Error: No .deb file found in $REPO_ROOT/packaging/dist" >&2
    exit 1
fi

expected_uid="$(id -u)"
expected_gid="$(id -g)"

for deb in "${dist_debs[@]}"; do
    deb_uid=$(stat -c '%u' "$deb")
    deb_gid=$(stat -c '%g' "$deb")
    if [ "$deb_uid" -ne "$expected_uid" ] || [ "$deb_gid" -ne "$expected_gid" ]; then
        echo "Error: $deb ownership ($deb_uid:$deb_gid) does not match current host user ($expected_uid:$expected_gid)" >&2
        exit 1
    fi
    echo "Verified: $deb is owned by host user ($deb_uid:$deb_gid)"
done

echo ""
echo "Docker build completed successfully."
