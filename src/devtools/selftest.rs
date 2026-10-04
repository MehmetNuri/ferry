//! End-to-end check of the write operations against a saved connection. It
//! works only below "ferry-selftest/" and removes that folder afterwards.
//! Run with: FERRY_SELFTEST=<profile name> cargo test selftest -- --nocapture
#![cfg(test)]

use crate::profile;
use crate::runtime::runtime;
use crate::s3::{Progress, S3};

#[test]
fn selftest() {
    let Ok(name) = std::env::var("FERRY_SELFTEST") else { return };
    runtime().block_on(async move {
        let stored = profile::load().into_iter().find(|p| p.name == name).expect("no such profile");
        let client = S3::connect(profile::with_secrets(stored).await.expect("secrets")).await.expect("connect");
        let bucket = client.list_buckets().await.unwrap().buckets[0].name.clone();
        let base = "ferry-selftest/";
        let dir = std::env::temp_dir().join(format!("ferry-selftest-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("hello.txt"), b"hello from Ferry\n").unwrap();
        std::fs::write(dir.join("nested/data.bin"), vec![7u8; 300_000]).unwrap();
        // 17 MiB goes through the multipart path.
        std::fs::write(dir.join("big.bin"), vec![1u8; 17 * 1024 * 1024]).unwrap();
        let mut failures = 0;
        let mut check = |name: &str, result: Result<String, String>| match result {
            Ok(text) => println!("OK    {name:<34} {text}"),
            Err(error) => { failures += 1; println!("FAIL  {name:<34} {error}") }
        };
        let p = Progress::default();
        check("upload small", client.upload_file(&bucket, &format!("{base}hello.txt"), &dir.join("hello.txt"), &p).await.map(|_| String::new()));
        check("upload nested", client.upload_file(&bucket, &format!("{base}nested/data.bin"), &dir.join("nested/data.bin"), &p).await.map(|_| String::new()));
        let started = std::time::Instant::now();
        check("upload multipart 17 MiB", client.upload_file(&bucket, &format!("{base}big.bin"), &dir.join("big.bin"), &p).await.map(|_| format!("{:?}", started.elapsed())));
        check("progress counted", if p.done.load(std::sync::atomic::Ordering::Relaxed) >= 17 * 1024 * 1024 { Ok(String::new()) } else { Err("progress too low".into()) });
        // An upload cancelled after its first part continues with only the rest.
        let mixed: Vec<u8> = (0..40 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.join("resume.bin"), &mixed).unwrap();
        let first = Progress::default();
        let watcher = { let (f, c, b, k, p) = (first.clone(), client.clone(), bucket.clone(), format!("{base}resume.bin"), dir.join("resume.bin")); tokio::spawn(async move {
            while c.resume_parts(&b, &k, &p) < 1 { tokio::time::sleep(std::time::Duration::from_millis(2)).await; }
            f.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }) };
        let interrupted = client.upload_file(&bucket, &format!("{base}resume.bin"), &dir.join("resume.bin"), &first).await;
        watcher.abort();
        check("upload interrupted", if interrupted.is_err() { Ok(String::new()) } else { Err("finished despite cancel".into()) });
        let second = Progress::default();
        let started = std::time::Instant::now();
        check("upload resumed", client.upload_file(&bucket, &format!("{base}resume.bin"), &dir.join("resume.bin"), &second).await.map(|_| format!("{:?}", started.elapsed())));
        // Large files send several parts at once; the parts must still join in order.
        let large: Vec<u8> = (0..49 * 1024 * 1024u32).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect();
        std::fs::write(dir.join("large.bin"), &large).unwrap();
        let parallel = Progress::default();
        let started = std::time::Instant::now();
        check("upload 49 MiB, parallel parts", client.upload_file(&bucket, &format!("{base}large.bin"), &dir.join("large.bin"), &parallel).await
            .and_then(|_| { let n = parallel.done.load(std::sync::atomic::Ordering::Relaxed); if n == large.len() as u64 { Ok(format!("{:?}", started.elapsed())) } else { Err(format!("progress {n}")) } }));
        let started = std::time::Instant::now();
        check("parallel parts content (one stream)", client.download_file(&bucket, &format!("{base}large.bin"), None, &dir.join("down/large.bin"), &Progress::default()).await
            .and_then(|_| if std::fs::read(dir.join("down/large.bin")).ok().as_deref() == Some(&large[..]) { Ok(format!("{:?}", started.elapsed())) } else { Err("content differs".into()) }));
        let started = std::time::Instant::now();
        check("download 49 MiB in parallel", client.download_sized(&bucket, &format!("{base}large.bin"), large.len() as u64, &dir.join("down/fast.bin"), &Progress::default()).await
            .and_then(|_| if std::fs::read(dir.join("down/fast.bin")).ok().as_deref() == Some(&large[..]) { Ok(format!("{:?}", started.elapsed())) } else { Err("content differs".into()) }));
        // Large objects download in parallel pieces; an interrupted one continues.
        let first = Progress::default();
        let stopper = { let (f, marker) = (first.clone(), dir.join("down/parallel.bin.part.json")); tokio::spawn(async move {
            while !marker.exists() { tokio::time::sleep(std::time::Duration::from_millis(2)).await; }
            f.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }) };
        let interrupted = client.download_sized(&bucket, &format!("{base}large.bin"), large.len() as u64, &dir.join("down/parallel.bin"), &first).await;
        stopper.abort();
        let pieces = std::fs::read(dir.join("down/parallel.bin.part.json")).ok().and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
            .and_then(|v| v["done"].as_array().map(|a| a.len())).unwrap_or(0);
        check("parallel download interrupted", if interrupted.is_err() && pieces > 0 { Ok(format!("{pieces} pieces kept")) } else { Err(format!("{interrupted:?}, {pieces} pieces")) });
        let second = Progress::default();
        let started = std::time::Instant::now();
        check("parallel download continued", client.download_sized(&bucket, &format!("{base}large.bin"), large.len() as u64, &dir.join("down/parallel.bin"), &second).await
            .and_then(|_| if std::fs::read(dir.join("down/parallel.bin")).ok().as_deref() == Some(&large[..]) { Ok(format!("{:?}", started.elapsed())) } else { Err("content differs".into()) }));
        drop(large);
        // A download with half of the file already there fetches only the rest.
        let partial = dir.join("down/resume.bin.part");
        std::fs::create_dir_all(dir.join("down")).unwrap();
        std::fs::write(&partial, &mixed[..5 * 1024 * 1024]).unwrap();
        let etag = client.head_object(&bucket, &format!("{base}resume.bin")).await.map(|i| i.etag).unwrap_or_default();
        std::fs::write(dir.join("down/resume.bin.part.etag"), format!("\"{}\"", etag.trim_matches('"'))).unwrap();
        check("download resumed", client.download_file(&bucket, &format!("{base}resume.bin"), None, &dir.join("down/resume.bin"), &Progress::default()).await
            .and_then(|_| if std::fs::read(dir.join("down/resume.bin")).ok().as_deref() == Some(&mixed[..]) { Ok(String::new()) } else { Err("content differs".into()) }));
        check("list folder", client.list_objects(&bucket, base, "").await.map(|l| l.items.iter().map(|i| i.name.clone()).collect::<Vec<_>>().join(", ")));
        check("list all", client.list_all(&bucket, base, 1000).await.map(|(i, _)| format!("{} objects", i.len())));
        check("head", client.head_object(&bucket, &format!("{base}hello.txt")).await.map(|i| format!("{} {} {}", i.size, i.content_type, i.etag)));
        let target = dir.join("down/hello.txt");
        check("download + md5", client.download_file(&bucket, &format!("{base}hello.txt"), None, &target, &Progress::default()).await.map(|_| String::new()));
        check("downloaded content", match std::fs::read(&target) { Ok(d) if d == b"hello from Ferry\n" => Ok(String::new()), Ok(_) => Err("content differs".into()), Err(e) => Err(e.to_string()) });
        check("download multipart", client.download_file(&bucket, &format!("{base}big.bin"), None, &dir.join("down/big.bin"), &Progress::default()).await.map(|_| String::new()));
        check("download as zip", async {
            let (items, _) = client.list_all(&bucket, base, 1000).await?;
            let keys: Vec<String> = items.into_iter().map(|i| i.key).collect();
            let zip_path = dir.join("down/archive.zip");
            client.download_archive(&bucket, base, keys.clone(), &zip_path, &Progress::default()).await?;
            let mut archive = zip::ZipArchive::new(std::fs::File::open(&zip_path).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
            let mut hello = String::new();
            std::io::Read::read_to_string(&mut archive.by_name("hello.txt").map_err(|e| e.to_string())?, &mut hello).map_err(|e| e.to_string())?;
            if hello != "hello from Ferry\n" { return Err("content differs".into()); }
            Ok(format!("{} entries, {} bytes", archive.len(), std::fs::metadata(&zip_path).map(|m| m.len()).unwrap_or(0)))
        }.await);
        // The local file's own date travels with the upload and comes back on download.
        check("file date kept", async {
            let path = dir.join("dated.txt");
            std::fs::write(&path, b"from 2001").map_err(|e| e.to_string())?;
            let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
            std::fs::File::options().write(true).open(&path).and_then(|f| f.set_modified(old)).map_err(|e| e.to_string())?;
            client.upload_file(&bucket, &format!("{base}dated.txt"), &path, &Progress::default()).await?;
            let back = dir.join("down/dated.txt");
            client.download_file(&bucket, &format!("{base}dated.txt"), None, &back, &Progress::default()).await?;
            let secs = std::fs::metadata(&back).and_then(|m| m.modified()).map_err(|e| e.to_string())?
                .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            if secs == 1_000_000_000 { Ok(String::new()) } else { Err(format!("got {secs}")) }
        }.await);
        // A move interrupted after its copy arrived and its original was removed finishes cleanly when run again.
        check("resumed move finishes", async {
            let (from, to) = (format!("{base}move-src.txt"), format!("{base}move-dst.txt"));
            client.create_object(&bucket, &to, b"moved".to_vec(), "text/plain").await?;
            crate::window::actions::run_transfer(&client, &client, &bucket, &from, &bucket, &to, 5, true, &Progress::default()).await?;
            // And a move whose original is still there copies and removes it.
            client.create_object(&bucket, &from, b"again".to_vec(), "text/plain").await?;
            crate::window::actions::run_transfer(&client, &client, &bucket, &from, &bucket, &to, 5, true, &Progress::default()).await?;
            let source_gone = client.head_object(&bucket, &from).await.is_err();
            let target = client.read_bytes(&bucket, &to, 100).await?;
            if source_gone && target == b"again" { Ok(String::new()) } else { Err(format!("source gone {source_gone}, target {:?}", String::from_utf8_lossy(&target))) }
        }.await);
        check("copy object", client.copy_object(&bucket, &format!("{base}hello.txt"), &bucket, &format!("{base}copy/hello (copy).txt")).await.map(|_| String::new()));
        check("rename", client.rename(&bucket, &format!("{base}copy/hello (copy).txt"), &format!("{base}copy/renamed.txt")).await.map(|_| String::new()));
        check("create text object", client.create_object(&bucket, &format!("{base}new.txt"), b"pasted".to_vec(), "text/plain; charset=utf-8").await.map(|_| String::new()));
        check("create refuses existing", match client.create_object(&bucket, &format!("{base}new.txt"), Vec::new(), "text/plain").await { Err(e) => Ok(e), Ok(()) => Err("replaced an existing object".into()) });
        check("create folder", client.create_folder(&bucket, &format!("{base}empty/")).await.map(|_| String::new()));
        check("save text (etag check)", match client.head_object(&bucket, &format!("{base}hello.txt")).await {
            Ok(info) => client.save_text(&bucket, &format!("{base}hello.txt"), "edited\n".into(), &info.etag).await.map(|_| String::new()),
            Err(e) => Err(e),
        });
        check("stale etag refused", match client.save_text(&bucket, &format!("{base}hello.txt"), "x".into(), "0000").await { Err(_) => Ok(String::new()), Ok(()) => Err("saved with a stale etag".into()) });
        check("rewrite headers", match client.head_object(&bucket, &format!("{base}hello.txt")).await {
            Ok(mut info) => { info.cache_control = "max-age=60".into(); client.rewrite(&bucket, &info, None).await.map(|_| String::new()) }
            Err(e) => Err(e),
        });
        check("bulk headers", async {
            let keys = vec![format!("{base}hello.txt"), format!("{base}new.txt")];
            let (changed, failed, error) = client.set_headers(&bucket, keys.clone(), Some("public, max-age=60".into()), None, &Progress::default()).await?;
            if failed > 0 { return Err(error); }
            for key in &keys {
                let info = client.head_object(&bucket, key).await?;
                if info.cache_control != "public, max-age=60" { return Err(format!("{key}: {}", info.cache_control)); }
                if info.content_type.is_empty() { return Err(format!("{key} lost its content type")); }
            }
            Ok(format!("{changed} changed"))
        }.await);
        check("presign", client.presign(&bucket, &format!("{base}hello.txt"), 60).await.map(|u| u.split('?').next().unwrap_or("").to_string()));
        check("sync up (add)", client.sync_up(&bucket, &format!("{base}sync/"), &dir, false, &Progress::default()).await.map(|r| format!("{r:?}")));
        check("sync up again skips", client.sync_up(&bucket, &format!("{base}sync/"), &dir, false, &Progress::default()).await.and_then(|r| if r.transferred == 0 { Ok(format!("{r:?}")) } else { Err(format!("re-uploaded {r:?}")) }));
        let _ = client.create_object(&bucket, &format!("{base}sync/only-remote.txt"), b"x".to_vec(), "text/plain").await;
        check("sync plan lists deletions", client.sync_plan(&bucket, &format!("{base}sync/"), &dir, false, true).await
            .and_then(|p| if p.transfer.is_empty() && p.delete == vec!["only-remote.txt".to_string()] { Ok(format!("{} up to date", p.skipped)) } else { Err(format!("{p:?}")) }));
        // A synced folder is up to date right after a sync down.
        check("sync down is idempotent", async {
            let target = std::env::temp_dir().join(format!("ferry-selftest-mirror-{}", std::process::id()));
            std::fs::create_dir_all(&target).map_err(|e| e.to_string())?;
            client.sync_down(&bucket, &format!("{base}sync/"), &target, false, &Progress::default()).await?;
            let again = client.sync_down(&bucket, &format!("{base}sync/"), &target, false, &Progress::default()).await?;
            let _ = std::fs::remove_dir_all(&target);
            if again.transferred == 0 { Ok(format!("{again:?}")) } else { Err(format!("downloaded again: {again:?}")) }
        }.await);
        check("analyze", client.analyze(&bucket, base).await.map(|a| format!("{} objects, {} bytes, {} wasted", a.objects, a.size, a.wasted)));
        if let Some(uploads) = client.incomplete_uploads(&bucket).await {
            let ours: Vec<_> = uploads.into_iter().filter(|u| u.key.starts_with(base)).collect();
            check("abort leftover uploads", client.abort_uploads(&bucket, ours).await.map(|n| format!("{n} aborted")));
        }
        check("delete folder recursively", client.delete_keys(&bucket, vec![base.to_string()]).await.map(|n| format!("{n} deleted")));
        check("folder gone", client.list_all(&bucket, base, 10).await.and_then(|(i, _)| if i.is_empty() { Ok(String::new()) } else { Err(format!("{} left", i.len())) }));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(failures, 0, "{failures} checks failed");
    });
}


#[test]
fn encrypted_backup() {
    let Ok(name) = std::env::var("FERRY_SELFTEST") else { return };
    runtime().block_on(async move {
        let path = std::env::temp_dir().join("ferry-backup-test.json");
        let n = crate::backup::export(path.clone(), crate::backup::Protection::Password("a long test password".into())).await.unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("secretKey"), "secrets must not appear in plain text");
        let data = std::fs::read(&path).unwrap();
        let profiles = crate::backup::password::open(&data, "a long test password").unwrap();
        let p = profiles.iter().find(|p| p.name == name).unwrap();
        println!("exported {n} profiles; {} has a secret of {} characters", p.name, p.secret_key.len());
        assert!(!p.secret_key.is_empty());
        assert!(crate::backup::password::open(&data, "wrong password").is_err());
        let _ = std::fs::remove_file(&path);
    });
}

/// Cryptomator vaults against a saved connection: a vault made by the Cryptomator app,
/// uploaded as it is, has to open; a vault made here has to work for every operation.
/// Works only below "ferry-selftest-vault/" and removes it afterwards.
#[test]
fn vault_live() {
    use crate::s3::vault;
    let Ok(name) = std::env::var("FERRY_SELFTEST") else { return };
    runtime().block_on(async move {
        let stored = profile::load().into_iter().find(|p| p.name == name).expect("no such profile");
        let client = S3::connect(profile::with_secrets(stored).await.expect("secrets")).await.expect("connect");
        let bucket = client.list_buckets().await.unwrap().buckets[0].name.clone();
        let base = "ferry-selftest-vault/";
        let p = Progress::default();
        let mut failures = 0;
        let mut check = |name: &str, ok: bool, detail: String| {
            if ok { println!("OK    {name:<40} {detail}") } else { failures += 1; println!("FAIL  {name:<40} {detail}") }
        };

        // A vault created by the Cryptomator desktop app (SIV_CTRMAC, nested folders).
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/cryptomator-vault/tests/fixtures/vault-ctrmac-1");
        let mut files = Vec::new();
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                if e.path().is_dir() { walk(&e.path(), out) } else { out.push(e.path()) }
            }
        }
        walk(&fixture, &mut files);
        let real = format!("{base}real/");
        for file in &files {
            let rel = file.strip_prefix(&fixture).unwrap().to_string_lossy().to_string();
            client.raw().upload_file(&bucket, &format!("{real}{rel}"), file, &p).await.unwrap();
        }
        let wrong = vault::unlock(&client, &bucket, &real, "wrong".into()).await;
        check("wrong password is refused", wrong.as_ref().is_err_and(|e| e.contains("Wrong")), format!("{:?}", wrong.err()));
        let unlocked = vault::unlock(&client, &bucket, &real, "qq11@@11".into()).await;
        check("Cryptomator vault unlocks", unlocked.is_ok(), format!("{:?}", unlocked.err()));
        let root = client.list_objects(&bucket, &real, "").await.unwrap_or_default();
        let dirs = root.items.iter().filter(|e| e.is_folder).count();
        let names: Vec<_> = root.items.iter().map(|e| e.name.clone()).collect();
        check("its root lists 4 folders and WELCOME.rtf", dirs == 4 && names.contains(&"WELCOME.rtf".to_string()), format!("{names:?}"));
        let welcome = client.read_bytes(&bucket, &format!("{real}WELCOME.rtf"), 1_000_000).await.unwrap_or_default();
        check("WELCOME.rtf decrypts", String::from_utf8_lossy(&welcome).contains("Cryptomator"), format!("{} bytes", welcome.len()));
        let (all, _) = client.list_all(&bucket, &real, usize::MAX).await.unwrap_or_default();
        check("nested folders are walked", all.iter().any(|e| e.key[real.len()..].matches('/').count() >= 3), format!("{} entries", all.len()));
        vault::lock(&client.profile.id, &bucket, &real);
        let locked = client.list_objects(&bucket, &real, "").await.unwrap_or_default();
        check("locked again it shows the encrypted files", locked.items.iter().any(|e| e.name == "masterkey.cryptomator"), String::new());

        // A vault made here.
        let new = format!("{base}new/");
        let created = vault::create(&client, &bucket, &new, "a long vault password".into()).await;
        check("create a vault", created.is_ok(), format!("{:?}", created.err()));
        let opened = vault::unlock(&client, &bucket, &new, "a long vault password".into()).await;
        check("unlock it", opened.is_ok(), format!("{:?}", opened.err()));
        let dir = std::env::temp_dir().join(format!("ferry-vault-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.join("a.bin"), &data).unwrap();
        check("create folder", client.create_folder(&bucket, &format!("{new}docs/")).await.is_ok(), String::new());
        let up = client.upload_file(&bucket, &format!("{new}docs/a.bin"), &dir.join("a.bin"), &p).await;
        check("upload into it", up.is_ok(), format!("{:?}", up.err()));
        let deep = client.upload_file(&bucket, &format!("{new}x/y/z.bin"), &dir.join("a.bin"), &p).await;
        check("upload creates missing folders", deep.is_ok(), format!("{:?}", deep.err()));
        let long = format!("{}.txt", "long name ".repeat(25));
        let up_long = client.create_object(&bucket, &format!("{new}{long}"), b"long".to_vec(), "text/plain").await;
        check("long names are shortened", up_long.is_ok(), format!("{:?}", up_long.err()));
        let listed = client.list_objects(&bucket, &new, "").await.unwrap_or_default();
        let names: Vec<_> = listed.items.iter().map(|e| e.name.clone()).collect();
        check("root lists the cleartext names", names.contains(&"docs".into()) && names.contains(&"x".into()) && names.contains(&long), format!("{} items", names.len()));
        let head = client.head_object(&bucket, &format!("{new}docs/a.bin")).await;
        check("size is the cleartext size", head.as_ref().is_ok_and(|h| h.size == 100_000), format!("{:?}", head.map(|h| h.size)));
        let part = client.read_bytes(&bucket, &format!("{new}docs/a.bin"), 40_000).await.unwrap_or_default();
        check("reading the start decrypts", part == data[..40_000], format!("{} bytes", part.len()));
        let back = dir.join("back.bin");
        let down = client.download_file(&bucket, &format!("{new}docs/a.bin"), None, &back, &p).await;
        check("download decrypts", down.is_ok() && std::fs::read(&back).unwrap_or_default() == data, format!("{:?}", down.err()));
        let renamed = client.rename(&bucket, &format!("{new}docs/a.bin"), &format!("{new}docs/b.bin")).await;
        check("rename a file", renamed.is_ok() && client.head_object(&bucket, &format!("{new}docs/b.bin")).await.is_ok(), format!("{:?}", renamed.err()));
        let moved = client.rename_folder(&bucket, &format!("{new}docs/"), &format!("{new}papers/")).await;
        let inside = client.list_objects(&bucket, &format!("{new}papers/"), "").await.unwrap_or_default();
        check("rename a folder keeps its contents", moved.is_ok() && inside.items.iter().any(|e| e.name == "b.bin"), format!("{:?}", moved.err()));
        let copied = client.copy_object(&bucket, &format!("{new}papers/b.bin"), &bucket, &format!("{new}x/copy.bin")).await;
        let copy_data = client.read_bytes(&bucket, &format!("{new}x/copy.bin"), 200_000).await.unwrap_or_default();
        check("copy a file", copied.is_ok() && copy_data == data, format!("{:?}", copied.err()));
        let out_copy = client.copy_object(&bucket, &format!("{new}x/copy.bin"), &bucket, &format!("{base}plain.bin")).await;
        let plain = client.read_bytes(&bucket, &format!("{base}plain.bin"), 200_000).await.unwrap_or_default();
        check("copy out of the vault decrypts", out_copy.is_ok() && plain == data, format!("{:?}", out_copy.err()));
        let raw = client.raw().list_all(&bucket, &new, usize::MAX).await.unwrap_or_default().0;
        check("no cleartext names are stored", !raw.iter().any(|e| e.key.contains("papers") || e.key.contains("long name") || e.key.contains(".bin")), format!("{} objects", raw.len()));
        let deleted = client.delete_keys(&bucket, vec![format!("{new}x/")]).await;
        let after = client.list_objects(&bucket, &new, "").await.unwrap_or_default();
        check("delete a folder with contents", deleted.is_ok() && !after.items.iter().any(|e| e.name == "x"), format!("{:?}", deleted));
        let gone = client.raw().list_all(&bucket, &new, usize::MAX).await.unwrap_or_default().0.len();
        check("its encrypted objects are gone too", gone < raw.len(), format!("{} → {gone}", raw.len()));
        check("sharing links is refused inside", client.presign(&bucket, &format!("{new}papers/b.bin"), 60).await.is_err(), String::new());

        vault::lock(&client.profile.id, &bucket, &new);
        let _ = std::fs::remove_dir_all(&dir);
        let cleaned = client.delete_keys(&bucket, vec![base.to_string()]).await;
        check("remove the test folder", cleaned.is_ok(), format!("{:?}", cleaned));
        assert_eq!(failures, 0, "{failures} checks failed");
    });
}
