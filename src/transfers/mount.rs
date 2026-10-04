//! Mounts a bucket or folder as a local folder through FUSE, so it can be used
//! from GNOME Files and any application. Reads are fetched in ranges; files
//! opened for writing are edited in a private temporary copy and uploaded when closed.
use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, MountOption, OpenFlags, RenameFlags,
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::i18n::{tr, trf};
use crate::runtime::runtime;
use crate::s3::{Progress, S3};

const TTL: Duration = Duration::from_secs(2);
const LIST_TTL: Duration = Duration::from_secs(5);
const READ_AHEAD: u64 = 1024 * 1024;
const ROOT: u64 = 1;

#[derive(Clone)]
struct Entry {
    name: String,
    dir: bool,
    size: u64,
    mtime: i64,
}

struct Handle {
    path: String,
    /// The private copy of a file opened for writing.
    temp: Option<(std::fs::File, PathBuf)>,
    dirty: bool,
    cache: Option<(u64, Vec<u8>)>,
}

#[derive(Default)]
struct State {
    paths: HashMap<u64, String>,
    inodes: HashMap<String, u64>,
    next_ino: u64,
    lists: HashMap<String, (Vec<Entry>, Instant)>,
    handles: HashMap<u64, Handle>,
    next_fh: u64,
}

struct S3Fs {
    client: S3,
    bucket: String,
    prefix: String,
    read_only: bool,
    uid: u32,
    gid: u32,
    temp_dir: PathBuf,
    state: Mutex<State>,
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() { name.to_string() } else { format!("{parent}/{name}") }
}

fn split(path: &str) -> (&str, &str) {
    match path.rfind('/') { Some(i) => (&path[..i], &path[i + 1..]), None => ("", path) }
}

impl S3Fs {
    fn key(&self, path: &str) -> String {
        format!("{}{}", self.prefix, path)
    }

    fn ino(&self, state: &mut State, path: &str) -> u64 {
        if path.is_empty() { return ROOT; }
        if let Some(ino) = state.inodes.get(path) { return *ino; }
        state.next_ino += 1;
        let ino = state.next_ino;
        state.inodes.insert(path.to_string(), ino);
        state.paths.insert(ino, path.to_string());
        ino
    }

    fn path(&self, ino: INodeNo) -> Option<String> {
        if ino.0 == ROOT { return Some(String::new()); }
        self.state.lock().unwrap().paths.get(&ino.0).cloned()
    }

    fn attr(&self, ino: u64, entry: &Entry) -> FileAttr {
        let mtime = UNIX_EPOCH + Duration::from_secs(entry.mtime.max(0) as u64);
        let writable = if self.read_only { 0 } else { 0o200 };
        FileAttr {
            ino: INodeNo(ino), size: entry.size, blocks: entry.size.div_ceil(512), atime: mtime, mtime, ctime: mtime, crtime: mtime,
            kind: if entry.dir { FileType::Directory } else { FileType::RegularFile },
            perm: if entry.dir { 0o500 | writable | if self.read_only { 0 } else { 0o700 } } else { 0o400 | writable },
            nlink: if entry.dir { 2 } else { 1 }, uid: self.uid, gid: self.gid, rdev: 0, blksize: 4096, flags: 0,
        }
    }

