//! Settings, versions, analysis and sync on top of the basic operations in s3.rs.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use aws_sdk_s3::types::{
    AbortIncompleteMultipartUpload, BucketLifecycleConfiguration, BucketVersioningStatus, CorsConfiguration, CorsRule,
    ExpirationStatus, LifecycleExpiration, LifecycleRule, LifecycleRuleFilter, NoncurrentVersionExpiration,
    ServerSideEncryption, ServerSideEncryptionByDefault, ServerSideEncryptionConfiguration, ServerSideEncryptionRule,
    Tag, Tagging, Transition, TransitionStorageClass, VersioningConfiguration,
};
use serde_json::{Value, json};

use crate::i18n::{tr, trf};
use crate::s3::{Entry, Progress, Res, S3, describe, error_code};

#[derive(Clone, Debug, Default)]
pub struct Version {
    pub version_id: String,
    pub modified: i64,
    pub size: i64,
    pub is_latest: bool,
    pub delete_marker: bool,
}

/// A bucket setting that could be read, or why the provider refused it.
#[derive(Clone, Debug)]
pub enum Setting<T> {
    Value(T),
    Unsupported(String),
}

impl<T: Default> Setting<T> {
    pub fn value(&self) -> Option<&T> {
        match self { Setting::Value(v) => Some(v), Setting::Unsupported(_) => None }
    }
    pub fn note(&self) -> Option<&str> {
        match self { Setting::Value(_) => None, Setting::Unsupported(s) => Some(s) }
    }
}

#[derive(Clone, Debug)]
pub struct BucketSettings {
    pub region: String,
    /// "Enabled", "Suspended" or "" (never enabled).
    pub versioning: Setting<String>,
    pub encryption: Setting<(String, String)>,
    pub policy: Setting<String>,
    pub cors: Setting<String>,
    pub lifecycle: Setting<String>,
    pub tags: Setting<Vec<(String, String)>>,
}

#[derive(Clone, Debug, Default)]
pub struct AnalysisEntry {
    pub name: String,
    pub size: i64,
    pub objects: i64,
}

#[derive(Clone, Debug, Default)]
pub struct DuplicateGroup {
    pub size: i64,
    pub copies: i64,
    pub wasted: i64,
    pub keys: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Analysis {
    pub objects: i64,
    pub size: i64,
    pub truncated: bool,
    pub wasted: i64,
    pub folders: Vec<AnalysisEntry>,
    pub types: Vec<AnalysisEntry>,
    pub ages: Vec<AnalysisEntry>,
    pub classes: Vec<AnalysisEntry>,
    pub largest: Vec<Entry>,
    pub duplicates: Vec<DuplicateGroup>,
}

#[derive(Clone, Debug, Default)]
pub struct IncompleteUpload {
    pub key: String,
    pub upload_id: String,
    pub initiated: i64,
    pub stale: bool,
}

#[derive(Clone, Debug, Default)]
pub struct SyncPlan {
    /// Relative paths to transfer, with their sizes.
    pub transfer: Vec<(String, u64)>,
    /// Relative paths of objects or local files that would be deleted.
    pub delete: Vec<String>,
    pub skipped: usize,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SyncResult {
    pub transferred: usize,
    pub skipped: usize,
    pub deleted: usize,
    pub failed: usize,
}

/// Errors that mean "this provider has no such feature" rather than a real failure.
fn missing<E: aws_sdk_s3::error::ProvideErrorMetadata>(error: &aws_sdk_s3::error::SdkError<E>, empty_codes: &[&str]) -> bool {
    empty_codes.contains(&error_code(error).as_str())
}

fn tags_of(tags: &[Tag]) -> Vec<(String, String)> {
    let mut list: Vec<(String, String)> = tags.iter().map(|t| (t.key().to_string(), t.value().to_string())).collect();
    list.sort();
    list
}

fn tagging(tags: &[(String, String)]) -> Res<Tagging> {
    if tags.len() > 50 {
        return Err(trf("At most {n} tags can be set", &[("n", "50")]));
    }
    let mut set = Vec::new();
    for (key, value) in tags {
        if key.is_empty() || key.chars().count() > 128 || value.chars().count() > 256 {
            return Err(tr("A tag key cannot be empty; keys are limited to 128 and values to 256 characters"));
        }
        set.push(Tag::builder().key(key).value(value).build().map_err(|e| e.to_string())?);
    }
    Tagging::builder().set_tag_set(Some(set)).build().map_err(|e| e.to_string())
}

fn strings(value: &Value, name: &str) -> Option<Vec<String>> {
    value.get(name).and_then(Value::as_array).map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
}

fn int(value: &Value, path: &[&str]) -> Option<i32> {
    let mut current = value;
    for part in path {
        current = current.get(part)?;
    }
    current.as_i64().map(|n| n as i32)
}

fn rules_array(text: &str) -> Res<Vec<Value>> {
    if text.len() > 64 * 1024 {
        return Err(tr("The rule text is too large"));
    }
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Array(rules)) => Ok(rules),
        Ok(_) => Err(trf("The rules could not be read: {error}", &[("error", &tr("a JSON array is expected"))])),
        Err(error) => Err(trf("The rules could not be read: {error}", &[("error", &error.to_string())])),
    }
}

