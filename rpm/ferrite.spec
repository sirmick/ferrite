Name:           ferrite
Version:        0.9.0
Release:        1%{?dist}
Summary:        Web-based SDR daemon with bundled SoapySDR

License:        MIT OR Apache-2.0
URL:            https://github.com/sirmick/ferrite
Source0:        ferrite_%{version}.tar.xz

# Bundled SoapySDR libs/plugins live under /usr/lib/ferrite/soapysdr — don't
# advertise them to the system Provides set, and don't try to satisfy our
# own libSoapySDR.so dep from system packages.
%global __provides_exclude_from ^/usr/lib/ferrite/soapysdr/.*$
%global __requires_exclude ^libSoapySDR\\.so.*$

BuildRequires:  gcc gcc-c++ make pkgconf-pkg-config cmake clang lld
BuildRequires:  git-core ca-certificates curl xz tar
BuildRequires:  rtl-sdr-devel hackrf-devel
# rustc/cargo come from rustup in the Dockerfile (Cargo.toml pins
# rust-version = 1.89, newer than Fedora 40's apt rust 1.78).

%description
Spectrum-centric SDR with a thin Rust daemon (ferrited) and a browser
front end that does demod and decoding in WASM. Includes RTL-SDR and
HackRF driver modules; SDRplay support requires the closed-source
SDRplay API installed separately.

Bundles SoapySDR + driver plugins under /usr/lib/ferrite/soapysdr to
avoid version skew with distro packages.

%prep
%setup -q -n ferrite-%{version}

%build
./scripts/build-soapysdr.sh
# Point cargo at the bundled SoapySDR — sourcing soapysdr/env.sh
# directly is brittle inside rpmbuild's %build wrapper, so set the
# vars explicitly here.
export PKG_CONFIG_PATH="$PWD/soapysdr/lib/pkgconfig:${PKG_CONFIG_PATH:-}"
export LD_LIBRARY_PATH="$PWD/soapysdr/lib:${LD_LIBRARY_PATH:-}"
cargo build --release

%install
install -d %{buildroot}/usr/bin
install -d %{buildroot}/usr/lib/ferrite
install -d %{buildroot}/usr/lib/ferrite/soapysdr/lib/SoapySDR/modules0.8-3
install -d %{buildroot}/usr/share/ferrite/web
install -d %{buildroot}/usr/share/ferrite/flowgraphs
install -m 755 target/release/ferrited %{buildroot}/usr/lib/ferrite/
install -m 755 packaging/ferrited      %{buildroot}/usr/bin/
cp -a soapysdr/lib/libSoapySDR.so* \
    %{buildroot}/usr/lib/ferrite/soapysdr/lib/
cp -a soapysdr/lib/SoapySDR/modules0.8-3/*.so \
    %{buildroot}/usr/lib/ferrite/soapysdr/lib/SoapySDR/modules0.8-3/
cp -a web/build/.   %{buildroot}/usr/share/ferrite/web/
cp -a flowgraphs/.  %{buildroot}/usr/share/ferrite/flowgraphs/

%files
/usr/bin/ferrited
/usr/lib/ferrite/
/usr/share/ferrite/

%changelog
* Tue Apr 28 2026 Mick <sirmick@gmail.com> - 0.9.0-1
- Pre-release version bump. Multi-arch package matrix + expanded SoapySDR
  driver bundle (Airspy R2/HF+, BladeRF, PlutoSDR added on top of the
  prior RTL-SDR + HackRF + SDRplay set).

* Mon Apr 27 2026 Mick <sirmick@gmail.com> - 0.0.1-1
- Initial Fedora package.
