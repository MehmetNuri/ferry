#![cfg(test)]

use crate::profile;
use crate::runtime::runtime;
use crate::s3::S3;

#[test]
fn probe() {
    let Ok(name) = std::env::var("FERRY_PROBE") else { return };
    runtime().block_on(async move {
        let stored = profile::load().into_iter().find(|p| p.name == name).expect("no such profile");
        let client = S3::connect(profile::with_secrets(stored).await.expect("secrets")).await.expect("connect");
        let report = |op: &str, result: Result<String, String>| match result {
            Ok(text) => println!("OK    {op:<28} {text}"),
            Err(error) => println!("FAIL  {op:<28} {error}"),
        };
        let buckets = client.list_buckets().await;
        report("ListBuckets", buckets.as_ref().map(|b| format!("{:?} warning={:?}", b.buckets.iter().map(|x| &x.name).collect::<Vec<_>>(), b.warning)).map_err(Clone::clone));
        let Some(bucket) = buckets.ok().and_then(|b| b.buckets.first().map(|x| x.name.clone())) else { return };
        println!("URL   {}", client.presign(&bucket, "probe.txt", 60).await.unwrap_or_default().split('?').next().unwrap_or(""));
        let listing = client.list_objects(&bucket, "", "").await;
        report("ListObjectsV2 /", listing.as_ref().map(|l| format!("{} items: {:?}", l.items.len(), l.items.iter().take(5).map(|i| &i.key).collect::<Vec<_>>())).map_err(Clone::clone));
        let items = listing.map(|l| l.items).unwrap_or_default();
        if let Some(folder) = items.iter().find(|i| i.is_folder) {
            report("ListObjectsV2 folder", client.list_objects(&bucket, &folder.key, "").await.map(|l| format!("{} items in {}", l.items.len(), folder.key)));
        }
        report("ListAll", client.list_all(&bucket, "", 1000).await.map(|(i, t)| format!("{} objects truncated={t}: {:?}", i.len(), i.iter().map(|e| e.key.as_str()).collect::<Vec<_>>())));
        if let Some(file) = items.iter().find(|i| !i.is_folder) {
            report("HeadObject", client.head_object(&bucket, &file.key).await.map(|i| format!("{} {} bytes {}", i.key, i.size, i.content_type)));
            report("GetObject range", client.read_bytes(&bucket, &file.key, 64).await.map(|b| format!("{} bytes", b.len())));
            report("GetObjectTagging", client.object_tags(&bucket, &file.key).await.map(|t| format!("{t:?}")));
            report("ListObjectVersions", client.list_versions(&bucket, &file.key).await.map(|v| format!("{} versions", v.len())));
            report("GetObjectAcl", client.object_acl(&bucket, &file.key).await.map(|a| format!("{} grants", a.grants.len())));
            report("Presign", client.presign(&bucket, &file.key, 60).await.map(|u| u.chars().take(60).collect()));
        }
        report("GetBucketVersioning", client.versioning(&bucket).await);
        let settings = client.bucket_settings(&bucket).await;
        println!("INFO  BucketSettings region={:?}\n      versioning={:?}\n      encryption={:?}\n      policy={:?}\n      cors={:?}\n      lifecycle={:?}\n      tags={:?}",
            settings.region, settings.versioning, settings.encryption, settings.policy.note(), settings.cors.note(), settings.lifecycle.note(), settings.tags.note());
        report("ListMultipartUploads", Ok(format!("{:?}", client.incomplete_uploads(&bucket).await.map(|u| u.len()))));
        report("GetBucketAcl", client.bucket_acl(&bucket).await.map(|a| format!("{} grants", a.grants.len())));
        report("GetObjectLockConfig", client.lock_config(&bucket).await.map(|l| format!("{l:?}")));
        let hosting = client.hosting(&bucket).await;
        println!("INFO  Hosting {hosting:?}");
    });
}