impl S3 {
    pub async fn list_versions(&self, bucket: &str, key: &str) -> Res<Vec<Version>> {
        let out = self.client.list_object_versions().bucket(bucket).prefix(key).max_keys(1000).send().await.map_err(describe)?;
        let mut versions = Vec::new();
        for v in out.versions().iter().filter(|v| v.key() == Some(key)) {
            versions.push(Version {
                version_id: v.version_id().unwrap_or("null").to_string(),
                modified: v.last_modified().map(|d| d.secs()).unwrap_or(0),
                size: v.size().unwrap_or(0),
                is_latest: v.is_latest().unwrap_or(false),
                delete_marker: false,
            });
        }
        for m in out.delete_markers().iter().filter(|m| m.key() == Some(key)) {
            versions.push(Version {
                version_id: m.version_id().unwrap_or("null").to_string(),
                modified: m.last_modified().map(|d| d.secs()).unwrap_or(0),
                is_latest: m.is_latest().unwrap_or(false),
                delete_marker: true,
                ..Default::default()
            });
        }
        versions.sort_by(|a, b| b.modified.cmp(&a.modified));
        Ok(versions)
    }

    /// Objects below a folder whose latest version is a delete marker: deleted, but
    /// recoverable in a versioned bucket. Returns (key, marker version, deleted at).
    pub async fn deleted_objects(&self, bucket: &str, prefix: &str) -> Res<Vec<(String, String, i64)>> {
        let mut found = Vec::new();
        let (mut key_marker, mut version_marker): (Option<String>, Option<String>) = (None, None);
        loop {
            let out = self.client.list_object_versions().bucket(bucket).prefix(prefix).max_keys(1000)
                .set_key_marker(key_marker.clone()).set_version_id_marker(version_marker.clone())
                .send().await.map_err(describe)?;
            for m in out.delete_markers().iter().filter(|m| m.is_latest().unwrap_or(false)) {
                if let (Some(key), Some(version)) = (m.key(), m.version_id()) {
                    found.push((key.to_string(), version.to_string(), m.last_modified().map(|d| d.secs()).unwrap_or(0)));
                }
            }
            if !out.is_truncated().unwrap_or(false) || found.len() > 10_000 { break; }
            key_marker = out.next_key_marker().map(str::to_string);
            version_marker = out.next_version_id_marker().map(str::to_string);
            if key_marker.is_none() { break; }
        }
        found.sort_by(|a, b| b.2.cmp(&a.2));
        Ok(found)
    }

    /// Brings deleted objects back by removing their delete markers.
    pub async fn undelete(&self, bucket: &str, markers: Vec<(String, String)>) -> Res<usize> {
        let count = markers.len();
        for (key, version) in markers {
            self.client.delete_object().bucket(bucket).key(&key).version_id(&version).send().await.map_err(describe)?;
        }
        Ok(count)
    }

