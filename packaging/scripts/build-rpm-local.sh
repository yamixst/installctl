#!/usr/bin/env bash
set -euo pipefail

# Prepend ~/.cargo/bin to PATH if present
if [ -d "$HOME/.cargo/bin" ] && [[ ":$PATH:" != *":$HOME/.cargo/bin:"* ]]; then
    export PATH="$HOME/.cargo/bin:$PATH"
fi

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

# 1. Extract package name and version from Cargo.toml
if [ ! -f "$REPO_ROOT/Cargo.toml" ]; then
    echo "Error: Cargo.toml not found at $REPO_ROOT/Cargo.toml" >&2
    exit 1
fi

NAME=$(grep -m1 '^name *= *' "$REPO_ROOT/Cargo.toml" | sed -E 's/^name *= *["'"'"']([^"'"'"']+)["'"'"'].*/\1/')
VERSION=$(grep -m1 '^version *= *' "$REPO_ROOT/Cargo.toml" | sed -E 's/^version *= *["'"'"']([^"'"'"']+)["'"'"'].*/\1/')

if [ -z "$NAME" ] || [ -z "$VERSION" ]; then
    echo "Error: Failed to extract package name or version from Cargo.toml" >&2
    exit 1
fi

# 2. Dependency validation
missing_deps=()

for cmd in cargo rustc rpmbuild tar gzip pkg-config; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        missing_deps+=("$cmd")
    fi
done

if command -v pkg-config >/dev/null 2>&1; then
    if ! pkg-config --exists fuse3; then
        missing_deps+=("fuse3 headers (pkg-config fuse3)")
    fi
else
    if [ ! -f /usr/include/fuse3/fuse.h ]; then
        missing_deps+=("fuse3 headers (/usr/include/fuse3/fuse.h)")
    fi
fi

if [ ${#missing_deps[@]} -gt 0 ]; then
    echo "Error: Missing required build dependencies:" >&2
    for dep in "${missing_deps[@]}"; do
        echo "  - $dep" >&2
    done
    echo "" >&2
    echo "Install missing dependencies with:" >&2
    echo "  Enterprise Linux (EL9):" >&2
    echo "    sudo dnf install -y rpm-build gcc pkgconf-pkg-config fuse3-devel" >&2
    echo "  Debian/Ubuntu:" >&2
    echo "    sudo apt-get update && sudo apt-get install -y rpm rpm2cpio pkg-config libfuse3-dev" >&2
    exit 1
fi

SPEC_FILE="$REPO_ROOT/packaging/rpm/${NAME}.spec"
if [ ! -f "$SPEC_FILE" ]; then
    echo "Error: RPM spec file not found at $SPEC_FILE" >&2
    exit 1
fi

# 3. Destination directory and isolated temporary directory
mkdir -p "$REPO_ROOT/packaging/dist"

RPMBUILD_DIR="$(mktemp -d /tmp/installctl-rpmbuild.XXXXXX)"
BUILD_SUCCESS=0
MOVED_RPMS=()

cleanup() {
    local exit_code=$?
    trap - EXIT INT TERM HUP
    if [ -n "$RPMBUILD_DIR" ] && [ -d "$RPMBUILD_DIR" ]; then
        rm -rf "$RPMBUILD_DIR"
    fi
    if [ "$exit_code" -ne 0 ] || [ "$BUILD_SUCCESS" -ne 1 ]; then
        for rpm_file in "${MOVED_RPMS[@]}"; do
            if [ -f "$rpm_file" ]; then
                rm -f "$rpm_file"
            fi
        done
    fi
    exit "$exit_code"
}
trap cleanup EXIT INT TERM HUP

# 4. RPM directory tree
mkdir -p "$RPMBUILD_DIR"/{BUILD,RPMS,SOURCES,SPECS,SRPMS}

# 5. Create source tarball
echo "Creating source archive ${NAME}-${VERSION}.tar.gz..."
tar -czf "$RPMBUILD_DIR/SOURCES/${NAME}-${VERSION}.tar.gz" \
    --transform "s,^\.,${NAME}-${VERSION}," \
    --exclude="target" \
    --exclude="./target" \
    --exclude="target-docker" \
    --exclude="./target-docker" \
    --exclude="packaging/dist" \
    --exclude="./packaging/dist" \
    --exclude=".git" \
    --exclude="./.git" \
    -C "$REPO_ROOT" .

# 6. Copy spec file
cp "$SPEC_FILE" "$RPMBUILD_DIR/SPECS/${NAME}.spec"

# 7. Execute rpmbuild
echo "Building RPM package for ${NAME}-${VERSION}..."
rpmbuild -bb \
    --define "_topdir $RPMBUILD_DIR" \
    --nodeps \
    "$RPMBUILD_DIR/SPECS/${NAME}.spec"

# 8. Collect and move generated RPMs
shopt -s nullglob
rpm_files=("$RPMBUILD_DIR"/RPMS/*/*.rpm)
if [ ${#rpm_files[@]} -eq 0 ]; then
    echo "Error: No .rpm files generated in $RPMBUILD_DIR/RPMS" >&2
    exit 1
fi

for f in "${rpm_files[@]}"; do
    dest="$REPO_ROOT/packaging/dist/${f##*/}"
    MOVED_RPMS+=("$dest")
    cp "$f" "$dest"
done

# 9. Find main RPM package (non-debuginfo)
MAIN_RPM=""
for rpm_file in "${MOVED_RPMS[@]}"; do
    case "${rpm_file##*/}" in
        *-debuginfo-*.rpm|*-debugsource-*.rpm)
            ;;
        *)
            MAIN_RPM="$rpm_file"
            ;;
    esac
done

if [ -z "$MAIN_RPM" ]; then
    MAIN_RPM="${MOVED_RPMS[0]}"
fi

# 10. Verify generated RPM package if rpm command is available
if command -v rpm >/dev/null 2>&1; then
    RPM_QUERY_ARGS=()
    if [ ! -w "/var/lib/rpm" ]; then
        RPM_QUERY_ARGS+=(--define "_dbpath /tmp")
    fi

    echo ""
    echo "=== Package Information ==="
    rpm "${RPM_QUERY_ARGS[@]}" -qpi "$MAIN_RPM"

    echo ""
    echo "=== Package Contents ==="
    rpm "${RPM_QUERY_ARGS[@]}" -qpl "$MAIN_RPM"

    rpm_info=$(rpm "${RPM_QUERY_ARGS[@]}" -qpi "$MAIN_RPM")
    if ! echo "$rpm_info" | grep -q "^Name[[:space:]]*:[[:space:]]*${NAME}"; then
        echo "Error: Package name in RPM does not match '${NAME}'" >&2
        exit 1
    fi

    rpm_files_list=$(rpm "${RPM_QUERY_ARGS[@]}" -qpl "$MAIN_RPM")
    for req_path in "/usr/bin/${NAME}" "/usr/share/doc/${NAME}/README.md"; do
        if ! echo "$rpm_files_list" | grep -q "^${req_path}$"; then
            echo "Error: Generated RPM is missing expected path: $req_path" >&2
            exit 1
        fi
    done
fi

BUILD_SUCCESS=1

echo ""
echo "=== Build Successful ==="
echo "Package(s) built and verified at:"
for rpm_file in "${MOVED_RPMS[@]}"; do
    echo "  $rpm_file"
done
