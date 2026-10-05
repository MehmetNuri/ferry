# Ferry

Ferry is a file transfer client for GNOME. Browse cloud storage and file servers, upload
and download files, share links and keep folders in sync. Cryptomator vaults open right
in the window, so files can be encrypted before they leave your computer.

It is written in Rust with GTK 4 and libadwaita, and follows the GNOME Human
Interface Guidelines.

![Ferry showing the photos of a file server in a grid](data/screenshots/photos.png)

## Supported services

**File servers**

- SFTP (SSH), also through a jump host as with `ssh -J`; signing in with the SSH agent
  (including keys on smartcards and YubiKeys), a key file or a password, and server keys
  checked against `~/.ssh/known_hosts` or confirmed by their fingerprint
- FTP over TLS (explicit or implicit), and plain FTP where you choose it
- WebDAV, and Nextcloud and ownCloud by their server address

**Cloud storage**

- Google Drive, with the Google account of Settings › Online Accounts; nothing to sign in
  to again, and deleted files go to the Drive trash
- Azure Blob Storage, signing in with the account key or a shared access signature (SAS)

Ferry also works with services that offer the S3 API. These come with ready-made settings:

- Amazon S3
- Cloudflare R2
- Supabase Storage
- MinIO
- Backblaze B2
- Wasabi
- DigitalOcean Spaces
- Hetzner Object Storage
- Scaleway Object Storage
- OVHcloud Object Storage
- Akamai (Linode) Object Storage
- Exoscale SOS
- Storj
- Google Cloud Storage (with HMAC keys)

Other S3-compatible servers, such as Ceph, Garage or SeaweedFS, are added with their
endpoint address, also when they use a self-signed certificate or a private certificate
authority. The built-in compatibility test shows which features a server supports.

**Encryption:** Cryptomator vaults (format 8), created by Ferry or by the Cryptomator
apps, can be unlocked, browsed and written in any bucket and on any file server.

**Signing in:** access keys kept in the GNOME keyring, AWS CLI profiles including IAM
Identity Center (single sign-on) and `credential_process`, assumed roles with MFA codes,
and temporary session tokens.

**Importing connections:** from rclone, s3cmd, Cyberduck bookmarks and the AWS CLI.

Further cloud services, such as OneDrive and Dropbox, are planned.

## Features

**Browsing**
- List and grid views (Ctrl+1 / Ctrl+2), grid zoom (Ctrl+plus / Ctrl+minus), thumbnails cached on disk
- Sorting (A–Z, Z–A, last or first modified, size), hidden files with Ctrl+H, filters by type, size and date
- Quick preview with the space key: images (zoom, pan), video, audio, and text with syntax highlighting
- Details pane (Alt+Return) with headers, versions, tags, permissions, SHA-256 and image dimensions
- Recent objects of every connection, and a search of all connections (Ctrl+Shift+F)
- Back, forward, breadcrumbs, favorites, `s3://` locations (Ctrl+L), and `s3://` links opened from anywhere in the desktop

**Files**
- Upload by drag and drop (also onto a folder), by pasting files from GNOME Files, a picture or plain text
- Conflict handling: replace, replace only older objects, keep both, skip
- Copy, cut and paste, moving by drag and drop, copying to another bucket by dropping on it, undo with Ctrl+Z
- Copied objects paste into GNOME Files as files; dragging out to Files or the desktop works too
- Batch rename, new folder, new text file opened in the text editor and uploaded again when saved
- Download folders as one ZIP file, export a folder listing as CSV
- Copy as `s3://` location, AWS CLI command or the text of a small file
- Select items matching a pattern (Ctrl+S), background menu on empty space
- Deleted objects of versioned buckets can be restored, one by one or all at once
- Bulk changes of Cache-Control, Content-Type and storage class

**Transfers**
- A queue with priorities, drag to reorder, global and per-transfer pause, speed limit, concurrency
- Uploads and downloads continue where they stopped, after an error or a restart
- The files' own dates travel with them (rclone-compatible `mtime` metadata)
- Large files move in parallel parts in both directions
- Connection problems are retried by themselves; unfinished transfers are kept for the next start
- Live speed graph, progress on the dock icon (LauncherEntry), notifications with Retry, keeps the computer awake

**Tools**
- Folder sync with a review of the changes (deletions included) before anything happens
- Scheduled and watched backup jobs, mounting a bucket as a folder (bookmarked in GNOME Files while mounted)
- Share links with QR codes, upload links for people without an account
- Bucket policy, CORS, lifecycle, versioning, encryption, Object Lock, website hosting, logging, CloudFront
- Storage analyzer, a compatibility test for any provider, emptying a bucket
- Common errors explained in plain words (access, keys, clock, quotas, network)