    /// Makes an older version current again by copying it over the object.
    pub async fn restore_version(&self, bucket: &str, key: &str, version: &str) -> Res<()> {
        self.send_copy(self.client.copy_object().bucket(bucket).key(key), bucket, key, Some(version)).await
    }

    pub async fn delete_version(&self, bucket: &str, key: &str, version: &str) -> Res<()> {
        self.client.delete_object().bucket(bucket).key(key).version_id(version).send().await.map_err(describe)?;
        Ok(())
    }

    pub async fn versioning(&self, bucket: &str) -> Res<String> {
        let out = self.client.get_bucket_versioning().bucket(bucket).send().await.map_err(describe)?;
        Ok(out.status().map(|s| s.as_str().to_string()).unwrap_or_default())
    }

    pub async fn set_versioning(&self, bucket: &str, enabled: bool) -> Res<()> {
        let status = if enabled { BucketVersioningStatus::Enabled } else { BucketVersioningStatus::Suspended };
        self.client.put_bucket_versioning().bucket(bucket)
            .versioning_configuration(VersioningConfiguration::builder().status(status).build())
            .send().await.map_err(describe)?;
        Ok(())
    }

    pub async fn object_tags(&self, bucket: &str, key: &str) -> Res<Vec<(String, String)>> {
        let out = self.client.get_object_tagging().bucket(bucket).key(key).send().await.map_err(describe)?;
        Ok(tags_of(out.tag_set()))
    }

    pub async fn set_object_tags(&self, bucket: &str, key: &str, tags: &[(String, String)]) -> Res<()> {
        if tags.is_empty() {
            self.client.delete_object_tagging().bucket(bucket).key(key).send().await.map_err(describe)?;
        } else {
            self.client.put_object_tagging().bucket(bucket).key(key).tagging(tagging(tags)?).send().await.map_err(describe)?;
        }
        Ok(())
    }

