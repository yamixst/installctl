# The release binary is built by cargo without DWARF info, so the
# auto-generated debuginfo/debugsource subpackages would be empty and
# rpmbuild fails with "Empty %files file debugsourcefiles.list".
%global debug_package %{nil}

Name:           installctl
Version:        0.1.0
Release:        1%{?dist}
Summary:        Safe sandbox runner and change tracker for software installers

License:        Apache-2.0
URL:            https://github.com/yamixst/installctl
Source0:        %{name}-%{version}.tar.gz

BuildRequires:  gcc
BuildRequires:  pkgconfig
BuildRequires:  fuse3-devel

Requires:       fuse3-libs
Requires:       shadow-utils
Requires:       sudo

%description
installctl executes arbitrary software installers in an unprivileged sandbox
backed by a FUSE overlay filesystem, intercepts privilege escalation and
dangerous system commands, presents an interactive diff review, and generates
clean uninstallation scripts.

%prep
%autosetup

%build
cargo build --release

%install
rm -rf %{buildroot}
install -D -p -m 0755 target/release/installctl %{buildroot}%{_bindir}/installctl
install -D -p -m 0644 README.md %{buildroot}%{_docdir}/%{name}/README.md

%files
%license LICENSE
%{_bindir}/installctl
%doc %{_docdir}/%{name}/README.md