**GNOME integration**
- Search provider for the GNOME Shell overview, background portal instead of a tray icon, start at login
- Credentials in the system keyring; AWS CLI profiles (also IAM Identity Center / SSO), assumed roles with MFA codes, Transfer Acceleration
- Connection backups encrypted with a password (Argon2id, AES-256-GCM), age or SSH keys, a YubiKey (age-plugin-yubikey) or a GnuPG key
- Self-signed servers and private certificate authorities, trusted after comparing the SHA-256 fingerprint; the desktop proxy settings are followed
- Adaptive layout, keyboard shortcuts window (Ctrl+?), follows the privacy setting for file history

## Screenshots

| | |
|---|---|
| ![Welcome page with the providers](data/screenshots/welcome.png) | ![Documents on a file server](data/screenshots/documents.png) |
| Choose a provider to add a connection | Browse a server like a folder |
| ![Quick preview of a photo](data/screenshots/preview.png) | ![An unlocked Cryptomator vault](data/screenshots/vault.png) |
| Quick preview with the space key | Cryptomator vaults open in place |
| ![The transfer queue](data/screenshots/transfers.png) | ![The connection editor for SFTP with a jump host](data/screenshots/connection.png) |
| Transfers with speed and time left | SFTP through a jump host |

## Command line

The saved connections can be used from a terminal and in scripts, without a window:

```sh
ferry connections                              # the saved connections
ferry ls -l "Work:photos/2026"                 # list a folder
ferry put -r ~/Pictures/trip Work:photos/      # upload a folder
ferry get -r Work:photos/trip ~/Downloads      # download a folder
ferry cat Work:notes/todo.txt                  # print an object
ferry share --expires 600 Work:photos/a.jpg    # a link valid for ten minutes
ferry rm -r Work:photos/old                    # remove a folder
```

## Source layout

```
src/
  main.rs, application.rs      start-up, application actions, preferences, about
  config.rs, i18n.rs           build configuration, gettext helpers
  cli.rs                       the command line (ferry ls/get/put/rm/cat/share)
  profile.rs, settings.rs      saved connections (secrets in the keyring), GSettings
  backup/                      connection backups: password, age/SSH/YubiKey, GnuPG
  runtime.rs                   the Tokio runtime and the bridge to the GLib main loop
  search.rs                    GNOME Shell search provider
  window/                      the main window: browsing, actions, tabs
  remote/                      file servers and cloud drives: SFTP (with jump hosts), FTP over
                               TLS, WebDAV, Azure Blob Storage, Google Drive (through GNOME
                               Online Accounts)
  s3/                          S3 operations: objects, bucket tools, access settings,
                               connections (proxy, certificates, AWS profiles, roles)
  transfers/                   transfer queue and its panel, mounts, drags, external editors
  pages/                       Recent, Analyzer, Backups, Compatibility
  dialogs/                     dialogs and the quick preview
  widgets/                     small custom widgets
  devtools/                    self test, probe, the scripted interface run and screenshots
crates/
  cryptomator-vault/           Cryptomator vault format 8, independent of the storage
data/
  resources/                   Blueprint files, style sheet and icons (GResource)
  *.desktop.in, *.metainfo.xml.in, *.gschema.xml, icons/
po/                            translations
build-aux/                     Flatpak manifest
```

## Building

Dependencies: Rust (stable), GTK ≥ 4.16, libadwaita ≥ 1.6, blueprint-compiler, gettext.
On Fedora: `sudo dnf install gtk4-devel libadwaita-devel blueprint-compiler meson gcc openssl-devel`.
Syntax highlighting needs `gtksourceview5-devel` (a default feature; build with `--no-default-features` without it, Meson does this by itself).

```sh
cargo build --profile fast        # optimized build for trying the application
cargo run                          # development build
```

With Meson (installs the desktop file, icons, schema, search provider and translations):

```sh
meson setup build -Dprofile=default   # -Dsourceview=enabled|disabled|auto
meson compile -C build
meson install -C build
```

Flatpak:

```sh
flatpak run org.flatpak.Builder --user --force-clean --repo=repo build build-aux/io.github.mehmetnuri.Ferry.json
flatpak build-bundle repo ferry.flatpak io.github.mehmetnuri.Ferry
```

## Testing

```sh
cargo test                                         # unit tests
FERRY_SELFTEST=<connection name> cargo test selftest -- --nocapture
FERRY_SMOKE=<connection name> target/debug/ferry   # scripted run of the interface
```

The self test and the scripted run use a saved connection and write only below
`ferry-selftest/` and `ferry-smoke/` in its first bucket, which they delete afterwards.
`FERRY_SHOTS=<folder>` saves pictures of each step of the scripted run.

## License

Ferry is free software: you can redistribute it and/or modify it under the terms of
the GNU General Public License as published by the Free Software Foundation, either
version 3 of the License, or (at your option) any later version. See [LICENSE](LICENSE).

The `cryptomator-vault` crate is under the same license. Its test vaults come from
other projects under the MIT license, as listed in
`crates/cryptomator-vault/tests/fixtures/SOURCES.txt`.
