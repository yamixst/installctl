# installctl

`installctl` runs untrusted or opaque third-party software installers (such as `curl ... | sh` scripts) inside an unprivileged user-space sandbox backed by a FUSE-based copy-on-write overlay filesystem.

Nothing touches your real system until you review and approve the changes.

## How it works

1. **Sandboxed Execution**: `installctl` executes the installer command inside an unprivileged user and mount namespace. All file creations, modifications, and deletions are captured in a FUSE overlay filesystem in a temporary staging area.
2. **Action Interception**: Dangerous system commands (package managers, user/group management, service managers) and `sudo` calls are shimmed and intercepted. You are prompted whether to skip them, run them on the host system, or abort.
3. **Interactive Review**: When the installer finishes, `installctl` presents a concise report of all file changes and actions. You can inspect diffs, choose what to apply (all, subset, or reject), and cancel if conflicts are detected.
4. **Privilege-Separated Application**: Changes requiring root permissions are separated from user-owned changes and applied via a minimal privileged helper.
5. **Clean Uninstallation**: `installctl` can generate a self-contained, interactive uninstall script that tracks created files, restores modified files from captured originals, and runs inverse operations for system actions.

## Installation

### From Debian / Ubuntu package (`.deb`)

Download the latest `.deb` package from [Releases](https://github.com/yamixst/installctl/releases) or build it from source (see below), then install it using `apt`:

```sh
sudo apt install ./installctl_*.deb
```

> [!NOTE]
> The Debian package automatically installs required runtime dependencies (`libfuse3-3`, `uidmap`, `sudo`) and configures an AppArmor profile (`/etc/apparmor.d/installctl`) for unprivileged user namespaces on Ubuntu 24.04+.
>
> You only need to ensure subordinate UIDs/GIDs are configured for your user (see [Prerequisites](#prerequisites)).

### Building the Debian package

You can build the Debian package either in an isolated Docker container (recommended for reproducible builds across any Linux distribution) or directly on a Debian/Ubuntu host.

#### Using Docker (Ubuntu 24.04)

Builds the `.deb` package inside an official Ubuntu 24.04 container. Requires Docker:

```sh
bash packaging/scripts/build-deb-docker.sh
```

The resulting package will be verified and written to `packaging/dist/` with ownership matching your host user.

#### Locally

Requires a Debian or Ubuntu system with build dependencies installed:

```sh
# 1. Install build dependencies
sudo apt-get update && sudo apt-get install -y debhelper dpkg-dev pkg-config libfuse3-dev

# 2. Build the package
bash packaging/scripts/build-deb-local.sh
```

The resulting package will be verified and written to `packaging/dist/`.

## Usage

Run any installer command through `installctl`:

```sh
# Typical curl-pipe-to-sh installer
installctl -- "curl -fsSL https://herdr.dev/install.sh | sh"

# Or specifying the program name directly
installctl --name myapp "sh ./install.sh"
```

### Review and Prompts

During and after the installation run:
- **Dangerous Commands**: Prompted as `[s] skip / [r] run for real / [a] abort`
- **Review Menu**:
  - `[a]` Apply all changes
  - `[s]` Select what to apply (exclude ranges)
  - `[d]` Show unified diffs for modified text files
  - `[r]` Reject and change nothing
- **Uninstall Script**: Optionally creates an interactive `uninstall-<name>.sh` (default `~/.local/bin/uninstall-<name>.sh`).

## Prerequisites

`installctl` requires Linux unprivileged user namespaces and FUSE support. Specifically:

1. **/dev/fuse**:
   `/dev/fuse is missing or inaccessible. Ensure the fuse module is loaded and /dev/fuse has read/write permissions.`
2. **newuidmap**:
   `newuidmap executable not found in PATH. Install the uidmap package (e.g. 'apt-get install uidmap').`
3. **newgidmap**:
   `newgidmap executable not found in PATH. Install the uidmap package (e.g. 'apt-get install uidmap').`
4. **/etc/subuid**:
   `No subordinate UID range configured in /etc/subuid for the current user. Configure at least 65535 subuids (e.g. '<user>:100000:65536').`
5. **/etc/subgid**:
   `No subordinate GID range configured in /etc/subgid for the current user. Configure at least 65535 subgids (e.g. '<user>:100000:65536').`
6. **User Namespaces / AppArmor**:
   - `Unprivileged user namespaces cannot be created`
   - On recent Ubuntu / Debian kernels:
     `Unprivileged user namespaces are restricted by AppArmor (kernel.apparmor_restrict_unprivileged_userns = 1). Disable the restriction via sysctl -w kernel.apparmor_restrict_unprivileged_userns=0 or provide an AppArmor profile.`
     *(Note: Installing via the `.deb` package automatically configures and reloads this profile.)*

## Limitations

- **Not a Security Sandbox**: `installctl` is designed for transparency and review of cooperative installers, not as a hard security boundary against adversarial malware attempting kernel exploits or network attacks.
- **Dangerous Commands Interception**: Interception of dangerous actions is based on recognized command names (such as `apt-get`, `systemctl`, `useradd`). Arbitrary binaries or scripts invoking low-level syscalls directly are not intercepted.
- **Hard Links**: In the FUSE overlay filesystem, hard links created during installation are staged and applied as copies rather than preserving inode sharing.
