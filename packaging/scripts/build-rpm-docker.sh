#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

# 1. Verify docker is available
if ! command -v docker >/dev/null 2>&1; then
    echo "Error: docker command not found. Please install Docker." >&2
    exit 1
fi

# 2. Build Docker builder image
echo "Building Docker image installctl-rpm-builder:el9..."
docker build -t installctl-rpm-builder:el9 -f "$REPO_ROOT/packaging/docker/Dockerfile.almalinux9" "$REPO_ROOT/packaging/docker"

# 3. Run packaging build inside container
echo ""
echo "Running packaging build inside AlmaLinux 9 container..."
docker run --rm \
    --user "$(id -u):$(id -g)" \
    -e HOME=/tmp \
    -v "$REPO_ROOT":/build/installctl \
    -w /build/installctl \
    installctl-rpm-builder:el9 \
    bash packaging/scripts/build-rpm-local.sh

# 4. Verify generated package ownership on host
echo ""
echo "=== Docker Build Verification ==="
shopt -s nullglob
dist_rpms=("$REPO_ROOT"/packaging/dist/*.rpm)
if [ ${#dist_rpms[@]} -eq 0 ]; then
    echo "Error: No .rpm file found in $REPO_ROOT/packaging/dist" >&2
    exit 1
fi

expected_uid="$(id -u)"
expected_gid="$(id -g)"

for rpm in "${dist_rpms[@]}"; do
    rpm_uid=$(stat -c '%u' "$rpm")
    rpm_gid=$(stat -c '%g' "$rpm")
    if [ "$rpm_uid" -ne "$expected_uid" ] || [ "$rpm_gid" -ne "$expected_gid" ]; then
        echo "Error: $rpm ownership ($rpm_uid:$rpm_gid) does not match current host user ($expected_uid:$expected_gid)" >&2
        exit 1
    fi
    echo "Verified: $rpm is owned by host user ($rpm_uid:$rpm_gid)"
done

echo ""
echo "Docker RPM build completed successfully."
