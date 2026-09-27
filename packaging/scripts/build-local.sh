#!/usr/bin/env bash
set -euo pipefail

# Ensure ~/.cargo/bin is in PATH if available
if [ -d "$HOME/.cargo/bin" ] && [[ ":$PATH:" != *":$HOME/.cargo/bin:"* ]]; then
    export PATH="$HOME/.cargo/bin:$PATH"
fi

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

# 1. Dependency check
missing_deps=()

for cmd in cargo rustc dpkg-buildpackage pkg-config dh; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        missing_deps+=("$cmd")
    fi
done

if [ ! -f /usr/include/fuse3/fuse.h ]; then
    missing_deps+=("libfuse3-dev (/usr/include/fuse3/fuse.h)")
fi

if [ ${#missing_deps[@]} -gt 0 ]; then
    echo "Error: Missing required build dependencies:" >&2
    for dep in "${missing_deps[@]}"; do
        echo "  - $dep" >&2
    done
    echo "" >&2
    echo "Install missing dependencies with:" >&2
    echo "  sudo apt-get update && sudo apt-get install -y debhelper dpkg-dev pkg-config libfuse3-dev" >&2
    exit 1
fi

# 2. Destination directory
mkdir -p "$REPO_ROOT/packaging/dist"

# Clean up any lingering dpkg artifacts in parent directory from previous failed builds
rm -f "$REPO_ROOT"/../installctl_*.buildinfo \
      "$REPO_ROOT"/../installctl_*.changes \
      "$REPO_ROOT"/../installctl_*.deb \
      "$REPO_ROOT"/../installctl-dbgsym_*

# 3. Symlink and trap handling
created_debian_symlink=0

cleanup() {
    local exit_code=$?
    trap - EXIT INT TERM HUP
    if [ "$created_debian_symlink" -eq 1 ] && [ -L "$REPO_ROOT/debian" ]; then
        rm -f "$REPO_ROOT/debian"
    fi
    # Remove debhelper temporary build artifacts
    rm -rf "$REPO_ROOT/packaging/debian/.debhelper" \
           "$REPO_ROOT/packaging/debian/debhelper-build-stamp" \
           "$REPO_ROOT/packaging/debian/files" \
           "$REPO_ROOT/packaging/debian/installctl.substvars" \
           "$REPO_ROOT/packaging/debian/installctl" \
           "$REPO_ROOT/packaging/debian/tmp"
    # Remove temporary build artifacts from parent directory
    rm -f "$REPO_ROOT"/../installctl_*.buildinfo \
          "$REPO_ROOT"/../installctl_*.changes \
          "$REPO_ROOT"/../installctl-dbgsym_*
    if [ "$exit_code" -ne 0 ]; then
        rm -f "$REPO_ROOT"/../installctl_*.deb
    fi
    exit "$exit_code"
}
trap cleanup EXIT INT TERM HUP

if [ -L "$REPO_ROOT/debian" ]; then
    created_debian_symlink=1
elif [ -e "$REPO_ROOT/debian" ]; then
    echo "Error: $REPO_ROOT/debian already exists and is not a symlink" >&2
    exit 1
else
    ln -s packaging/debian "$REPO_ROOT/debian"
    created_debian_symlink=1
fi

# 4. Run build
echo "Building Debian package for installctl..."
(cd "$REPO_ROOT" && dpkg-buildpackage -b -uc -us -d)

# 5. Move and verify
shopt -s nullglob
deb_files=("$REPO_ROOT"/../installctl_*_*.deb)
if [ ${#deb_files[@]} -eq 0 ]; then
    echo "Error: No .deb file generated in $REPO_ROOT/.." >&2
    exit 1
fi

mv "${deb_files[@]}" "$REPO_ROOT/packaging/dist/"

TARGET_DEB="${deb_files[0]##*/}"
DEB_PATH="$REPO_ROOT/packaging/dist/$TARGET_DEB"

echo ""
echo "=== Package Control Information ==="
dpkg-deb -I "$DEB_PATH"

# Validate package metadata
pkg_info=$(dpkg-deb -I "$DEB_PATH")
if ! echo "$pkg_info" | grep -q "^ Package: installctl"; then
    echo "Error: Package control field 'Package' is not 'installctl'" >&2
    exit 1
fi

for dep in libfuse3-3 uidmap sudo; do
    if ! echo "$pkg_info" | grep "^ Depends:" | grep -q "$dep"; then
        echo "Error: Generated package is missing required dependency in Depends: $dep" >&2
        exit 1
    fi
done

echo ""
echo "=== Package Contents ==="
dpkg-deb -c "$DEB_PATH"

pkg_contents=$(dpkg-deb -c "$DEB_PATH")
for req_path in "usr/bin/installctl" "etc/apparmor.d/installctl" "usr/share/doc/installctl/README.md"; do
    if ! echo "$pkg_contents" | grep -q "$req_path"; then
        echo "Error: Generated package is missing expected path: $req_path" >&2
        exit 1
    fi
done

echo ""
echo "=== Build Successful ==="
echo "Package built and verified at:"
echo "  $DEB_PATH"