    pub async fn bucket_settings(&self, bucket: &str) -> BucketSettings {
        let client = &self.client;
        let region = match client.get_bucket_location().bucket(bucket).send().await {
            Ok(out) => out.location_constraint().map(|c| c.as_str().to_string()).filter(|r| !r.is_empty()).unwrap_or_else(|| "us-east-1".into()),
            Err(_) => String::new(),
        };
        let versioning = match client.get_bucket_versioning().bucket(bucket).send().await {
            Ok(out) => Setting::Value(out.status().map(|s| s.as_str().to_string()).unwrap_or_default()),
            Err(e) => Setting::Unsupported(describe(e)),
        };
        let encryption = match client.get_bucket_encryption().bucket(bucket).send().await {
            Ok(out) => {
                let rule = out.server_side_encryption_configuration().and_then(|c| c.rules().first()).and_then(|r| r.apply_server_side_encryption_by_default());
                Setting::Value(rule.map(|r| (r.sse_algorithm().as_str().to_string(), r.kms_master_key_id().unwrap_or_default().to_string())).unwrap_or_default())
            }
            Err(e) if missing(&e, &["ServerSideEncryptionConfigurationNotFoundError"]) => Setting::Value(Default::default()),
            Err(e) => Setting::Unsupported(describe(e)),
        };
        let policy = match client.get_bucket_policy().bucket(bucket).send().await {
            Ok(out) => {
                let text = out.policy().unwrap_or_default();
                Setting::Value(serde_json::from_str::<Value>(text).ok().and_then(|v| serde_json::to_string_pretty(&v).ok()).unwrap_or_else(|| text.to_string()))
            }
            Err(e) if missing(&e, &["NoSuchBucketPolicy"]) => Setting::Value(String::new()),
            Err(e) => Setting::Unsupported(describe(e)),
        };
        let cors = match client.get_bucket_cors().bucket(bucket).send().await {
            Ok(out) => {
                let rules: Vec<Value> = out.cors_rules().iter().map(|r| {
                    let mut rule = json!({ "AllowedOrigins": r.allowed_origins(), "AllowedMethods": r.allowed_methods() });
                    if !r.allowed_headers().is_empty() { rule["AllowedHeaders"] = json!(r.allowed_headers()); }
                    if !r.expose_headers().is_empty() { rule["ExposeHeaders"] = json!(r.expose_headers()); }
                    if let Some(age) = r.max_age_seconds() { rule["MaxAgeSeconds"] = json!(age); }
                    if let Some(id) = r.id() { rule["ID"] = json!(id); }
                    rule
                }).collect();
                Setting::Value(serde_json::to_string_pretty(&rules).unwrap_or_default())
            }
            Err(e) if missing(&e, &["NoSuchCORSConfiguration"]) => Setting::Value(String::new()),
            Err(e) => Setting::Unsupported(describe(e)),
        };
        let lifecycle = match client.get_bucket_lifecycle_configuration().bucket(bucket).send().await {
            Ok(out) => {
                let rules: Vec<Value> = out.rules().iter().map(|r| {
                    let mut rule = json!({ "ID": r.id().unwrap_or_default(), "Status": r.status().as_str(),
                        "Filter": { "Prefix": r.filter().and_then(|f| f.prefix()).unwrap_or_default() } });
                    if let Some(days) = r.expiration().and_then(|e| e.days()) { rule["Expiration"] = json!({ "Days": days }); }
                    let transitions: Vec<Value> = r.transitions().iter().map(|t| json!({ "Days": t.days(), "StorageClass": t.storage_class().map(|c| c.as_str()) })).collect();
                    if !transitions.is_empty() { rule["Transitions"] = json!(transitions); }
                    if let Some(days) = r.noncurrent_version_expiration().and_then(|e| e.noncurrent_days()) { rule["NoncurrentVersionExpiration"] = json!({ "NoncurrentDays": days }); }
                    if let Some(days) = r.abort_incomplete_multipart_upload().and_then(|a| a.days_after_initiation()) { rule["AbortIncompleteMultipartUpload"] = json!({ "DaysAfterInitiation": days }); }
                    rule
                }).collect();
                Setting::Value(serde_json::to_string_pretty(&rules).unwrap_or_default())
            }
            Err(e) if missing(&e, &["NoSuchLifecycleConfiguration"]) => Setting::Value(String::new()),
            Err(e) => Setting::Unsupported(describe(e)),
        };
        let tags = match client.get_bucket_tagging().bucket(bucket).send().await {
            Ok(out) => Setting::Value(tags_of(out.tag_set())),
            Err(e) if missing(&e, &["NoSuchTagSet", "NoSuchTagSetError"]) => Setting::Value(Vec::new()),
            Err(e) => Setting::Unsupported(describe(e)),
        };
        BucketSettings { region, versioning, encryption, policy, cors, lifecycle, tags }
    }

    pub async fn set_bucket_policy(&self, bucket: &str, policy: &str) -> Res<()> {
        if policy.trim().is_empty() {
            self.client.delete_bucket_policy().bucket(bucket).send().await.map_err(describe)?;
            return Ok(());
        }
        if policy.len() > 20 * 1024 || serde_json::from_str::<Value>(policy).is_err() {
            return Err(tr("The policy must be a valid JSON document of at most 20 KB"));
        }
        self.client.put_bucket_policy().bucket(bucket).policy(policy).send().await.map_err(describe)?;
        Ok(())
    }

    pub async fn set_bucket_cors(&self, bucket: &str, text: &str) -> Res<()> {
        let rules = rules_array(text)?;
        if rules.is_empty() {
            self.client.delete_bucket_cors().bucket(bucket).send().await.map_err(describe)?;
            return Ok(());
        }
        let mut built = Vec::new();
        for rule in &rules {
            built.push(CorsRule::builder()
                .set_id(rule.get("ID").and_then(Value::as_str).map(str::to_string))
                .set_allowed_origins(strings(rule, "AllowedOrigins"))
                .set_allowed_methods(strings(rule, "AllowedMethods"))
                .set_allowed_headers(strings(rule, "AllowedHeaders"))
                .set_expose_headers(strings(rule, "ExposeHeaders"))
                .set_max_age_seconds(int(rule, &["MaxAgeSeconds"]))
                .build().map_err(|e| trf("The rules could not be read: {error}", &[("error", &e.to_string())]))?);
        }
        let config = CorsConfiguration::builder().set_cors_rules(Some(built)).build().map_err(|e| e.to_string())?;
        self.client.put_bucket_cors().bucket(bucket).cors_configuration(config).send().await.map_err(describe)?;
        Ok(())
    }