    fn root_entry() -> Entry {
        Entry { name: String::new(), dir: true, size: 0, mtime: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0) }
    }

    /// Lists a folder, cached for a few seconds so the file manager stays fast.
    fn list(&self, path: &str) -> Result<Vec<Entry>, Errno> {
        if let Some((entries, at)) = self.state.lock().unwrap().lists.get(path) && at.elapsed() < LIST_TTL {
            return Ok(entries.clone());
        }
        let prefix = if path.is_empty() { self.prefix.clone() } else { format!("{}/", self.key(path)) };
        let (client, bucket) = (self.client.clone(), self.bucket.clone());
        let entries = runtime().block_on(async move {
            let mut entries = Vec::new();
            let mut token = String::new();
            loop {
                let listing = client.list_objects(&bucket, &prefix, &token).await?;
                for item in listing.items {
                    let name = item.name.trim_end_matches('/').to_string();
                    // Supabase keeps empty folders alive with a hidden placeholder object.
                    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name == ".emptyFolderPlaceholder" { continue; }
                    entries.push(Entry { name, dir: item.is_folder, size: item.size.max(0) as u64, mtime: item.modified });
                }
                if listing.next_token.is_empty() { break; }
                token = listing.next_token;
            }
            Ok::<_, String>(entries)
        }).map_err(|_| Errno::EIO)?;
        self.state.lock().unwrap().lists.insert(path.to_string(), (entries.clone(), Instant::now()));
        Ok(entries)
    }

    fn find(&self, path: &str) -> Result<Entry, Errno> {
        if path.is_empty() { return Ok(Self::root_entry()); }
        let (parent, name) = split(path);
        self.list(parent)?.into_iter().find(|e| e.name == name).ok_or(Errno::ENOENT)
    }

    fn forget_list(&self, path: &str) {
        {
            let mut state = self.state.lock().unwrap();
            state.lists.remove(split(path).0);
            state.lists.remove(path);
        }
        // Every change made through the mount passes here; the window hears of it.
        let parent = split(path).0;
        let folder = if parent.is_empty() { self.prefix.clone() } else { format!("{}/", self.key(parent)) };
        let (profile, bucket) = (self.client.profile.id.clone(), self.bucket.clone());
        gtk::glib::MainContext::default().invoke(move || {
            ON_CHANGE.with(|f| if let Some(f) = f.borrow().as_ref() { f(&profile, &bucket, &folder) });
        });
    }

    fn new_handle(&self, path: &str, temp: Option<(std::fs::File, PathBuf)>, dirty: bool) -> u64 {
        let mut state = self.state.lock().unwrap();
        state.next_fh += 1;
        let fh = state.next_fh;
        state.handles.insert(fh, Handle { path: path.to_string(), temp, dirty, cache: None });
        fh
    }

    fn temp_file(&self) -> std::io::Result<(std::fs::File, PathBuf)> {
        let path = self.temp_dir.join(glib_uuid());
        let file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path)?;
        Ok((file, path))
    }

    fn block<T: Send + 'static>(&self, future: impl std::future::Future<Output = Result<T, String>> + Send + 'static) -> Result<T, Errno> {
        runtime().block_on(future).map_err(|error| {
            eprintln!("S3 mount: {error}");
            Errno::EIO
        })
    }
}

fn glib_uuid() -> String {
    gtk::glib::uuid_string_random().to_string()
}

