# Packages a tree installed by `meson install`; see .github/workflows/release.yml.
%global debug_package %{nil}

Name:           ferry
Version:        %{ferry_version}
Release:        1%{?dist}
Summary:        Move files to and from servers and cloud storage
License:        GPL-3.0-or-later
URL:            https://github.com/MehmetNuri/ferry
Packager:       Mehmet Nuri Öztürk <info@mehmetnuri.net>
Recommends:     fuse3
Suggests:       gnome-online-accounts
Suggests:       gnupg2

%description
Ferry is a GNOME client for S3-compatible storage, SFTP, FTP, WebDAV,
Azure Blob Storage and Google Drive, with Cryptomator vaults.

%install
cp -a %{ferry_root}/. %{buildroot}/
install -Dm644 %{ferry_source}/LICENSE %{buildroot}%{_datadir}/licenses/ferry/LICENSE

%files
%{_datadir}/licenses/ferry/LICENSE
%{_bindir}/ferry
%{_datadir}/applications/io.github.mehmetnuri.Ferry.desktop
%{_datadir}/dbus-1/services/io.github.mehmetnuri.Ferry.service
%{_datadir}/glib-2.0/schemas/io.github.mehmetnuri.Ferry.gschema.xml
%{_datadir}/gnome-shell/search-providers/io.github.mehmetnuri.Ferry.search-provider.ini
%{_datadir}/icons/hicolor/scalable/apps/io.github.mehmetnuri.Ferry.svg
%{_datadir}/icons/hicolor/symbolic/apps/io.github.mehmetnuri.Ferry-symbolic.svg
%{_datadir}/locale/*/LC_MESSAGES/ferry.mo
%{_datadir}/metainfo/io.github.mehmetnuri.Ferry.metainfo.xml