    pub async fn set_bucket_lifecycle(&self, bucket: &str, text: &str) -> Res<()> {
        let rules = rules_array(text)?;
        if rules.is_empty() {
            self.client.delete_bucket_lifecycle().bucket(bucket).send().await.map_err(describe)?;
            return Ok(());
        }
        let mut built = Vec::new();
        for rule in &rules {
            let prefix = rule.get("Filter").and_then(|f| f.get("Prefix")).or_else(|| rule.get("Prefix")).and_then(Value::as_str).unwrap_or_default();
            let mut builder = LifecycleRule::builder()
                .set_id(rule.get("ID").and_then(Value::as_str).map(str::to_string))
                .status(ExpirationStatus::from(rule.get("Status").and_then(Value::as_str).unwrap_or("Enabled")))
                .filter(LifecycleRuleFilter::builder().prefix(prefix).build());
            if let Some(days) = int(rule, &["Expiration", "Days"]) {
                builder = builder.expiration(LifecycleExpiration::builder().days(days).build());
            }
            if let Some(transitions) = rule.get("Transitions").and_then(Value::as_array) {
                for t in transitions {
                    builder = builder.transitions(Transition::builder().set_days(int(t, &["Days"]))
                        .set_storage_class(t.get("StorageClass").and_then(Value::as_str).map(TransitionStorageClass::from)).build());
                }
            }
            if let Some(days) = int(rule, &["NoncurrentVersionExpiration", "NoncurrentDays"]) {
                builder = builder.noncurrent_version_expiration(NoncurrentVersionExpiration::builder().noncurrent_days(days).build());
            }
            if let Some(days) = int(rule, &["AbortIncompleteMultipartUpload", "DaysAfterInitiation"]) {
                builder = builder.abort_incomplete_multipart_upload(AbortIncompleteMultipartUpload::builder().days_after_initiation(days).build());
            }
            built.push(builder.build().map_err(|e| trf("The rules could not be read: {error}", &[("error", &e.to_string())]))?);
        }
        let config = BucketLifecycleConfiguration::builder().set_rules(Some(built)).build().map_err(|e| e.to_string())?;
        self.client.put_bucket_lifecycle_configuration().bucket(bucket).lifecycle_configuration(config).send().await.map_err(describe)?;
        Ok(())
    }

    pub async fn set_bucket_tags(&self, bucket: &str, tags: &[(String, String)]) -> Res<()> {
        if tags.is_empty() {
            self.client.delete_bucket_tagging().bucket(bucket).send().await.map_err(describe)?;
        } else {
            self.client.put_bucket_tagging().bucket(bucket).tagging(tagging(tags)?).send().await.map_err(describe)?;
        }
        Ok(())
    }

    pub async fn set_bucket_encryption(&self, bucket: &str, algorithm: &str, kms_key: &str) -> Res<()> {
        if algorithm.is_empty() {
            self.client.delete_bucket_encryption().bucket(bucket).send().await.map_err(describe)?;
            return Ok(());
        }
        let default = ServerSideEncryptionByDefault::builder().sse_algorithm(ServerSideEncryption::from(algorithm))
            .set_kms_master_key_id((algorithm == "aws:kms" && !kms_key.is_empty()).then(|| kms_key.to_string()))
            .build().map_err(|e| e.to_string())?;
        let config = ServerSideEncryptionConfiguration::builder()
            .rules(ServerSideEncryptionRule::builder().apply_server_side_encryption_by_default(default).build())
            .build().map_err(|e| e.to_string())?;
        self.client.put_bucket_encryption().bucket(bucket).server_side_encryption_configuration(config).send().await.map_err(describe)?;
        Ok(())
    }

