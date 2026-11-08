# installctl

**Try an installer before it touches your system.**

[![CI](https://github.com/yamixst/installctl/actions/workflows/ci.yml/badge.svg)](https://github.com/yamixst/installctl/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/yamixst/installctl)](https://github.com/yamixst/installctl/releases)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)

`installctl` runs a software installer, such as a `curl ... | sh` one-liner, in a
sandbox. Then it shows you everything the installer tried to change. You choose
what to keep, and you get a script that removes it all later.

```sh
installctl "curl -fsSL https://example.com/install.sh | sh"
```

---

## Why?

Installer scripts are convenient, but they are black boxes:

- they put files in `~/.local`, `/usr/local/bin`, `/opt`, ...
- they quietly append lines to your `~/.bashrc`;
- they call `sudo apt-get install`, enable services, add users;
- and they rarely come with an uninstaller.

You only find out what happened afterwards, if ever.

With `installctl`:

| Without installctl                         | With installctl                                           |
|--------------------------------------------|-----------------------------------------------------------|
| The installer writes straight to your disk | All writes go to a sandbox first                          |
| `sudo apt-get install ...` just runs       | You are asked: skip it, run it, or abort                  |
| You don't know which files changed         | You get a full list of created, modified and deleted files |
| Everything or nothing                      | Apply all, apply some, or reject everything               |
| Uninstalling is guesswork                  | A ready-made `uninstall-<name>.sh` script                  |

## What it looks like

```text
$ installctl --name mytool "curl -fsSL https://example.com/install.sh | sh"
installctl: running the installer in a sandbox; nothing is changed on your system yet.
Downloading mytool 1.4.2...

installctl: the installer wants to run a command that changes your system outside the sandbox:
  sudo apt-get install -y libfuse2
  → installs system packages libfuse2 via apt-get
What would you like to do?
  [s] skip it, tell the installer it succeeded
  [r] run it for real
  [S] skip this and every further one
  [a] abort the installation
Choice: s

Installer exited with code 0.

Created (2):
  [1] + /home/alice/.local/bin/mytool  (4.2 MB)
  [2] + /usr/local/bin/mytool-helper  (120 B, root)
Modified (1):
  [3] ~ /home/alice/.bashrc  (+1 line)
Deleted (0):
Dangerous actions (1):
  ! skipped: sudo apt-get install -y libfuse2 — installs system packages libfuse2 via apt-get
  [a] apply all
  [s] select what to apply
  [d] show diffs of modified text files
  [r] reject, change nothing
Choice: a
Create an uninstall script?
  [y] yes
  [n] no
Choice: y
Where should uninstall-<name>.sh go? [~/.local/bin]:
Uninstall script written to /home/alice/.local/bin/uninstall-mytool.sh
```

Nothing on your real system changes until you pick `[a]` or `[s]` in the
review menu.

## Installation

Prebuilt packages are on the [Releases](https://github.com/yamixst/installctl/releases) page.

**Debian / Ubuntu**

```sh
sudo apt install ./installctl_*_amd64.deb
```

The package pulls in `libfuse3-3`, `uidmap` and `sudo`. On Ubuntu 24.04+ it also
installs an AppArmor profile so that `installctl` may use user namespaces.

**AlmaLinux / Rocky / RHEL 9**

```sh
sudo dnf install ./installctl-*.el9.x86_64.rpm
```

**From source** (Rust 2024 edition, `gcc`, `pkg-config`, `libfuse3-dev` / `fuse3-devel`)

```sh
git clone https://github.com/yamixst/installctl.git
cd installctl
cargo build --release
install -m 0755 target/release/installctl ~/.local/bin/installctl
```

After installing, make sure your user has subordinate IDs (most distributions
create them for new users automatically):

```sh
grep "^$USER:" /etc/subuid /etc/subgid
```

If nothing is printed, see [Requirements](#requirements).

## Usage

```sh
installctl [--name <program>] <command...>
```

Pass the install command either as one quoted string (handy for pipes) or as
separate arguments:

```sh
installctl "curl -fsSL https://example.com/install.sh | sh"
installctl sh ./install.sh --prefix "$HOME/.local"
installctl --name mytool ./install.sh
```

`--name` sets the program name used for the uninstall script. Without it,
`installctl` guesses the name from the installed executables and asks if it is
not sure.

### Step by step

1. **The installer runs in the sandbox.** It sees your real system and has
   network access, but every file it writes, changes or deletes goes into a
   temporary layer. `sudo` inside the sandbox is emulated, so the installer
   can "write" to `/usr` without a password.

2. **System commands need your approval.** When the installer calls a command
   that would change the system beyond files, `installctl` pauses and asks:

   | Key | Meaning                                                        |
   |-----|----------------------------------------------------------------|
   | `s` | skip it; the installer is told it succeeded                    |
   | `r` | run it for real on your system (with real `sudo` if requested) |
   | `S` | skip this command and every later call of it                   |
   | `a` | abort; nothing is changed                                      |

   These commands are intercepted:

   | Category   | Commands                                                                 |
   |------------|--------------------------------------------------------------------------|
   | Packages   | `apt`, `apt-get`, `dpkg`, `dnf`, `yum`, `rpm`, `pacman`, `zypper`, `apk`, `snap`, `flatpak` |
   | Accounts   | `useradd`, `usermod`, `userdel`, `groupadd`, `groupmod`, `groupdel`, `adduser`, `addgroup` |
   | Services   | `systemctl`, `service`, `loginctl`, `update-rc.d`, `chkconfig`           |
   | Scheduling | `crontab`, `at`                                                          |
   | System     | `ldconfig`, `modprobe`, `sysctl`                                         |

3. **You review the result.** `installctl` lists every created (`+`),
   modified (`~`) and deleted (`-`) file with its size, owner and line changes,
   plus the system commands and any background processes the installer left
   behind (they are stopped). Then you choose:

   | Key | Meaning                                                       |
   |-----|---------------------------------------------------------------|
   | `a` | apply all changes                                             |
   | `s` | select: enter numbers to **exclude**, e.g. `2,5-7`            |
   | `d` | show unified diffs of modified text files                     |
   | `r` | reject; your system stays exactly as it was                   |

   If a file on your system changed since the installer started,
   `installctl` warns you about the conflict and asks before going on.

4. **Changes are applied.** Files you can write are applied directly. Only if
   some changes need root (for example `/usr/local/bin`) does `installctl` call
   `sudo` once, for that part only.

5. **You get an uninstall script** (optional), by default
   `~/.local/bin/uninstall-<name>.sh`.

### Uninstalling

Run the generated script:

```text
$ uninstall-mytool.sh
This will undo the installation of mytool:
[1] delete file /home/alice/.local/bin/mytool
[2] delete file /usr/local/bin/mytool-helper
[3] revert installer changes in /home/alice/.bashrc
Proceed? [a]ll / [s]elect / [c]ancel: a
Remove this uninstall script? [y/N] y
```

The script is careful:

- it reverts only the installer's own edits in modified files, so lines you
  added later stay; if that is impossible, it shows you the diff instead;
- it warns before deleting a file that changed since the install;
- it removes directories only if they are empty;
- it undoes system commands where possible (`systemctl enable` → `disable`);
- it uses `sudo` only for paths you cannot write.

### Exit codes

| Code | Meaning                                                              |
|------|----------------------------------------------------------------------|
| `0`  | changes applied, or there was nothing to install                     |
| `1`  | you rejected or aborted, or root rights could not be obtained        |
| `2`  | internal error                                                       |
| `3`  | system requirements are not met (each problem is printed)            |

## Requirements

Linux with FUSE and unprivileged user namespaces. `installctl` checks these on
every start and explains what is missing:

| Requirement                     | How to fix                                                                 |
|---------------------------------|----------------------------------------------------------------------------|
| `/dev/fuse` is accessible       | load the module: `sudo modprobe fuse`                                      |
| `newuidmap`, `newgidmap`        | install the `uidmap` package (`shadow-utils` on EL)                        |
| subordinate IDs for your user   | `sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 $USER` |
| user namespaces are allowed     | on Ubuntu 24.04+ the `.deb` installs an AppArmor profile; otherwise run `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0` |

## How it works

```text
                ┌──────────────────────── sandbox ────────────────────────┐
  installer ──► │ user + mount + PID namespaces                           │
                │   /  = FUSE copy-on-write overlay over your real system │
                │   sudo, apt-get, systemctl, ... = installctl shims ─────┼──► ask you
                └─────────────────────────────┬───────────────────────────┘
                                              │ list of changes
                                              ▼
                                   review: apply / select / reject
                                              │
                         ┌────────────────────┴────────────────────┐
                         ▼                                         ▼
               user files: applied directly          root files: one sudo call
                                              │
                                              ▼
                                  uninstall-<name>.sh
```

- The installer runs as you, inside unprivileged namespaces. Its view of `/` is
  a FUSE overlay: reads come from your real system, writes go to a staging
  directory under `~/.local/state/installctl` (or `$XDG_STATE_HOME/installctl`).
- `sudo` and the commands listed above are replaced by small shims that ask
  `installctl` (and you) what to do.
- After the installer exits, the staged files are compared with the real
  system and shown to you. Accepted changes are written atomically.

## Limitations

- **It is not a security sandbox.** The installer can read your files and use
  the network. `installctl` is about transparency and control over well-behaved
  installers, not protection from malware.
- **Only known commands are intercepted.** Programs that change the system
  through other tools or direct syscalls are not caught (their file changes
  still are).
- **Hard links** created by the installer are applied as separate copies.

## Development

```sh
cargo test                      # unit tests and unprivileged integration tests
./scripts/test-in-docker.sh     # full suite incl. FUSE/namespace/sudo tests (Docker, --privileged)
```

Building packages (results go to `packaging/dist/`):

| Package          | In Docker (any distro)                       | On a native host                            |
|------------------|----------------------------------------------|---------------------------------------------|
| `.deb` (Ubuntu 24.04) | `bash packaging/scripts/build-deb-docker.sh` | `bash packaging/scripts/build-deb-local.sh` |
| `.rpm` (EL9)     | `bash packaging/scripts/build-rpm-docker.sh` | `bash packaging/scripts/build-rpm-local.sh` |

Native builds need `debhelper dpkg-dev pkg-config libfuse3-dev` (Debian/Ubuntu)
or `rpm-build gcc pkgconf-pkg-config fuse3-devel` (EL9). Pushing a `v*` tag
builds both packages and publishes a GitHub release.

## License

[Apache-2.0](LICENSE)