impl Filesystem for S3Fs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(parent) = self.path(parent) else { return reply.error(Errno::ENOENT) };
        let path = join(&parent, &name.to_string_lossy());
        match self.find(&path) {
            Ok(entry) => {
                let ino = self.ino(&mut self.state.lock().unwrap(), &path);
                reply.entry(&TTL, &self.attr(ino, &entry), Generation(0));
            }
            Err(e) => reply.error(e),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        let Some(path) = self.path(ino) else { return reply.error(Errno::ENOENT) };
        // A file being written reports the size of its private copy.
        if let Some(fh) = fh && let Some(handle) = self.state.lock().unwrap().handles.get(&fh.0)
            && let Some((file, _)) = &handle.temp && let Ok(meta) = file.metadata() {
            let entry = Entry { name: String::new(), dir: false, size: meta.len(), mtime: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0) };
            return reply.attr(&TTL, &self.attr(ino.0, &entry));
        }
        match self.find(&path) {
            Ok(entry) => reply.attr(&TTL, &self.attr(ino.0, &entry)),
            Err(e) => reply.error(e),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(&self, _req: &Request, ino: INodeNo, _mode: Option<u32>, _uid: Option<u32>, _gid: Option<u32>, size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>, _mtime: Option<fuser::TimeOrNow>, _ctime: Option<SystemTime>, fh: Option<FileHandle>,
        _crtime: Option<SystemTime>, _chgtime: Option<SystemTime>, _bkuptime: Option<SystemTime>, _flags: Option<fuser::BsdFileFlags>, reply: ReplyAttr) {
        let Some(path) = self.path(ino) else { return reply.error(Errno::ENOENT) };
        if let Some(size) = size {
            if self.read_only { return reply.error(Errno::EROFS); }
            let mut state = self.state.lock().unwrap();
            if let Some(fh) = fh && let Some(handle) = state.handles.get_mut(&fh.0) && let Some((file, _)) = &handle.temp {
                if file.set_len(size).is_err() { return reply.error(Errno::EIO); }
                handle.dirty = true;
                drop(state);
                let entry = Entry { name: String::new(), dir: false, size, mtime: 0 };
                return reply.attr(&TTL, &self.attr(ino.0, &entry));
            }
            drop(state);
            // Truncating a closed file to zero writes an empty object.
            if size == 0 {
                let (client, bucket, key) = (self.client.clone(), self.bucket.clone(), self.key(&path));
                if self.block(async move { client.create_folder(&bucket, &key).await }).is_err() { return reply.error(Errno::EIO); }
                self.forget_list(&path);
            }
        }
        match self.find(&path) {
            Ok(entry) => reply.attr(&TTL, &self.attr(ino.0, &entry)),
            Err(e) => reply.error(e),
        }
    }

    fn mkdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
        if self.read_only { return reply.error(Errno::EROFS); }
        let Some(parent) = self.path(parent) else { return reply.error(Errno::ENOENT) };
        let path = join(&parent, &name.to_string_lossy());
        let (client, bucket, key) = (self.client.clone(), self.bucket.clone(), format!("{}/", self.key(&path)));
        if let Err(e) = self.block(async move { client.create_folder(&bucket, &key).await }) { return reply.error(e); }
        self.forget_list(&path);
        let ino = self.ino(&mut self.state.lock().unwrap(), &path);
        reply.entry(&TTL, &self.attr(ino, &Entry { name: String::new(), dir: true, size: 0, mtime: Self::root_entry().mtime }), Generation(0));
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        if self.read_only { return reply.error(Errno::EROFS); }
        let Some(parent) = self.path(parent) else { return reply.error(Errno::ENOENT) };
        let path = join(&parent, &name.to_string_lossy());
        let (client, bucket, key) = (self.client.clone(), self.bucket.clone(), self.key(&path));
        match self.block(async move { client.delete_object(&bucket, &key).await }) {
            Ok(()) => { self.forget_list(&path); reply.ok() }
            Err(e) => reply.error(e),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        if self.read_only { return reply.error(Errno::EROFS); }
        let Some(parent) = self.path(parent) else { return reply.error(Errno::ENOENT) };
        let path = join(&parent, &name.to_string_lossy());
        self.forget_list(&path);
        match self.list(&path) {
            Ok(entries) if !entries.is_empty() => return reply.error(Errno::ENOTEMPTY),
            Err(e) => return reply.error(e),
            _ => {}
        }
        // The folder marker and any placeholder inside go together.
        let (client, bucket, key) = (self.client.clone(), self.bucket.clone(), format!("{}/", self.key(&path)));
        match self.block(async move { client.delete_keys(&bucket, vec![key]).await.map(|_| ()) }) {
            Ok(()) => { self.forget_list(&path); reply.ok() }
            Err(e) => reply.error(e),
        }
    }

    fn rename(&self, _req: &Request, parent: INodeNo, name: &OsStr, newparent: INodeNo, newname: &OsStr, _flags: RenameFlags, reply: ReplyEmpty) {
        if self.read_only { return reply.error(Errno::EROFS); }
        let (Some(parent), Some(newparent)) = (self.path(parent), self.path(newparent)) else { return reply.error(Errno::ENOENT) };
        let (from, to) = (join(&parent, &name.to_string_lossy()), join(&newparent, &newname.to_string_lossy()));
        let entry = match self.find(&from) { Ok(e) => e, Err(e) => return reply.error(e) };
        let (client, bucket) = (self.client.clone(), self.bucket.clone());
        let (from_key, to_key) = (self.key(&from), self.key(&to));
        let result = self.block(async move {
            if !entry.dir {
                return client.rename(&bucket, &from_key, &to_key).await;
            }
            // A folder moves object by object.
            let (items, _) = client.list_all(&bucket, &format!("{from_key}/"), usize::MAX).await?;
            for item in items {
                let target = format!("{to_key}/{}", item.key.strip_prefix(&format!("{from_key}/")).unwrap_or(&item.key));
                client.rename(&bucket, &item.key, &target).await?;
            }
            Ok(())
        });
        match result {
            Ok(()) => {
                let mut state = self.state.lock().unwrap();
                state.lists.clear();
                if let Some(ino) = state.inodes.remove(&from) {
                    state.inodes.insert(to.clone(), ino);
                    state.paths.insert(ino, to.clone());
                }
                drop(state);
                // Both folders changed (GNOME Files names a new folder by renaming it).
                self.forget_list(&from);
                self.forget_list(&to);
                reply.ok()
            }
            Err(e) => reply.error(e),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let Some(path) = self.path(ino) else { return reply.error(Errno::ENOENT) };
        let access = flags.0 & libc::O_ACCMODE;
        if access == libc::O_RDONLY {
            return reply.opened(FileHandle(self.new_handle(&path, None, false)), FopenFlags::empty());
        }
        if self.read_only { return reply.error(Errno::EROFS); }
        let (mut file, temp) = match self.temp_file() { Ok(t) => t, Err(e) => return reply.error(e.into()) };
        // Unless the file is truncated, editing starts from the current content.
        if flags.0 & libc::O_TRUNC == 0 {
            let (client, bucket, key) = (self.client.clone(), self.bucket.clone(), self.key(&path));
            let target = temp.clone();
            if let Err(e) = self.block(async move { client.download_file(&bucket, &key, None, &target, &Progress::default()).await }) {
                let _ = std::fs::remove_file(&temp);
                return reply.error(e);
            }
            file = match std::fs::OpenOptions::new().read(true).write(true).open(&temp) { Ok(f) => f, Err(e) => return reply.error(e.into()) };
        }
        let dirty = flags.0 & libc::O_TRUNC != 0;
        reply.opened(FileHandle(self.new_handle(&path, Some((file, temp)), dirty)), FopenFlags::empty());
    }

    #[allow(clippy::too_many_arguments)]
    fn read(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, offset: u64, size: u32, _flags: OpenFlags, _lock: Option<fuser::LockOwner>, reply: ReplyData) {
        let mut state = self.state.lock().unwrap();
        let Some(handle) = state.handles.get_mut(&fh.0) else { return reply.error(Errno::EBADF) };
        if let Some((file, _)) = handle.temp.as_mut() {
            let mut buffer = vec![0u8; size as usize];
            let read = file.seek(SeekFrom::Start(offset)).and_then(|_| file.read(&mut buffer));
            return match read { Ok(n) => reply.data(&buffer[..n]), Err(e) => reply.error(e.into()) };
        }
        let end = offset + size as u64;
        if let Some((start, data)) = &handle.cache && offset >= *start && end <= *start + data.len() as u64 {
            let from = (offset - start) as usize;
            return reply.data(&data[from..from + size as usize]);
        }
        let key = self.key(&handle.path);
        drop(state);
        // Sequential readers get the next megabyte in the same request.
        let (client, bucket) = (self.client.clone(), self.bucket.clone());
        let length = READ_AHEAD.max(size as u64);
        let result = self.block(async move {
            let out = client.client.get_object().bucket(&bucket).key(&key).range(format!("bytes={offset}-{}", offset + length - 1)).send().await;
            match out {
                Ok(out) => Ok(out.body.collect().await.map_err(|e| e.to_string())?.into_bytes().to_vec()),
                // Reading past the end of the object.
                Err(e) if crate::s3::error_code(&e) == "InvalidRange" => Ok(Vec::new()),
                Err(e) => Err(crate::s3::describe(e)),
            }
        });
        match result {
            Ok(data) => {
                let take = data.len().min(size as usize);
                reply.data(&data[..take]);
                if let Some(handle) = self.state.lock().unwrap().handles.get_mut(&fh.0) {
                    handle.cache = Some((offset, data));
                }
            }
            Err(e) => reply.error(e),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn write(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, offset: u64, data: &[u8], _write_flags: fuser::WriteFlags, _flags: OpenFlags, _lock: Option<fuser::LockOwner>, reply: ReplyWrite) {
        let mut state = self.state.lock().unwrap();
        let Some(handle) = state.handles.get_mut(&fh.0) else { return reply.error(Errno::EBADF) };
        let Some((file, _)) = handle.temp.as_mut() else { return reply.error(Errno::EBADF) };
        match file.seek(SeekFrom::Start(offset)).and_then(|_| file.write_all(data)) {
            Ok(()) => { handle.dirty = true; reply.written(data.len() as u32) }
            Err(e) => reply.error(e.into()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn release(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _flags: OpenFlags, _lock: Option<fuser::LockOwner>, _flush: bool, reply: ReplyEmpty) {
        let Some(handle) = self.state.lock().unwrap().handles.remove(&fh.0) else { return reply.ok() };
        let Some((file, temp)) = handle.temp else { return reply.ok() };
        drop(file);
        let mut result = Ok(());
        if handle.dirty {
            let (client, bucket, key, source) = (self.client.clone(), self.bucket.clone(), self.key(&handle.path), temp.clone());
            result = self.block(async move { client.upload_file(&bucket, &key, &source, &Progress::default()).await });
            self.forget_list(&handle.path);
        }
        let _ = std::fs::remove_file(&temp);
        match result { Ok(()) => reply.ok(), Err(e) => reply.error(e) }
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let Some(path) = self.path(ino) else { return reply.error(Errno::ENOENT) };
        let entries = match self.list(&path) { Ok(e) => e, Err(e) => return reply.error(e) };
        let mut state = self.state.lock().unwrap();
        let parent = if path.is_empty() { ROOT } else { self.ino(&mut state, split(&path).0) };
        let mut all = vec![(ino.0, FileType::Directory, ".".to_string()), (parent, FileType::Directory, "..".to_string())];
        for entry in entries {
            let child = self.ino(&mut state, &join(&path, &entry.name));
            all.push((child, if entry.dir { FileType::Directory } else { FileType::RegularFile }, entry.name));
        }
        for (i, (child, kind, name)) in all.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(child), (i + 1) as u64, kind, name) { break; }
        }
        reply.ok();
    }

    fn create(&self, _req: &Request, parent: INodeNo, name: &OsStr, _mode: u32, _umask: u32, _flags: i32, reply: ReplyCreate) {
        if self.read_only { return reply.error(Errno::EROFS); }
        let Some(parent) = self.path(parent) else { return reply.error(Errno::ENOENT) };
        let path = join(&parent, &name.to_string_lossy());
        let temp = match self.temp_file() { Ok(t) => t, Err(e) => return reply.error(e.into()) };
        // A new file is uploaded on close even when nothing was written.
        let fh = self.new_handle(&path, Some(temp), true);
        let ino = self.ino(&mut self.state.lock().unwrap(), &path);
        let entry = Entry { name: String::new(), dir: false, size: 0, mtime: Self::root_entry().mtime };
        reply.created(&TTL, &self.attr(ino, &entry), Generation(0), FileHandle(fh), FopenFlags::empty());
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        // Object storage has no fixed size; report a large, mostly free volume.
        reply.statfs(1 << 40, 1 << 39, 1 << 39, 1 << 30, 1 << 29, 4096, 1024, 4096);
    }
}

pub struct Mount {
    pub id: u64,
    pub label: String,
    pub path: PathBuf,
    pub read_only: bool,
    pub profile_id: String,
    pub bucket: String,
    pub prefix: String,
    session: Option<fuser::BackgroundSession>,
}

thread_local! {
    static MOUNTS: RefCell<Vec<Mount>> = const { RefCell::new(Vec::new()) };
    static NEXT: RefCell<u64> = const { RefCell::new(0) };
}

/// Mount points live under "~/Ferry" and are removed on unmount.
fn mount_root() -> PathBuf {
    gtk::glib::home_dir().join("Ferry")
}

/// The bookmarks of GNOME Files and the file chooser; a mounted bucket is listed
/// there while it is mounted.
fn bookmarks_path() -> PathBuf {
    gtk::glib::user_config_dir().join("gtk-3.0").join("bookmarks")
}

fn uri_of(path: &std::path::Path) -> String {
    use gtk::gio::prelude::FileExt;
    gtk::gio::File::for_path(path).uri().to_string()
}

/// Removes the lines `drop` matches and appends `add`. The file is written only when
/// something changed, in one step, so the user's own bookmarks are never disturbed.
fn edit_bookmarks(drop: impl Fn(&str) -> bool, add: Option<String>) {
    let path = bookmarks_path();
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    let kept: Vec<&str> = current.lines().filter(|line| !drop(line.split(' ').next().unwrap_or(""))).collect();
    let dropped = current.lines().count() - kept.len();
    if dropped == 0 && add.is_none() { return; }
    let mut text = kept.join("\n");
    if let Some(line) = add {
        if !text.is_empty() { text.push('\n'); }
        text.push_str(&line);
    }
    if !text.is_empty() { text.push('\n'); }
    if let Some(parent) = path.parent() { let _ = std::fs::create_dir_all(parent); }
    let temporary = path.with_extension("ferry-new");
    if std::fs::write(&temporary, text).is_ok() {
        let _ = std::fs::rename(&temporary, &path);
    }
}

/// Mounts do not survive the application; bookmarks of earlier ones are removed at startup.
pub fn forget_stale_bookmarks() {
    let root = uri_of(&mount_root());
    edit_bookmarks(|uri| uri.starts_with(&format!("{root}/")), None);
}

fn safe_name(text: &str) -> String {
    text.chars().map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' }).collect::<String>().trim_matches('-').to_string()
}

pub fn mount(client: S3, bucket: &str, prefix: &str, read_only: bool) -> Result<(u64, PathBuf), String> {
    if std::path::Path::new("/.flatpak-info").exists() {
        return Err(tr("Mounting is not possible inside the Flatpak sandbox"));
    }
    if !std::path::Path::new("/dev/fuse").exists() {
        return Err(tr("This system has no FUSE support (/dev/fuse)"));
    }
    if MOUNTS.with(|m| m.borrow().len()) >= 8 {
        return Err(trf("At most {n} mounts can be active at the same time", &[("n", "8")]));
    }
    let mut name = format!("{}-{}", safe_name(&client.profile.name), safe_name(bucket));
    let folder = safe_name(prefix.trim_end_matches('/').rsplit('/').next().unwrap_or(""));
    if !folder.is_empty() { name.push('-'); name.push_str(&folder); }
    let path = mount_root().join(&name);
    std::fs::create_dir_all(&path).map_err(|e| e.to_string())?;
    if std::fs::read_dir(&path).map(|mut d| d.next().is_some()).unwrap_or(false) {
        return Err(trf("The mount folder is not empty: {path}", &[("path", &path.display().to_string())]));
    }
    let temp_dir = gtk::glib::user_cache_dir().join("ferry").join("mounts").join(&name);
    std::fs::create_dir_all(&temp_dir).map_err(|e| e.to_string())?;
    let fs = S3Fs {
        client: client.clone(), bucket: bucket.to_string(), prefix: prefix.to_string(), read_only,
        uid: unsafe { libc::getuid() }, gid: unsafe { libc::getgid() }, temp_dir, state: Mutex::new(State { next_ino: ROOT, ..Default::default() }),
    };
    let mut config = fuser::Config::default();
    config.mount_options = vec![MountOption::FSName(format!("ferry:{bucket}")), MountOption::Subtype("s3".into()), MountOption::NoAtime];
    if read_only { config.mount_options.push(MountOption::RO); }
    config.n_threads = Some(4);
    let session = fuser::spawn_mount(fs, &path, &config).map_err(|e| {
        let _ = std::fs::remove_dir(&path);
        trf("Mounting failed: {error}", &[("error", &e.to_string())])
    })?;
    let id = NEXT.with(|n| { *n.borrow_mut() += 1; *n.borrow() });
    let label: String = format!("{} / {}{}", client.profile.name, bucket, if prefix.is_empty() { String::new() } else { format!("/{}", prefix.trim_end_matches('/')) })
        .chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let uri = uri_of(&path);
    edit_bookmarks(|u| u == uri, Some(format!("{uri} {label}")));
    MOUNTS.with(|m| m.borrow_mut().push(Mount { id, label, path: path.clone(), read_only, profile_id: client.profile.id.clone(), bucket: bucket.into(), prefix: prefix.into(), session: Some(session) }));
    Ok((id, path))
}

thread_local! {
    static ON_CHANGE: std::cell::RefCell<Option<Box<dyn Fn(&str, &str, &str)>>> = const { std::cell::RefCell::new(None) };
}

/// Called on the main thread with (connection, bucket, folder) after a change through a mount.
pub fn on_change(f: impl Fn(&str, &str, &str) + 'static) {
    ON_CHANGE.with(|c| c.replace(Some(Box::new(f))));
}

pub fn unmount(id: u64) -> Result<(), String> {
    let mount = MOUNTS.with(|m| {
        let mut mounts = m.borrow_mut();
        let index = mounts.iter().position(|x| x.id == id)?;
        Some(mounts.remove(index))
    });
    let Some(mut mount) = mount else { return Err(tr("No such mount")) };
    if let Some(session) = mount.session.take() {
        session.umount_and_join().map_err(|e| trf("Unmounting failed (the folder may be in use): {error}", &[("error", &e.to_string())]))?;
    }
    let _ = std::fs::remove_dir(&mount.path);
    let uri = uri_of(&mount.path);
    edit_bookmarks(|u| u == uri, None);
    Ok(())
}

pub fn unmount_all() {
    let ids: Vec<u64> = MOUNTS.with(|m| m.borrow().iter().map(|x| x.id).collect());
    for id in ids {
        let _ = unmount(id);
    }
}

/// Mounts as (id, label, path, read only, profile id, bucket, prefix).
pub fn list() -> Vec<(u64, String, PathBuf, bool, String, String, String)> {
    MOUNTS.with(|m| m.borrow().iter().map(|x| (x.id, x.label.clone(), x.path.clone(), x.read_only, x.profile_id.clone(), x.bucket.clone(), x.prefix.clone())).collect())
}