    /// Space by folder, type, age and class, the largest objects and duplicates.
    pub async fn analyze(&self, bucket: &str, prefix: &str) -> Res<Analysis> {
        let (items, truncated) = self.list_all(bucket, prefix, crate::s3::SCAN_LIMIT).await?;
        let now = glib_now();
        let mut analysis = Analysis { truncated, ..Default::default() };
        let mut folders: HashMap<String, AnalysisEntry> = HashMap::new();
        let mut types: HashMap<String, AnalysisEntry> = HashMap::new();
        let mut classes: HashMap<String, AnalysisEntry> = HashMap::new();
        let mut ages: Vec<AnalysisEntry> = ["30d", "90d", "1y", "older"].iter().map(|n| AnalysisEntry { name: n.to_string(), ..Default::default() }).collect();
        let mut by_etag: HashMap<(String, i64), Vec<String>> = HashMap::new();
        for item in items.iter().filter(|e| !e.key.ends_with('/')) {
            analysis.objects += 1;
            analysis.size += item.size;
            let relative = item.key.strip_prefix(prefix).unwrap_or(&item.key);
            let folder = relative.split_once('/').map(|(f, _)| format!("{f}/")).unwrap_or_default();
            let add = |map: &mut HashMap<String, AnalysisEntry>, name: String| {
                let entry = map.entry(name.clone()).or_insert_with(|| AnalysisEntry { name, ..Default::default() });
                entry.size += item.size;
                entry.objects += 1;
            };
            add(&mut folders, folder);
            let name = relative.rsplit('/').next().unwrap_or("");
            let ext = name.rsplit_once('.').filter(|(base, ext)| !base.is_empty() && ext.len() <= 10).map(|(_, e)| e.to_lowercase()).unwrap_or_default();
            add(&mut types, ext);
            add(&mut classes, if item.storage_class.is_empty() { "STANDARD".into() } else { item.storage_class.clone() });
            let days = (now - item.modified) / 86_400;
            let bucket_index = if days <= 30 { 0 } else if days <= 90 { 1 } else if days <= 365 { 2 } else { 3 };
            ages[bucket_index].size += item.size;
            ages[bucket_index].objects += 1;
            // Multipart ETags are not content hashes of the whole object; they still match for identical uploads.
            if item.size > 0 && !item.etag.is_empty() {
                by_etag.entry((item.etag.clone(), item.size)).or_default().push(item.key.clone());
            }
        }
        let sorted = |map: HashMap<String, AnalysisEntry>, limit: usize| {
            let mut list: Vec<AnalysisEntry> = map.into_values().collect();
            list.sort_by(|a, b| b.size.cmp(&a.size));
            if list.len() > limit {
                let rest = list.split_off(limit - 1);
                list.push(AnalysisEntry { name: "*".into(), size: rest.iter().map(|e| e.size).sum(), objects: rest.iter().map(|e| e.objects).sum() });
            }
            list
        };
        analysis.folders = sorted(folders, 15);
        analysis.types = sorted(types, 12);
        analysis.classes = sorted(classes, 10);
        analysis.ages = ages;
        let mut largest: Vec<Entry> = items.into_iter().filter(|e| !e.key.ends_with('/')).collect();
        largest.sort_by(|a, b| b.size.cmp(&a.size));
        largest.truncate(15);
        analysis.largest = largest;
        let mut duplicates: Vec<DuplicateGroup> = by_etag.into_iter().filter(|(_, keys)| keys.len() > 1).map(|((_, size), mut keys)| {
            keys.sort();
            let copies = keys.len() as i64;
            keys.truncate(6);
            DuplicateGroup { size, copies, wasted: size * (copies - 1), keys }
        }).collect();
        duplicates.sort_by(|a, b| b.wasted.cmp(&a.wasted));
        analysis.wasted = duplicates.iter().map(|d| d.wasted).sum();
        duplicates.truncate(20);
        analysis.duplicates = duplicates;
        Ok(analysis)
    }

    /// Unfinished multipart uploads; None when the service cannot list them.
    pub async fn incomplete_uploads(&self, bucket: &str) -> Option<Vec<IncompleteUpload>> {
        let out = self.client.list_multipart_uploads().bucket(bucket).send().await.ok()?;
        let now = glib_now();
        Some(out.uploads().iter().map(|u| {
            let initiated = u.initiated().map(|d| d.secs()).unwrap_or(0);
            IncompleteUpload { key: u.key().unwrap_or_default().into(), upload_id: u.upload_id().unwrap_or_default().into(), initiated, stale: now - initiated > 86_400 }
        }).collect())
    }

    pub async fn abort_uploads(&self, bucket: &str, uploads: Vec<IncompleteUpload>) -> Res<usize> {
        let mut aborted = 0;
        for upload in uploads {
            self.client.abort_multipart_upload().bucket(bucket).key(&upload.key).upload_id(&upload.upload_id).send().await.map_err(describe)?;
            aborted += 1;
        }
        Ok(aborted)
    }

    /// What a sync would do, without doing it, so the user can review deletions first.
    pub async fn sync_plan(&self, bucket: &str, prefix: &str, dir: &Path, down: bool, mirror: bool) -> Res<SyncPlan> {
        let local = local_files(dir)?;
        let (remote, truncated) = self.list_all(bucket, prefix, usize::MAX).await?;
        let mut plan = SyncPlan::default();
        if down {
            let mut wanted = HashSet::new();
            for entry in remote.iter().filter(|e| !e.key.ends_with('/')) {
                let Ok(path) = crate::s3::download_target(dir, prefix, &entry.key) else { continue };
                let relative = path.strip_prefix(dir).map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
                let unchanged = local.get(&relative).is_some_and(|(_, size, modified)| *size as i64 == entry.size && *modified >= entry.modified);
                if unchanged { plan.skipped += 1; } else { plan.transfer.push((relative.clone(), entry.size.max(0) as u64)); }
                wanted.insert(relative);
            }
            if mirror && !truncated && !wanted.is_empty() {
                plan.delete = local.keys().filter(|rel| !wanted.contains(*rel)).cloned().collect();
            }
        } else {
            let remote: HashMap<String, Entry> = remote.into_iter().map(|e| (e.key.clone(), e)).collect();
            for (relative, (_, size, modified)) in &local {
                let unchanged = remote.get(&format!("{prefix}{relative}")).is_some_and(|r| r.size == *size as i64 && r.modified >= *modified);
                if unchanged { plan.skipped += 1; } else { plan.transfer.push((relative.clone(), *size)); }
            }
            if mirror && !truncated && !local.is_empty() {
                plan.delete = remote.keys().filter(|k| !k.ends_with('/')).map(|k| k.strip_prefix(prefix).unwrap_or(k).to_string())
                    .filter(|rel| !local.contains_key(rel) && !rel.split('/').any(crate::s3::skipped)).collect();
            }
        }
        plan.transfer.sort();
        plan.delete.sort();
        Ok(plan)
    }

    /// Uploads new and changed files of a folder; with `mirror`, deletes objects the folder no longer has.
    pub async fn sync_up(&self, bucket: &str, prefix: &str, dir: &Path, mirror: bool, progress: &Progress) -> Res<SyncResult> {
        let local = local_files(dir)?;
        let (remote, truncated) = self.list_all(bucket, prefix, usize::MAX).await?;
        let remote: HashMap<String, Entry> = remote.into_iter().map(|e| (e.key.clone(), e)).collect();
        let mut result = SyncResult::default();
        for (relative, (path, size, modified)) in &local {
            progress.check_public()?;
            let key = format!("{prefix}{relative}");
            let unchanged = remote.get(&key).is_some_and(|r| r.size == *size as i64 && r.modified >= *modified);
            if unchanged {
                result.skipped += 1;
                continue;
            }
            match self.upload_file(bucket, &key, path, progress).await {
                Ok(()) => result.transferred += 1,
                Err(e) if e == crate::s3::CANCELLED => return Err(e),
                Err(_) => result.failed += 1,
            }
        }
        if mirror && !truncated {
            // Objects the source leaves out on purpose (skipped names) are not mirrored away.
            let extra: Vec<String> = remote.keys().filter(|k| !k.ends_with('/'))
                .filter(|k| { let rel = k.strip_prefix(prefix).unwrap_or(k); !local.contains_key(rel) && !rel.split('/').any(crate::s3::skipped) })
                .cloned().collect();
            if local.is_empty() && !extra.is_empty() {
                return Err(trf("The source folder is empty or unavailable; deleting {n} objects was prevented", &[("n", &extra.len().to_string())]));
            }
            result.deleted = extra.len();
            if !extra.is_empty() {
                self.delete_keys(bucket, extra).await?;
            }
        }
        Ok(result)
    }

    /// Downloads new and changed objects; with `mirror`, deletes local files the bucket no longer has.
    pub async fn sync_down(&self, bucket: &str, prefix: &str, dir: &Path, mirror: bool, progress: &Progress) -> Res<SyncResult> {
        let local = local_files(dir)?;
        let (remote, truncated) = self.list_all(bucket, prefix, usize::MAX).await?;
        let mut result = SyncResult::default();
        let mut wanted = HashSet::new();
        for entry in remote.iter().filter(|e| !e.key.ends_with('/')) {
            progress.check_public()?;
            // A key that cannot be a local path ("..", empty parts) is skipped, not fatal,
            // and paths are compared in the normalized form the download uses.
            let Ok(path) = crate::s3::download_target(dir, prefix, &entry.key) else { result.failed += 1; continue };
            let relative = path.strip_prefix(dir).map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
            wanted.insert(relative.clone());
            let unchanged = local.get(&relative).is_some_and(|(_, size, modified)| *size as i64 == entry.size && *modified >= entry.modified);
            if unchanged {
                result.skipped += 1;
                continue;
            }
            match self.download_sized(bucket, &entry.key, entry.size.max(0) as u64, &path, progress).await {
                Ok(()) => {
                    // A synced copy carries the object's date, which the next sync compares with.
                    if let Ok(file) = std::fs::File::options().write(true).open(&path) {
                        let _ = file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(entry.modified.max(0) as u64));
                    }
                    result.transferred += 1
                }
                Err(e) if e == crate::s3::CANCELLED => return Err(e),
                Err(_) => result.failed += 1,
            }
        }
        if mirror && !truncated {
            let extra: Vec<&PathBuf> = local.iter().filter(|(rel, _)| !wanted.contains(*rel)).map(|(_, (path, _, _))| path).collect();
            if wanted.is_empty() && !extra.is_empty() {
                return Err(tr("This location has no objects; mirror mode stopped because it would empty the local folder"));
            }
            for path in extra {
                if std::fs::remove_file(path).is_ok() {
                    result.deleted += 1;
                }
            }
        }
        Ok(result)
    }
}

impl Progress {
    pub fn check_public(&self) -> Res<()> {
        if self.cancel.load(std::sync::atomic::Ordering::Relaxed) { Err(crate::s3::CANCELLED.to_string()) } else { Ok(()) }
    }
}

fn glib_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}


/// Files below a folder by relative path ("a/b.txt"), with size and modification time.
fn local_files(dir: &Path) -> Res<HashMap<String, (PathBuf, u64, i64)>> {
    let mut files = HashMap::new();
    if !dir.is_dir() {
        return Err(trf("The folder {path} is not available", &[("path", &dir.display().to_string())]));
    }
    for (path, key, size) in crate::s3::collect_uploads("", &[dir.to_path_buf()])? {
        // collect_uploads puts the folder name first; the sync compares paths inside it.
        let relative = key.split_once('/').map(|(_, rest)| rest.to_string()).unwrap_or(key);
        let modified = std::fs::metadata(&path).ok().and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0);
        files.insert(relative, (path, size, modified));
    }
    Ok(files)
}
