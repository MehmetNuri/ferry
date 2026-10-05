pub mod access;
pub mod connection;
pub mod pinned;
pub mod tools;
pub mod vault;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use aws_sdk_s3::Client;
use aws_sdk_s3::config::{Region, RequestChecksumCalculation, ResponseChecksumValidation};
use aws_sdk_s3::error::{DisplayErrorContext, ProvideErrorMetadata, SdkError};
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{
    BucketLocationConstraint, CompletedMultipartUpload, CompletedPart, CreateBucketConfiguration, Delete,
    MetadataDirective, ObjectIdentifier, ServerSideEncryption, StorageClass,
};
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

use crate::i18n::{tr, trf, trn};
use crate::profile::Profile;

pub type Res<T> = Result<T, String>;

const MULTIPART_THRESHOLD: u64 = 16 * 1024 * 1024;
const PART_SIZE: u64 = 8 * 1024 * 1024;
const MAX_PARTS: u64 = 10_000;
pub const SCAN_LIMIT: usize = 200_000;

pub fn describe<E>(error: SdkError<E>) -> String
where
    E: ProvideErrorMetadata + std::error::Error + 'static,
{
    // Some providers (Supabase) reply 200 with an unparsable body for missing features.
    if let Some(raw) = error.raw_response()
        && raw.status().is_success()
    {
        return tr("This provider does not support this feature");
    }
    if let SdkError::ServiceError(service) = &error {
        let status = service.raw().status().as_u16();
        if status == 501 || service.err().code() == Some("NotImplemented") {
            return tr("This provider does not support this feature");
        }
        if let Some(code) = service.err().code()
            && let Some(hint) = explain(code)
        {
            return format!("{hint} ({code})");
        }
        return match (service.err().code(), service.err().message()) {
            (Some(code), Some(message)) => format!("{code}: {message}"),
            (Some(code), None) => format!("{code} (HTTP {status})"),
            _ => format!("HTTP {status}"),
        };
    }
    let detail = format!("{}", DisplayErrorContext(&error));
    match &error {
        SdkError::DispatchFailure(_) => format!(
            "{} ({detail})",
            tr("The service could not be reached; check the network connection and the endpoint address")
        ),
        SdkError::TimeoutError(_) => format!("{} ({detail})", tr("The service did not answer in time")),
        _ => detail,
    }
}

fn explain(code: &str) -> Option<String> {
    Some(match code {
        "AccessDenied" | "Forbidden" => tr("Access denied: these credentials may not do this here"),
        "InvalidAccessKeyId" => tr("The service does not know this access key; check the connection settings"),
        "SignatureDoesNotMatch" => tr("The secret key does not match the access key; check the connection settings"),
        "RequestTimeTooSkewed" => {
            tr("The computer’s clock is too far off; turn on automatic date and time in Settings")
        }
        "NoSuchBucket" => tr("The bucket does not exist, or it is in another region"),
        "NoSuchKey" => tr("The object no longer exists"),
        "EntityTooLarge" => tr("The file is larger than the service allows"),
        "SlowDown" | "TooManyRequests" => tr("The service asks to slow down; it is tried again shortly"),
        "ExpiredToken" | "TokenRefreshRequired" => tr("The session token has expired; enter new credentials"),
        "BucketAlreadyExists" => tr("This bucket name is already taken; bucket names are shared by everyone"),
        "BucketAlreadyOwnedByYou" => tr("You already have a bucket with this name"),
        "BucketNotEmpty" => tr("The bucket is not empty; delete its objects first"),
        "InvalidBucketName" => tr("The bucket name is not valid: use 3–63 lowercase letters, digits, dots and hyphens"),
        "QuotaExceeded" | "StorageQuotaExceeded" => tr("The storage quota of the account is used up"),
        _ => return None,
    })
}

#[allow(dead_code)]
pub fn error_code<E: ProvideErrorMetadata>(error: &SdkError<E>) -> String {
    match error {
        SdkError::ServiceError(service) => service.err().code().unwrap_or_default().to_string(),
        _ => String::new(),
    }
}

fn encode_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for byte in key.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    pub key: String,
    pub name: String,
    pub size: i64,
    pub modified: i64,
    pub etag: String,
    pub storage_class: String,
    pub is_folder: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Listing {
    pub items: Vec<Entry>,
    pub next_token: String,
}

#[derive(Clone, Debug, Default)]
pub struct BucketEntry {
    pub name: String,
    pub pinned: bool,
}

#[derive(Clone, Debug, Default)]
pub struct BucketList {
    pub buckets: Vec<BucketEntry>,
    pub warning: String,
}

#[derive(Clone, Debug, Default)]
pub struct ObjectInfo {
    pub key: String,
    pub size: i64,
    pub content_type: String,
    pub etag: String,
    pub modified: i64,
    pub storage_class: String,
    pub cache_control: String,
    pub content_disposition: String,
    pub content_encoding: String,
    pub version_id: String,
    pub encryption: String,
    pub metadata: Vec<(String, String)>,
}

#[derive(Clone, Default)]
pub struct Progress {
    pub done: Arc<AtomicU64>,
    pub cancel: Arc<AtomicBool>,
}

impl Progress {
    pub(crate) fn add(&self, bytes: u64) {
        self.done.fetch_add(bytes, Ordering::Relaxed);
    }
    pub(crate) fn check(&self) -> Res<()> {
        if self.cancel.load(Ordering::Relaxed) { Err(CANCELLED.to_string()) } else { Ok(()) }
    }
    pub(crate) async fn advance(&self, bytes: u64) -> Res<()> {
        throttle(bytes).await;
        self.check()?;
        self.add(bytes);
        Ok(())
    }
}

pub static BANDWIDTH: AtomicU64 = AtomicU64::new(0);
static BUCKET: std::sync::Mutex<Option<(f64, std::time::Instant)>> = std::sync::Mutex::new(None);

pub async fn throttle(bytes: u64) {
    let rate = BANDWIDTH.load(Ordering::Relaxed) as f64;
    if rate <= 0.0 || bytes == 0 {
        return;
    }
    let wait = {
        let mut bucket = BUCKET.lock().unwrap();
        let now = std::time::Instant::now();
        let (tokens, last) = bucket.unwrap_or((rate, now));
        let tokens = (tokens + now.duration_since(last).as_secs_f64() * rate).min(rate) - bytes as f64;
        *bucket = Some((tokens, now));
        if tokens < 0.0 { -tokens / rate } else { 0.0 }
    };
    if wait > 0.0 {
        tokio::time::sleep(Duration::from_secs_f64(wait.min(120.0))).await;
    }
}

pub const CANCELLED: &str = "cancelled";

#[derive(Clone)]
pub struct S3 {
    pub client: Client,
    pub profile: Profile,
    raw: bool,
    pub remote: Option<Arc<crate::remote::Remote>>,
}

pub fn public_url(profile: &Profile, bucket: &str, key: &str) -> String {
    let encoded: String = key
        .split('/')
        .map(|part| {
            part.bytes()
                .map(|b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
                    _ => format!("%{b:02X}"),
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/");
    let endpoint = endpoint_of(profile);
    // Supabase serves public objects from the storage API, not the S3 endpoint.
    if profile.provider == "supabase"
        && let Some(base) = endpoint.trim_end_matches('/').strip_suffix("/storage/v1/s3")
    {
        return format!("{base}/storage/v1/object/public/{bucket}/{encoded}");
    }
    if endpoint.is_empty() {
        let region = if profile.region.trim().is_empty() { "us-east-1" } else { profile.region.trim() };
        return format!("https://{bucket}.s3.{region}.amazonaws.com/{encoded}");
    }
    let base = endpoint.trim_end_matches('/');
    if profile.path_style {
        format!("{base}/{bucket}/{encoded}")
    } else {
        match base.split_once("://") {
            Some((scheme, host)) => format!("{scheme}://{bucket}.{host}/{encoded}"),
            None => format!("{base}/{bucket}/{encoded}"),
        }
    }
}

static CLIENTS: std::sync::Mutex<Option<std::collections::HashMap<String, S3>>> = std::sync::Mutex::new(None);

pub fn forget_client(profile_id: &str) {
    if let Some(map) = CLIENTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_mut() {
        map.remove(profile_id);
    }
    connection::forget_session(profile_id);
    vault::lock_all(profile_id);
}

pub async fn client_for(profile_id: &str) -> Res<S3> {
    if let Some(client) = CLIENTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .and_then(|m| m.get(profile_id).cloned())
    {
        return Ok(client);
    }
    let stored = crate::profile::load()
        .into_iter()
        .find(|p| p.id == profile_id)
        .ok_or_else(|| tr("The connection of this transfer was deleted"))?;
    let client = S3::connect(crate::profile::with_secrets(stored).await?).await.map_err(|e| {
        if e == connection::MFA_REQUIRED {
            tr("The MFA session of this connection ended. Connect again to enter a new code.")
        } else {
            e
        }
    })?;
    CLIENTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_or_insert_with(Default::default)
        .insert(profile_id.to_string(), client.clone());
    Ok(client)
}

pub fn endpoint_of(profile: &Profile) -> String {
    let mut endpoint = profile.endpoint.trim().trim_end_matches('/').to_string();
    if endpoint.is_empty() && profile.provider == "supabase" && !profile.project_ref.trim().is_empty() {
        endpoint = format!("https://{}.storage.supabase.co/storage/v1/s3", profile.project_ref.trim());
    }
    // Path-style endpoints with a path (Supabase /storage/v1/s3) need a trailing slash.
    let has_path = endpoint.split_once("://").map(|(_, rest)| rest.contains('/')).unwrap_or(false);
    if has_path {
        endpoint.push('/');
    }
    endpoint
}

impl S3 {
    fn vault_for(&self, bucket: &str, key: &str) -> Option<(Arc<vault::Unlocked>, String)> {
        if self.raw { None } else { vault::find(&self.profile.id, bucket, key) }
    }

    pub fn raw(&self) -> S3 {
        S3 { raw: true, ..self.clone() }
    }

    fn plain_only(&self, bucket: &str, key: &str) -> Res<()> {
        if self.vault_for(bucket, key).is_some() { Err(vault::unsupported()) } else { Ok(()) }
    }

    pub async fn put_bytes(&self, bucket: &str, key: &str, data: Vec<u8>) -> Res<()> {
        if let Some((v, rel)) = self.vault_for(bucket, key) {
            return v.put_bytes(&self.raw(), &rel, data, true).await;
        }
        self.put_bytes_plain(bucket, key, data).await
    }

    pub(super) async fn put_bytes_plain(&self, bucket: &str, key: &str, data: Vec<u8>) -> Res<()> {
        if let Some(r) = &self.remote {
            return r.write(bucket, key, data).await;
        }
        self.client.put_object().bucket(bucket).key(key).body(ByteStream::from(data)).send().await.map_err(describe)?;
        Ok(())
    }

    pub async fn rename_folder(&self, bucket: &str, from: &str, to: &str) -> Res<()> {
        if let (Some((v, rel_from)), Some((w, rel_to))) = (self.vault_for(bucket, from), self.vault_for(bucket, to))
            && Arc::ptr_eq(&v, &w)
        {
            return v.rename(&self.raw(), &rel_from, &rel_to).await;
        }
        if let Some(r) = &self.remote {
            return r.rename(bucket, from, to).await;
        }
        let (items, _) = self.list_all(bucket, from, usize::MAX).await?;
        for item in items {
            let target = format!("{to}{}", item.key.strip_prefix(from).unwrap_or(&item.key));
            self.rename(bucket, &item.key, &target).await?;
        }
        Ok(())
    }

    pub fn is_s3(&self) -> bool {
        self.remote.is_none()
    }

    fn s3_only(&self) -> Res<()> {
        if self.remote.is_some() { Err(tr("File servers do not offer this")) } else { Ok(()) }
    }

    pub async fn connect(profile: Profile) -> Res<S3> {
        if crate::remote::is_remote(&profile.provider) {
            let remote = crate::remote::Remote::connect(&profile).await?;
            let config = aws_sdk_s3::Config::builder()
                .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
                .region(Region::new("us-east-1"))
                .build();
            return Ok(S3 { client: Client::from_conf(config), profile, raw: false, remote: Some(Arc::new(remote)) });
        }
        let region =
            if profile.region.trim().is_empty() { "us-east-1".to_string() } else { profile.region.trim().to_string() };
        let endpoint = endpoint_of(&profile);
        if endpoint.is_empty() && profile.provider != "aws" {
            return Err(tr("An endpoint is required for this provider"));
        }
        let http = connection::http_client(&profile, &endpoint)?;
        let (shared, provider) = connection::credentials(&profile, &region, http.clone()).await?;
        let mut builder = aws_sdk_s3::config::Builder::from(&shared).http_client(http);
        if let Some(provider) = provider {
            builder = builder.credentials_provider(provider);
        }
        if profile.accelerate && profile.provider == "aws" {
            builder = builder.accelerate(true);
        }
        builder = builder
            .region(Region::new(region))
            .force_path_style(profile.path_style)
            // Many S3-compatible services reject the newer trailing checksums.
            .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
            .response_checksum_validation(ResponseChecksumValidation::WhenRequired);
        if !endpoint.is_empty() {
            builder = builder.endpoint_url(endpoint);
        }
        let s3 = S3 { client: Client::from_conf(builder.build()), profile, raw: false, remote: None };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let sweeper = s3.clone();
            runtime.spawn(async move { sweeper.sweep_resumes().await });
        }
        Ok(s3)
    }

    pub async fn test(profile: Profile) -> Res<String> {
        let s3 = S3::connect(profile).await?;
        if let Some(remote) = &s3.remote {
            return remote.test(&s3.profile).await;
        }
        let started = std::time::Instant::now();
        let list = s3.list_buckets().await?;
        let latency = trf("{ms} ms", &[("ms", &started.elapsed().as_millis().to_string())]);
        if list.warning.is_empty() {
            return Ok(format!(
                "{} · {latency}",
                trn(
                    "Connection successful: {n} bucket",
                    "Connection successful: {n} buckets",
                    &[("n", &list.buckets.len().to_string())]
                )
            ));
        }
        let Some(first) = s3.profile.buckets.first() else { return Err(list.warning) };
        s3.client.head_bucket().bucket(first).send().await.map_err(describe)?;
        Ok(format!("{} · {latency}", trf("Connection successful: bucket “{name}” is reachable", &[("name", first)])))
    }

    pub async fn list_buckets(&self) -> Res<BucketList> {
        if let Some(r) = &self.remote {
            let buckets =
                r.buckets(&self.profile).await?.into_iter().map(|name| BucketEntry { name, pinned: false }).collect();
            return Ok(BucketList { buckets, warning: String::new() });
        }
        let mut list = BucketList::default();
        match self.client.list_buckets().send().await {
            Ok(out) => {
                for bucket in out.buckets() {
                    if let Some(name) = bucket.name() {
                        list.buckets.push(BucketEntry { name: name.to_string(), pinned: false });
                    }
                }
            }
            Err(error) => list.warning = describe(error),
        }
        for name in &self.profile.buckets {
            if !list.buckets.iter().any(|b| &b.name == name) {
                list.buckets.push(BucketEntry { name: name.clone(), pinned: true });
            }
        }
        list.buckets.sort_by_key(|b| b.name.to_lowercase());
        Ok(list)
    }

    pub async fn list_objects(&self, bucket: &str, prefix: &str, token: &str) -> Res<Listing> {
        if let Some((v, rel)) = self.vault_for(bucket, prefix) {
            return v.list_objects(&self.raw(), &rel, token).await;
        }
        self.list_objects_plain(bucket, prefix, token).await
    }

    pub(super) async fn list_objects_plain(&self, bucket: &str, prefix: &str, token: &str) -> Res<Listing> {
        if let Some(r) = &self.remote {
            return Ok(Listing { items: r.list(bucket, prefix).await?, next_token: String::new() });
        }
        let mut request = self.client.list_objects_v2().bucket(bucket).prefix(prefix).delimiter("/").max_keys(1000);
        if !token.is_empty() {
            request = request.continuation_token(token);
        }
        let out = request.send().await.map_err(describe)?;
        let mut listing = Listing::default();
        for common in out.common_prefixes() {
            let Some(key) = common.prefix() else { continue };
            let name = key.strip_prefix(prefix).unwrap_or(key).trim_end_matches('/');
            if name.is_empty() {
                continue;
            }
            listing.items.push(Entry {
                key: key.to_string(),
                name: name.to_string(),
                is_folder: true,
                ..Default::default()
            });
        }
        for object in out.contents() {
            let entry = entry_of(object, prefix);
            if entry.key == prefix {
                continue;
            }
            listing.items.push(entry);
        }
        listing.next_token = out.next_continuation_token().unwrap_or_default().to_string();
        Ok(listing)
    }

    pub async fn list_all(&self, bucket: &str, prefix: &str, limit: usize) -> Res<(Vec<Entry>, bool)> {
        if let Some((v, rel)) = self.vault_for(bucket, prefix) {
            return v.list_all(&self.raw(), &rel, limit).await;
        }
        if let Some(r) = &self.remote {
            return r.list_all(bucket, prefix, limit).await;
        }
        let mut items = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let out = self
                .client
                .list_objects_v2()
                .bucket(bucket)
                .prefix(prefix)
                .max_keys(1000)
                .set_continuation_token(token.clone())
                .send()
                .await
                .map_err(describe)?;
            items.extend(out.contents().iter().map(|object| entry_of(object, prefix)));
            token = out.next_continuation_token().map(str::to_string);
            if items.len() >= limit {
                return Ok((items, token.is_some()));
            }
            if token.is_none() {
                return Ok((items, false));
            }
        }
    }

    pub async fn head_object(&self, bucket: &str, key: &str) -> Res<ObjectInfo> {
        if let Some((v, rel)) = self.vault_for(bucket, key) {
            return v.head_object(&self.raw(), &rel).await;
        }
        self.head_object_plain(bucket, key).await
    }

    pub(super) async fn head_object_plain(&self, bucket: &str, key: &str) -> Res<ObjectInfo> {
        if let Some(r) = &self.remote {
            return r.head(bucket, key).await;
        }
        let out = self.client.head_object().bucket(bucket).key(key).send().await.map_err(describe)?;
        let mut metadata: Vec<(String, String)> =
            out.metadata().map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default();
        metadata.sort();
        Ok(ObjectInfo {
            key: key.to_string(),
            size: out.content_length().unwrap_or(0),
            content_type: out.content_type().unwrap_or_default().to_string(),
            etag: out.e_tag().unwrap_or_default().trim_matches('"').to_string(),
            modified: out.last_modified().map(|d| d.secs()).unwrap_or(0),
            storage_class: out.storage_class().map(|c| c.as_str().to_string()).unwrap_or_default(),
            cache_control: out.cache_control().unwrap_or_default().to_string(),
            content_disposition: out.content_disposition().unwrap_or_default().to_string(),
            content_encoding: out.content_encoding().unwrap_or_default().to_string(),
            version_id: out.version_id().unwrap_or_default().to_string(),
            encryption: out.server_side_encryption().map(|e| e.as_str().to_string()).unwrap_or_default(),
            metadata,
        })
    }

    pub async fn read_bytes(&self, bucket: &str, key: &str, limit: u64) -> Res<Vec<u8>> {
        if let Some((v, rel)) = self.vault_for(bucket, key) {
            return v.read_bytes(&self.raw(), &rel, limit).await;
        }
        self.read_bytes_plain(bucket, key, limit).await
    }

    pub(super) async fn read_bytes_plain(&self, bucket: &str, key: &str, limit: u64) -> Res<Vec<u8>> {
        if let Some(r) = &self.remote {
            return r.read(bucket, key, limit).await;
        }
        let out = self
            .client
            .get_object()
            .bucket(bucket)
            .key(key)
            .range(format!("bytes=0-{}", limit.saturating_sub(1)))
            .send()
            .await
            .map_err(describe)?;
        let mut body = out.body;
        let mut data = Vec::new();
        while let Some(chunk) = body.try_next().await.map_err(|e| e.to_string())? {
            let room = (limit as usize).saturating_sub(data.len());
            data.extend_from_slice(&chunk[..chunk.len().min(room)]);
            if data.len() >= limit as usize {
                break;
            }
        }
        Ok(data)
    }

    pub async fn save_text(&self, bucket: &str, key: &str, text: String, etag: &str) -> Res<()> {
        if let Some((v, rel)) = self.vault_for(bucket, key) {
            return v.put_bytes(&self.raw(), &rel, text.into_bytes(), true).await;
        }
        if let Some(r) = &self.remote {
            return r.write(bucket, key, text.into_bytes()).await;
        }
        let current = self.head_object(bucket, key).await?;
        if current.etag != etag {
            return Err(tr("The object changed after it was opened. Open it again and retry."));
        }
        let mut request = self.client.put_object().bucket(bucket).key(key).body(ByteStream::from(text.into_bytes()));
        if !current.content_type.is_empty() {
            request = request.content_type(current.content_type);
        }
        if !current.cache_control.is_empty() {
            request = request.cache_control(current.cache_control);
        }
        for (name, value) in current.metadata {
            request = request.metadata(name, value);
        }
        request.send().await.map_err(describe)?;
        Ok(())
    }

    pub async fn create_object(&self, bucket: &str, key: &str, data: Vec<u8>, content_type: &str) -> Res<()> {
        if let Some((v, rel)) = self.vault_for(bucket, key) {
            return v.put_bytes(&self.raw(), &rel, data, false).await;
        }
        self.create_object_plain(bucket, key, data, content_type).await
    }

    pub(super) async fn create_object_plain(
        &self,
        bucket: &str,
        key: &str,
        data: Vec<u8>,
        content_type: &str,
    ) -> Res<()> {
        if let Some(r) = &self.remote {
            if r.stat(bucket, key).await.is_ok() {
                return Err(trf("“{name}” already exists", &[("name", key.rsplit('/').next().unwrap_or(key))]));
            }
            return r.write(bucket, key, data).await;
        }
        if self.head_object(bucket, key).await.is_ok() {
            return Err(trf("“{name}” already exists", &[("name", key.rsplit('/').next().unwrap_or(key))]));
        }
        self.client
            .put_object()
            .bucket(bucket)
            .key(key)
            .content_type(content_type)
            .body(ByteStream::from(data))
            .send()
            .await
            .map_err(describe)?;
        Ok(())
    }

    pub async fn create_folder(&self, bucket: &str, key: &str) -> Res<()> {
        if let Some((v, rel)) = self.vault_for(bucket, key) {
            return v.create_folder(&self.raw(), &rel).await;
        }
        if let Some(r) = &self.remote {
            return r.mkdir_p(bucket, key).await;
        }
        self.client
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from_static(b""))
            .send()
            .await
            .map_err(describe)?;
        Ok(())
    }

    pub async fn create_bucket(&self, name: &str) -> Res<()> {
        if let Some(r) = &self.remote {
            return r.create_bucket(name).await;
        }
        let mut request = self.client.create_bucket().bucket(name);
        let region = self.profile.region.trim();
        if self.profile.provider == "aws" && !region.is_empty() && region != "us-east-1" {
            let config = CreateBucketConfiguration::builder()
                .location_constraint(BucketLocationConstraint::from(region))
                .build();
            request = request.create_bucket_configuration(config);
        }
        request.send().await.map_err(describe)?;
        Ok(())
    }

    pub async fn empty_bucket(&self, name: &str, progress: &Progress) -> Res<usize> {
        if let Some(r) = &self.remote {
            let (items, _) = r.list(name, "").await.map(|i| (i, ()))?;
            let keys: Vec<String> = items.into_iter().map(|e| e.key).collect();
            return r.delete(name, keys).await;
        }
        let mut count = 0;
        loop {
            progress.check()?;
            let (items, _) = self.list_all(name, "", 10_000).await?;
            if items.is_empty() {
                break;
            }
            let batch = items.len();
            self.delete_exact(name, items.into_iter().map(|e| (e.key, None)).collect()).await?;
            count += batch;
            progress.add(batch as u64);
        }
        Ok(count)
    }

    pub async fn delete_bucket(&self, name: &str) -> Res<()> {
        if let Some(r) = &self.remote {
            return r.delete_bucket(name).await;
        }
        loop {
            let (items, _) = self.list_all(name, "", 10_000).await?;
            if items.is_empty() {
                break;
            }
            self.delete_exact(name, items.into_iter().map(|e| (e.key, None)).collect()).await?;
        }
        loop {
            let Ok(out) = self.client.list_object_versions().bucket(name).max_keys(1000).send().await else { break };
            let mut targets: Vec<(String, Option<String>)> = Vec::new();
            for version in out.versions() {
                targets.push((version.key().unwrap_or_default().to_string(), version.version_id().map(str::to_string)));
            }
            for marker in out.delete_markers() {
                targets.push((marker.key().unwrap_or_default().to_string(), marker.version_id().map(str::to_string)));
            }
            if targets.is_empty() {
                break;
            }
            self.delete_exact(name, targets).await?;
        }
        self.client.delete_bucket().bucket(name).send().await.map_err(describe)?;
        Ok(())
    }

    pub async fn delete_keys(&self, bucket: &str, keys: Vec<String>) -> Res<usize> {
        if let Some(first) = keys.first()
            && let Some((v, _)) = self.vault_for(bucket, first)
        {
            let root = self
                .vault_for(bucket, first)
                .map(|(_, rel)| first[..first.len() - rel.len()].to_string())
                .unwrap_or_default();
            let rels = keys
                .iter()
                .map(|k| k.strip_prefix(&root).map(str::to_string).ok_or_else(vault::unsupported))
                .collect::<Res<Vec<_>>>()?;
            return v.delete(&self.raw(), rels).await;
        }
        self.delete_keys_plain(bucket, keys).await
    }

    pub(super) async fn delete_keys_plain(&self, bucket: &str, keys: Vec<String>) -> Res<usize> {
        if let Some(r) = &self.remote {
            return r.delete(bucket, keys).await;
        }
        let mut targets = Vec::new();
        for key in keys {
            if key.ends_with('/') {
                let (items, _) = self.list_all(bucket, &key, usize::MAX).await?;
                targets.extend(items.into_iter().map(|e| (e.key, None)));
                if !targets.iter().any(|(k, _)| k == &key) {
                    targets.push((key, None));
                }
            } else {
                targets.push((key, None));
            }
        }
        let count = targets.len();
        self.delete_exact(bucket, targets).await?;
        Ok(count)
    }

    async fn delete_exact(&self, bucket: &str, targets: Vec<(String, Option<String>)>) -> Res<()> {
        for chunk in targets.chunks(1000) {
            let mut objects = Vec::with_capacity(chunk.len());
            for (key, version) in chunk {
                objects.push(
                    ObjectIdentifier::builder()
                        .key(key)
                        .set_version_id(version.clone())
                        .build()
                        .map_err(|e| e.to_string())?,
                );
            }
            let delete = Delete::builder().set_objects(Some(objects)).quiet(true).build().map_err(|e| e.to_string())?;
            match self.client.delete_objects().bucket(bucket).delete(delete).send().await {
                Ok(out) => {
                    if let Some(failed) = out.errors().first() {
                        return Err(format!(
                            "{}: {}",
                            failed.key().unwrap_or_default(),
                            failed.message().unwrap_or_default()
                        ));
                    }
                }
                // Some services lack batch delete, fall back to one request per object.
                Err(_) => {
                    for (key, version) in chunk {
                        self.client
                            .delete_object()
                            .bucket(bucket)
                            .key(key)
                            .set_version_id(version.clone())
                            .send()
                            .await
                            .map_err(describe)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn storage_class(&self) -> Option<StorageClass> {
        if self.profile.storage_class.is_empty() {
            None
        } else {
            Some(StorageClass::from(self.profile.storage_class.as_str()))
        }
    }

    fn encryption(&self) -> (Option<ServerSideEncryption>, Option<String>) {
        if self.profile.encryption.is_empty() {
            return (None, None);
        }
        let kms = if self.profile.encryption == "aws:kms" && !self.profile.kms_key.is_empty() {
            Some(self.profile.kms_key.clone())
        } else {
            None
        };
        (Some(ServerSideEncryption::from(self.profile.encryption.as_str())), kms)
    }

    // Copy source is URL-encoded per spec; Supabase wants it raw, so retry unencoded.
    pub async fn send_copy(
        &self,
        request: aws_sdk_s3::operation::copy_object::builders::CopyObjectFluentBuilder,
        bucket: &str,
        key: &str,
        version: Option<&str>,
    ) -> Res<()> {
        let encoded = encode_key(key);
        let mut sources = vec![encoded.clone()];
        if encoded != key {
            sources.push(key.to_string());
        }
        let mut last = String::new();
        for source in sources {
            let mut copy_source = format!("{bucket}/{source}");
            if let Some(version) = version {
                copy_source.push_str(&format!("?versionId={version}"));
            }
            match request.clone().copy_source(copy_source).send().await {
                Ok(_) => return Ok(()),
                Err(error) => {
                    let missing = error_code(&error) == "NoSuchKey";
                    last = describe(error);
                    if !missing {
                        return Err(last);
                    }
                }
            }
        }
        Err(last)
    }

    pub async fn copy_object(&self, src_bucket: &str, src_key: &str, dst_bucket: &str, dst_key: &str) -> Res<()> {
        match (self.vault_for(src_bucket, src_key), self.vault_for(dst_bucket, dst_key)) {
            (Some((v, from)), Some((w, to))) if Arc::ptr_eq(&v, &w) => {
                return v.copy_file(&self.raw(), &from, &to).await;
            }
            (None, None) => {}
            _ => return self.copy_through(src_bucket, src_key, self, dst_bucket, dst_key, &Progress::default()).await,
        }
        self.copy_object_plain(src_bucket, src_key, dst_bucket, dst_key).await
    }

    pub(super) async fn copy_object_plain(
        &self,
        src_bucket: &str,
        src_key: &str,
        dst_bucket: &str,
        dst_key: &str,
    ) -> Res<()> {
        if let Some(r) = &self.remote {
            if src_bucket == dst_bucket
                && let Some(done) = r.copy(src_bucket, src_key, dst_key).await
            {
                return done;
            }
            let dir = gtk::glib::user_cache_dir().join("ferry").join("copy");
            tokio::fs::create_dir_all(&dir).await.map_err(|e| e.to_string())?;
            let temp = dir.join(gtk::glib::uuid_string_random().as_str());
            let result = async {
                r.download(src_bucket, src_key, &temp, &Progress::default()).await?;
                r.upload(dst_bucket, &temp, dst_key, &Progress::default()).await
            }
            .await;
            let _ = tokio::fs::remove_file(&temp).await;
            return result;
        }
        let (sse, kms) = self.encryption();
        let request = self
            .client
            .copy_object()
            .bucket(dst_bucket)
            .key(dst_key)
            .set_storage_class(self.storage_class())
            .set_server_side_encryption(sse)
            .set_ssekms_key_id(kms);
        self.send_copy(request, src_bucket, src_key, None).await
    }

    pub async fn rename(&self, bucket: &str, from: &str, to: &str) -> Res<()> {
        if let (Some((v, rel_from)), Some((w, rel_to))) = (self.vault_for(bucket, from), self.vault_for(bucket, to)) {
            if !Arc::ptr_eq(&v, &w) {
                return Err(vault::unsupported());
            }
            return v.rename(&self.raw(), &rel_from, &rel_to).await;
        }
        if let Some(r) = &self.remote {
            return r.rename(bucket, from, to).await;
        }
        self.copy_object(bucket, from, bucket, to).await?;
        self.client.delete_object().bucket(bucket).key(from).send().await.map_err(describe)?;
        Ok(())
    }

    pub async fn presign_put(&self, bucket: &str, key: &str, seconds: u64) -> Res<(String, String)> {
        self.plain_only(bucket, key)?;
        self.s3_only()?;
        let config = PresigningConfig::expires_in(Duration::from_secs(seconds)).map_err(|e| e.to_string())?;
        let content_type = content_type_of(Path::new(key));
        let request = self
            .client
            .put_object()
            .bucket(bucket)
            .key(key)
            .content_type(&content_type)
            .presigned(config)
            .await
            .map_err(describe)?;
        Ok((request.uri().to_string(), content_type))
    }

    pub fn insecure_endpoint(endpoint: &str) -> bool {
        let Some(rest) = endpoint.trim().strip_prefix("http://") else { return false };
        let host = rest.split(['/', ':']).next().unwrap_or("").to_lowercase();
        let local = host == "localhost"
            || host.ends_with(".local")
            || host.starts_with("127.")
            || host.starts_with("10.")
            || host.starts_with("192.168.")
            || host == "[::1]"
            || host
                .strip_prefix("172.")
                .and_then(|r| r.split('.').next()?.parse::<u8>().ok())
                .is_some_and(|n| (16..=31).contains(&n));
        !local
    }

    pub async fn sha256(&self, bucket: &str, key: &str, progress: &Progress) -> Res<String> {
        self.plain_only(bucket, key)?;
        self.s3_only()?;
        use sha2::Digest;
        let out = self.client.get_object().bucket(bucket).key(key).send().await.map_err(describe)?;
        let mut hasher = sha2::Sha256::new();
        let mut body = out.body;
        while let Some(chunk) = body.try_next().await.map_err(|e| e.to_string())? {
            hasher.update(&chunk);
            progress.advance(chunk.len() as u64).await?;
        }
        Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
    }

    pub async fn presign(&self, bucket: &str, key: &str, seconds: u64) -> Res<String> {
        self.plain_only(bucket, key)?;
        self.s3_only()?;
        let config = PresigningConfig::expires_in(Duration::from_secs(seconds)).map_err(|e| e.to_string())?;
        let request = self.client.get_object().bucket(bucket).key(key).presigned(config).await.map_err(describe)?;
        Ok(request.uri().to_string())
    }

    pub async fn rewrite(&self, bucket: &str, info: &ObjectInfo, storage_class: Option<&str>) -> Res<()> {
        self.plain_only(bucket, &info.key)?;
        self.s3_only()?;
        let mut request =
            self.client.copy_object().bucket(bucket).key(&info.key).metadata_directive(MetadataDirective::Replace);
        if !info.content_type.is_empty() {
            request = request.content_type(&info.content_type);
        }
        if !info.cache_control.is_empty() {
            request = request.cache_control(&info.cache_control);
        }
        if !info.content_disposition.is_empty() {
            request = request.content_disposition(&info.content_disposition);
        }
        if !info.content_encoding.is_empty() {
            request = request.content_encoding(&info.content_encoding);
        }
        for (name, value) in &info.metadata {
            request = request.metadata(name, value);
        }
        let class = storage_class.unwrap_or(&info.storage_class);
        if !class.is_empty() {
            request = request.storage_class(StorageClass::from(class));
        }
        self.send_copy(request, bucket, &info.key, None).await
    }

    pub async fn set_headers(
        &self,
        bucket: &str,
        keys: Vec<String>,
        cache_control: Option<String>,
        content_type: Option<String>,
        progress: &Progress,
    ) -> Res<(usize, usize, String)> {
        if let Some(first) = keys.first() {
            self.plain_only(bucket, first)?;
        }
        self.s3_only()?;
        let (mut changed, mut failed, mut last_error) = (0, 0, String::new());
        for key in keys {
            progress.check()?;
            let result = async {
                let mut info = self.head_object(bucket, &key).await?;
                if let Some(value) = &cache_control {
                    info.cache_control = value.clone();
                }
                if let Some(value) = &content_type {
                    info.content_type = value.clone();
                }
                self.rewrite(bucket, &info, None).await
            }
            .await;
            match result {
                Ok(()) => changed += 1,
                Err(error) => {
                    failed += 1;
                    last_error = error;
                }
            }
            progress.add(1);
        }
        Ok((changed, failed, last_error))
    }

    pub async fn set_storage_class(&self, bucket: &str, keys: Vec<String>, class: &str) -> Res<usize> {
        if let Some(first) = keys.first() {
            self.plain_only(bucket, first)?;
        }
        self.s3_only()?;
        let mut changed = 0;
        let mut failed = 0;
        let mut last_error = String::new();
        for key in keys {
            let result = async {
                let info = self.head_object(bucket, &key).await?;
                self.rewrite(bucket, &info, Some(class)).await
            }
            .await;
            match result {
                Ok(()) => changed += 1,
                Err(error) => {
                    failed += 1;
                    last_error = error;
                }
            }
        }
        if failed > 0 {
            return Err(format!(
                "{} ({last_error})",
                trn(
                    "The storage class of {n} object could not be changed",
                    "The storage class of {n} objects could not be changed",
                    &[("n", &failed.to_string())]
                )
            ));
        }
        Ok(changed)
    }

    pub async fn upload_file(&self, bucket: &str, key: &str, path: &Path, progress: &Progress) -> Res<()> {
        if let Some((v, rel)) = self.vault_for(bucket, key) {
            return v.upload_file(&self.raw(), &rel, path, progress).await;
        }
        self.upload_file_plain(bucket, key, path, progress).await
    }

    pub(super) async fn upload_file_plain(&self, bucket: &str, key: &str, path: &Path, progress: &Progress) -> Res<()> {
        if let Some(r) = &self.remote {
            return r.upload(bucket, path, key, progress).await;
        }
        let size = tokio::fs::metadata(path).await.map_err(|e| e.to_string())?.len();
        let content_type = content_type_of(path);
        let (sse, kms) = self.encryption();
        if size <= MULTIPART_THRESHOLD {
            progress.check()?;
            let body = ByteStream::from_body_1_x(ProgressBody::open(path.to_path_buf(), size, progress.clone()));
            let encrypted_by_kms = kms.is_some() || sse.as_ref().is_some_and(|s| s.as_str().starts_with("aws:kms"));
            let sent = self
                .client
                .put_object()
                .bucket(bucket)
                .key(key)
                .body(body)
                .content_length(size as i64)
                .content_type(content_type)
                .set_metadata(mtime_metadata(path))
                .set_storage_class(self.storage_class())
                .set_server_side_encryption(sse)
                .set_ssekms_key_id(kms)
                .send()
                .await;
            progress.check()?;
            let out = sent.map_err(describe)?;
            if !encrypted_by_kms {
                let local = file_md5(path).await?;
                if etag_matches(out.e_tag().unwrap_or_default(), &local) == Some(false) {
                    return Err(tr(
                        "The uploaded object differs from the file; it was damaged on the way. Upload it again.",
                    ));
                }
            }
            return Ok(());
        }
        let start = progress.done.load(Ordering::Relaxed);
        match self.resumable_upload(bucket, key, &content_type, size, path, progress, true).await {
            // Some servers count interrupted parts against the size limit; start over once.
            Err(Resume::Continued(_)) => {
                progress.done.store(start, Ordering::Relaxed);
                self.resumable_upload(bucket, key, &content_type, size, path, progress, false)
                    .await
                    .map_err(Resume::into_error)
            }
            result => result.map_err(Resume::into_error),
        }
    }

    pub fn resume_parts(&self, bucket: &str, key: &str, path: &Path) -> usize {
        let state_path = resume_path(&format!("{}\n{bucket}\n{key}\n{}", self.profile.id, path.display()));
        std::fs::read(state_path)
            .ok()
            .and_then(|d| serde_json::from_slice::<ResumeState>(&d).ok())
            .map(|s| s.parts)
            .unwrap_or(0)
    }

    async fn sweep_resumes(&self) {
        let dir = gtk::glib::user_cache_dir().join("ferry").join("resume");
        let Ok(entries) = std::fs::read_dir(&dir) else { return };
        let week = std::time::Duration::from_secs(7 * 86_400);
        for entry in entries.flatten() {
            let old = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > week);
            if !old {
                continue;
            }
            let Some(state) =
                std::fs::read(entry.path()).ok().and_then(|d| serde_json::from_slice::<ResumeState>(&d).ok())
            else {
                let _ = std::fs::remove_file(entry.path());
                continue;
            };
            if state.profile != self.profile.id {
                continue;
            }
            let _ = self
                .client
                .abort_multipart_upload()
                .bucket(&state.bucket)
                .key(&state.key)
                .upload_id(&state.upload_id)
                .send()
                .await;
            let _ = std::fs::remove_file(entry.path());
        }
    }

    async fn resumable_upload(
        &self,
        bucket: &str,
        key: &str,
        content_type: &str,
        size: u64,
        path: &Path,
        progress: &Progress,
        may_continue: bool,
    ) -> Result<(), Resume> {
        let part_size = PART_SIZE.max(size.div_ceil(MAX_PARTS));
        let modified = tokio::fs::metadata(path)
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let state_path = resume_path(&format!("{}\n{bucket}\n{key}\n{}", self.profile.id, path.display()));
        let mut done: std::collections::BTreeMap<i32, String> = std::collections::BTreeMap::new();
        let mut upload_id = String::new();
        if let Some(state) =
            std::fs::read(&state_path).ok().and_then(|d| serde_json::from_slice::<ResumeState>(&d).ok())
            && may_continue
            && state.size == size
            && state.modified == modified
            && state.part_size == part_size
        {
            let mut marker: Option<String> = None;
            let mut alive = true;
            loop {
                match self
                    .client
                    .list_parts()
                    .bucket(bucket)
                    .key(key)
                    .upload_id(&state.upload_id)
                    .set_part_number_marker(marker.clone())
                    .send()
                    .await
                {
                    Ok(out) => {
                        for part in out.parts() {
                            if let (Some(n), Some(etag)) = (part.part_number(), part.e_tag()) {
                                done.insert(n, etag.to_string());
                            }
                        }
                        marker = out.next_part_number_marker().map(str::to_string);
                        if !out.is_truncated().unwrap_or(false) || marker.is_none() {
                            break;
                        }
                    }
                    Err(_) => {
                        alive = false;
                        break;
                    }
                }
            }
            if alive {
                upload_id = state.upload_id;
            } else {
                done.clear();
            }
        }
        let continued = !upload_id.is_empty();
        if upload_id.is_empty() {
            let (sse, kms) = self.encryption();
            let created = self
                .client
                .create_multipart_upload()
                .bucket(bucket)
                .key(key)
                .content_type(content_type)
                .set_metadata(mtime_metadata(path))
                .set_storage_class(self.storage_class())
                .set_server_side_encryption(sse)
                .set_ssekms_key_id(kms)
                .send()
                .await
                .map_err(describe)?;
            upload_id = created.upload_id().unwrap_or_default().to_string();
        }
        let save = |done: &std::collections::BTreeMap<i32, String>| {
            let state = ResumeState {
                upload_id: upload_id.clone(),
                part_size,
                size,
                modified,
                parts: done.len(),
                profile: self.profile.id.clone(),
                bucket: bucket.to_string(),
                key: key.to_string(),
            };
            if let Ok(data) = serde_json::to_vec(&state) {
                let _ = std::fs::write(&state_path, data);
            }
        };
        save(&done);
        let count = size.div_ceil(part_size) as i32;
        let length_of = |number: i32| part_size.min(size - (number as u64 - 1) * part_size);
        for number in done.keys() {
            progress.add(length_of(*number));
        }
        let missing: Vec<i32> = (1..=count).filter(|n| !done.contains_key(n)).collect();
        let concurrency = if count >= 4 { 3 } else { 1 };
        // With SSE-KMS a part's ETag is not its MD5.
        let kms_parts = self.profile.encryption == "aws:kms";
        let done = std::sync::Mutex::new(done);
        {
            use futures_util::{StreamExt, TryStreamExt};
            let (done, save, upload_id, length_of) = (&done, &save, upload_id.as_str(), &length_of);
            let sent = futures_util::stream::iter(missing)
                .map(|number| async move {
                    progress.check()?;
                    let (offset, length) = ((number as u64 - 1) * part_size, length_of(number));
                    let mut file = tokio::fs::File::open(path).await.map_err(|e| e.to_string())?;
                    use tokio::io::AsyncSeekExt;
                    file.seek(std::io::SeekFrom::Start(offset)).await.map_err(|e| e.to_string())?;
                    let mut chunk = vec![0u8; length as usize];
                    file.read_exact(&mut chunk).await.map_err(|e| e.to_string())?;
                    drop(file);
                    let local = {
                        use md5::Digest;
                        format!("{:x}", md5::Md5::digest(&chunk))
                    };
                    throttle(length).await;
                    progress.check()?;
                    let counted = Arc::new(AtomicU64::new(0));
                    let sent = self
                        .client
                        .upload_part()
                        .bucket(bucket)
                        .key(key)
                        .upload_id(upload_id)
                        .part_number(number)
                        .content_length(length as i64)
                        .body(chunk_body(bytes::Bytes::from(chunk), progress.clone(), counted.clone()))
                        .send()
                        .await;
                    progress.done.fetch_sub(counted.swap(0, Ordering::Relaxed), Ordering::Relaxed);
                    let out = match sent {
                        Ok(out) => out,
                        Err(error) => {
                            progress.check()?;
                            return Err(describe(error));
                        }
                    };
                    if !kms_parts && etag_matches(out.e_tag().unwrap_or_default(), &local) == Some(false) {
                        return Err(trf(
                            "Part {n} was damaged on the way. Try the upload again.",
                            &[("n", &number.to_string())],
                        ));
                    }
                    progress.add(length);
                    let mut done = done.lock().unwrap();
                    done.insert(number, out.e_tag().unwrap_or_default().to_string());
                    save(&done);
                    Ok::<(), String>(())
                })
                .buffer_unordered(concurrency)
                .try_collect::<Vec<()>>()
                .await;
            if let Err(error) = sent {
                // Interrupted parts can push a resumed upload over the size limit, start over.
                if continued && error.contains("EntityTooLarge") {
                    let _ =
                        self.client.abort_multipart_upload().bucket(bucket).key(key).upload_id(upload_id).send().await;
                    let _ = std::fs::remove_file(&state_path);
                    return Err(Resume::Continued(error));
                }
                return Err(Resume::Failed(error));
            }
        }
        let done = done.into_inner().unwrap();
        let parts = done.iter().map(|(n, etag)| CompletedPart::builder().part_number(*n).e_tag(etag).build()).collect();
        let completed = self
            .client
            .complete_multipart_upload()
            .bucket(bucket)
            .key(key)
            .upload_id(&upload_id)
            .multipart_upload(CompletedMultipartUpload::builder().set_parts(Some(parts)).build())
            .send()
            .await
            .map_err(describe);
        let _ = std::fs::remove_file(&state_path);
        if let Err(error) = completed {
            let _ = self.client.abort_multipart_upload().bucket(bucket).key(key).upload_id(&upload_id).send().await;
            return Err(if continued { Resume::Continued(error) } else { Resume::Failed(error) });
        }
        Ok(())
    }

    async fn multipart(
        &self,
        bucket: &str,
        key: &str,
        content_type: &str,
        size: u64,
        mut source: Source,
        progress: &Progress,
    ) -> Res<()> {
        let part_size = PART_SIZE.max(size.div_ceil(MAX_PARTS));
        let (sse, kms) = self.encryption();
        let created = self
            .client
            .create_multipart_upload()
            .bucket(bucket)
            .key(key)
            .content_type(content_type)
            .set_storage_class(self.storage_class())
            .set_server_side_encryption(sse)
            .set_ssekms_key_id(kms)
            .send()
            .await
            .map_err(describe)?;
        let upload_id = created.upload_id().unwrap_or_default().to_string();
        let result = async {
            let mut parts = Vec::new();
            let mut number = 1;
            loop {
                progress.check()?;
                let chunk = source.read_part(part_size as usize).await?;
                if chunk.is_empty() {
                    break;
                }
                let length = chunk.len() as u64;
                throttle(length).await;
                progress.check()?;
                let out = self
                    .client
                    .upload_part()
                    .bucket(bucket)
                    .key(key)
                    .upload_id(&upload_id)
                    .part_number(number)
                    .body(ByteStream::from(chunk))
                    .send()
                    .await
                    .map_err(describe)?;
                parts.push(
                    CompletedPart::builder().part_number(number).set_e_tag(out.e_tag().map(str::to_string)).build(),
                );
                progress.add(length);
                number += 1;
            }
            let completed = CompletedMultipartUpload::builder().set_parts(Some(parts)).build();
            self.client
                .complete_multipart_upload()
                .bucket(bucket)
                .key(key)
                .upload_id(&upload_id)
                .multipart_upload(completed)
                .send()
                .await
                .map_err(describe)?;
            Ok(())
        }
        .await;
        if result.is_err() {
            let _ = self.client.abort_multipart_upload().bucket(bucket).key(key).upload_id(&upload_id).send().await;
        }
        result
    }

    pub async fn download_sized(
        &self,
        bucket: &str,
        key: &str,
        size: u64,
        path: &Path,
        progress: &Progress,
    ) -> Res<()> {
        if size >= PARALLEL_DOWNLOAD {
            self.download_chunked(bucket, key, path, progress).await
        } else {
            self.download_file(bucket, key, None, path, progress).await
        }
    }

    async fn download_chunked(&self, bucket: &str, key: &str, path: &Path, progress: &Progress) -> Res<()> {
        use std::os::unix::fs::FileExt;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| e.to_string())?;
        }
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".part");
        let partial = path.with_file_name(&name);
        name.push(".json");
        let state_path = path.with_file_name(name);
        let head = self.client.head_object().bucket(bucket).key(key).send().await.map_err(describe)?;
        let size = head.content_length().unwrap_or(0).max(0) as u64;
        let etag = head.e_tag().unwrap_or_default().to_string();
        let last_modified = file_time(head.metadata(), head.last_modified());
        let count = size.div_ceil(CHUNK);
        let mut done: std::collections::BTreeSet<u64> = std::fs::read(&state_path)
            .ok()
            .and_then(|d| serde_json::from_slice::<ChunkState>(&d).ok())
            .filter(|s| {
                s.etag == etag
                    && s.size == size
                    && std::fs::metadata(&partial).map(|m| m.len() == size).unwrap_or(false)
            })
            .map(|s| s.done.into_iter().collect())
            .unwrap_or_default();
        if done.is_empty() {
            let file = std::fs::File::create(&partial).map_err(|e| e.to_string())?;
            file.set_len(size).map_err(|e| e.to_string())?;
        }
        for index in &done {
            progress.add(CHUNK.min(size - index * CHUNK));
        }
        let file = Arc::new(std::fs::OpenOptions::new().write(true).open(&partial).map_err(|e| e.to_string())?);
        let missing: Vec<u64> = (0..count).filter(|i| !done.contains(i)).collect();
        let state = std::sync::Mutex::new(std::mem::take(&mut done));
        {
            use futures_util::{StreamExt, TryStreamExt};
            let (state, etag, file, state_path) = (&state, &etag, &file, &state_path);
            futures_util::stream::iter(missing)
                .map(|index| async move {
                    progress.check()?;
                    let start = index * CHUNK;
                    let end = (start + CHUNK).min(size) - 1;
                    let out = self
                        .client
                        .get_object()
                        .bucket(bucket)
                        .key(key)
                        .range(format!("bytes={start}-{end}"))
                        .send()
                        .await
                        .map_err(describe)?;
                    let mut body = out.body;
                    let mut offset = start;
                    while let Some(bytes) = body.try_next().await.map_err(|e| e.to_string())? {
                        let file = file.clone();
                        tokio::task::block_in_place(|| file.write_all_at(&bytes, offset)).map_err(|e| e.to_string())?;
                        offset += bytes.len() as u64;
                        progress.advance(bytes.len() as u64).await?;
                    }
                    if offset != end + 1 {
                        return Err(tr("The connection ended before the download was complete"));
                    }
                    let mut done = state.lock().unwrap();
                    done.insert(index);
                    let saved = ChunkState { etag: etag.clone(), size, done: done.iter().copied().collect() };
                    if let Ok(data) = serde_json::to_vec(&saved) {
                        let _ = std::fs::write(state_path, data);
                    }
                    Ok::<(), String>(())
                })
                .buffer_unordered(PARALLEL_STREAMS)
                .try_collect::<Vec<()>>()
                .await?;
        }
        file.sync_all().map_err(|e| e.to_string())?;
        drop(file);
        tokio::fs::rename(&partial, path).await.map_err(|e| e.to_string())?;
        let _ = tokio::fs::remove_file(&state_path).await;
        if let Some(modified) = last_modified
            && let Ok(file) = std::fs::File::options().write(true).open(path)
        {
            let _ = file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(modified.max(0) as u64));
        }
        Ok(())
    }

    pub async fn download_file(
        &self,
        bucket: &str,
        key: &str,
        version: Option<&str>,
        path: &Path,
        progress: &Progress,
    ) -> Res<()> {
        if let Some((v, rel)) = self.vault_for(bucket, key) {
            return v.download_file(&self.raw(), &rel, path, progress).await;
        }
        self.download_file_plain(bucket, key, version, path, progress).await
    }

    pub(super) async fn download_file_plain(
        &self,
        bucket: &str,
        key: &str,
        version: Option<&str>,
        path: &Path,
        progress: &Progress,
    ) -> Res<()> {
        if let Some(r) = &self.remote {
            return r.download(bucket, key, path, progress).await;
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| e.to_string())?;
        }
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".part");
        let partial = path.with_file_name(&name);
        name.push(".etag");
        let sidecar = path.with_file_name(name);
        let have = tokio::fs::metadata(&partial).await.map(|m| m.len()).unwrap_or(0);
        let mut start = 0u64;
        if have > 0
            && let Ok(saved) = tokio::fs::read_to_string(&sidecar).await
        {
            let head = self
                .client
                .head_object()
                .bucket(bucket)
                .key(key)
                .set_version_id(version.map(str::to_string))
                .send()
                .await
                .map_err(describe)?;
            let current = head.e_tag().unwrap_or_default().to_string();
            if !current.is_empty() && current == saved.trim() && (have as i64) < head.content_length().unwrap_or(0) {
                start = have;
            }
        }
        let mut request = self.client.get_object().bucket(bucket).key(key).set_version_id(version.map(str::to_string));
        if start > 0 {
            request = request.range(format!("bytes={start}-"));
        }
        let out = request.send().await.map_err(describe)?;
        let etag_header = out.e_tag().unwrap_or_default().to_string();
        let last_modified = file_time(out.metadata(), out.last_modified());
        let _ = tokio::fs::write(&sidecar, &etag_header).await;
        let etag = etag_header.trim_matches('"').to_lowercase();
        let kms = out.server_side_encryption().is_some_and(|e| e.as_str().starts_with("aws:kms"));
        let verify = !kms && etag.len() == 32 && etag.bytes().all(|b| b.is_ascii_hexdigit());
        let result = async {
            use md5::Digest;
            let mut hasher = md5::Md5::new();
            let mut file = if start > 0 {
                if verify {
                    let mut existing = tokio::fs::File::open(&partial).await.map_err(|e| e.to_string())?;
                    let mut buffer = vec![0u8; 1024 * 1024];
                    loop {
                        let read = existing.read(&mut buffer).await.map_err(|e| e.to_string())?;
                        if read == 0 { break; }
                        hasher.update(&buffer[..read]);
                    }
                }
                progress.add(start);
                tokio::fs::OpenOptions::new().append(true).open(&partial).await.map_err(|e| e.to_string())?
            } else {
                tokio::fs::File::create(&partial).await.map_err(|e| e.to_string())?
            };
            let mut body = out.body;
            while let Some(chunk) = body.try_next().await.map_err(|e| e.to_string())? {
                if verify { hasher.update(&chunk); }
                file.write_all(&chunk).await.map_err(|e| e.to_string())?;
                progress.advance(chunk.len() as u64).await?;
            }
            file.flush().await.map_err(|e| e.to_string())?;
            drop(file);
            if verify {
                let actual: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
                if actual != etag {
                    let _ = tokio::fs::remove_file(&partial).await;
                    return Err(trf("The checksum of the downloaded file does not match (expected {expected}…, got {actual}…); the file was not saved",
                        &[("expected", &etag[..8]), ("actual", &actual[..8])]));
                }
            }
            tokio::fs::rename(&partial, path).await.map_err(|e| e.to_string())
        }.await;
        if result.is_ok() {
            let _ = tokio::fs::remove_file(&sidecar).await;
            if let Some(modified) = last_modified
                && let Ok(file) = std::fs::File::options().write(true).open(path)
            {
                let _ =
                    file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(modified.max(0) as u64));
            }
        }
        result
    }

    pub async fn download_archive(
        &self,
        bucket: &str,
        prefix: &str,
        keys: Vec<String>,
        path: &Path,
        progress: &Progress,
    ) -> Res<()> {
        self.plain_only(bucket, prefix)?;
        self.s3_only()?;
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        const STORED: &[&str] = &[
            "zip", "gz", "tgz", "xz", "bz2", "zst", "7z", "rar", "jpg", "jpeg", "png", "gif", "webp", "avif", "heic",
            "mp4", "mkv", "webm", "mov", "mp3", "ogg", "opus", "flac", "m4a", "pdf", "docx", "xlsx", "pptx", "odt",
            "ods", "odp", "epub", "jar", "apk",
        ];
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".part");
        let partial = path.with_file_name(name);
        let file = std::fs::File::create(&partial).map_err(|e| e.to_string())?;
        let mut writer = zip::ZipWriter::new(std::io::BufWriter::with_capacity(1024 * 1024, file));
        let result = async {
            for key in keys {
                progress.check()?;
                let raw = key.strip_prefix(prefix).unwrap_or(&key);
                let folder = raw.ends_with('/');
                let parts: Vec<&str> = raw.split('/').filter(|p| !p.is_empty() && *p != "." && *p != "..").collect();
                if parts.is_empty() {
                    continue;
                }
                let inner = format!("{}{}", parts.join("/"), if folder { "/" } else { "" });
                if inner.ends_with('/') {
                    writer.add_directory(inner.as_str(), SimpleFileOptions::default()).map_err(|e| e.to_string())?;
                    continue;
                }
                let out = self.client.get_object().bucket(bucket).key(&key).send().await.map_err(describe)?;
                let extension = inner.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_default();
                let method = if STORED.contains(&extension.as_str()) {
                    zip::CompressionMethod::Stored
                } else {
                    zip::CompressionMethod::Deflated
                };
                let mut options = SimpleFileOptions::default()
                    .compression_method(method)
                    .large_file(out.content_length().unwrap_or(0) >= u32::MAX as i64);
                if let Some(modified) = out.last_modified()
                    && let Ok(time) = gtk::glib::DateTime::from_unix_utc(modified.secs())
                    && let Ok(stamp) = zip::DateTime::from_date_and_time(
                        time.year() as u16,
                        time.month() as u8,
                        time.day_of_month() as u8,
                        time.hour() as u8,
                        time.minute() as u8,
                        time.second() as u8,
                    )
                {
                    options = options.last_modified_time(stamp);
                }
                writer.start_file(inner.as_str(), options).map_err(|e| e.to_string())?;
                let mut body = out.body;
                while let Some(chunk) = body.try_next().await.map_err(|e| e.to_string())? {
                    tokio::task::block_in_place(|| writer.write_all(&chunk)).map_err(|e| e.to_string())?;
                    progress.advance(chunk.len() as u64).await?;
                }
            }
            let file = tokio::task::block_in_place(|| writer.finish()).map_err(|e| e.to_string())?;
            file.into_inner().map_err(|e| e.to_string())?.sync_all().map_err(|e| e.to_string())?;
            tokio::fs::rename(&partial, path).await.map_err(|e| e.to_string())
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&partial).await;
        }
        result
    }

    pub async fn copy_to(
        &self,
        bucket: &str,
        key: &str,
        dst: &S3,
        dst_bucket: &str,
        dst_key: &str,
        progress: &Progress,
    ) -> Res<()> {
        if self.vault_for(bucket, key).is_some()
            || dst.vault_for(dst_bucket, dst_key).is_some()
            || self.remote.is_some()
            || dst.remote.is_some()
        {
            return self.copy_through(bucket, key, dst, dst_bucket, dst_key, progress).await;
        }
        if self.profile.id == dst.profile.id {
            progress.check()?;
            let size = self.head_object(bucket, key).await.map(|i| i.size).unwrap_or(0);
            self.copy_object(bucket, key, dst_bucket, dst_key).await?;
            progress.add(size.max(0) as u64);
            return Ok(());
        }
        let out = self.client.get_object().bucket(bucket).key(key).send().await.map_err(describe)?;
        let size = out.content_length().unwrap_or(0).max(0) as u64;
        let content_type = out.content_type().unwrap_or("application/octet-stream").to_string();
        if size > MULTIPART_THRESHOLD {
            return dst
                .multipart(dst_bucket, dst_key, &content_type, size, Source::Body(out.body, Vec::new()), progress)
                .await;
        }
        progress.check()?;
        throttle(size).await;
        let data = out.body.collect().await.map_err(|e| e.to_string())?.into_bytes();
        let (sse, kms) = dst.encryption();
        dst.client
            .put_object()
            .bucket(dst_bucket)
            .key(dst_key)
            .body(ByteStream::from(data))
            .content_type(content_type)
            .set_storage_class(dst.storage_class())
            .set_server_side_encryption(sse)
            .set_ssekms_key_id(kms)
            .send()
            .await
            .map_err(describe)?;
        progress.add(size);
        Ok(())
    }

    async fn copy_through(
        &self,
        bucket: &str,
        key: &str,
        dst: &S3,
        dst_bucket: &str,
        dst_key: &str,
        progress: &Progress,
    ) -> Res<()> {
        let dir = gtk::glib::user_cache_dir().join("ferry").join("vault");
        tokio::fs::create_dir_all(&dir).await.map_err(|e| e.to_string())?;
        let temp = dir.join(gtk::glib::uuid_string_random().as_str());
        let result = async {
            self.download_file(bucket, key, None, &temp, progress).await?;
            dst.upload_file(dst_bucket, dst_key, &temp, &Progress::default()).await
        }
        .await;
        let _ = tokio::fs::remove_file(&temp).await;
        result
    }

    pub async fn delete_object(&self, bucket: &str, key: &str) -> Res<()> {
        if self.vault_for(bucket, key).is_some() || self.remote.is_some() {
            return self.delete_keys(bucket, vec![key.to_string()]).await.map(|_| ());
        }
        self.client.delete_object().bucket(bucket).key(key).send().await.map_err(describe)?;
        Ok(())
    }
}

fn entry_of(object: &aws_sdk_s3::types::Object, prefix: &str) -> Entry {
    let key = object.key().unwrap_or_default();
    Entry {
        key: key.to_string(),
        name: key.strip_prefix(prefix).unwrap_or(key).to_string(),
        size: object.size().unwrap_or(0),
        modified: object.last_modified().map(|d| d.secs()).unwrap_or(0),
        etag: object.e_tag().unwrap_or_default().trim_matches('"').to_string(),
        storage_class: object.storage_class().map(|c| c.as_str().to_string()).unwrap_or_default(),
        is_folder: key.ends_with('/'),
    }
}

pub(crate) fn content_type_of(path: &Path) -> String {
    let (guess, _) = gtk::gio::content_type_guess(Some(path), None::<&[u8]>);
    gtk::gio::content_type_get_mime_type(&guess)
        .map(|m| m.to_string())
        .unwrap_or_else(|| "application/octet-stream".to_string())
}

enum Resume {
    Continued(String),
    Failed(String),
}

impl Resume {
    fn into_error(self) -> String {
        match self {
            Resume::Continued(e) | Resume::Failed(e) => e,
        }
    }
}

impl From<String> for Resume {
    fn from(error: String) -> Self {
        Resume::Failed(error)
    }
}

fn mtime_metadata(path: &Path) -> Option<std::collections::HashMap<String, String>> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(std::collections::HashMap::from([(
        "mtime".to_string(),
        format!("{}.{:09}", modified.as_secs(), modified.subsec_nanos()),
    )]))
}

fn file_time(
    metadata: Option<&std::collections::HashMap<String, String>>,
    last_modified: Option<&aws_sdk_s3::primitives::DateTime>,
) -> Option<i64> {
    metadata
        .and_then(|m| m.get("mtime"))
        .and_then(|v| v.split('.').next()?.parse::<i64>().ok())
        .or_else(|| last_modified.map(|d| d.secs()))
}

const PARALLEL_DOWNLOAD: u64 = 32 * 1024 * 1024;
const CHUNK: u64 = 8 * 1024 * 1024;
const PARALLEL_STREAMS: usize = 4;

#[derive(serde::Serialize, serde::Deserialize)]
struct ChunkState {
    etag: String,
    size: u64,
    done: Vec<u64>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ResumeState {
    upload_id: String,
    part_size: u64,
    size: u64,
    modified: i64,
    parts: usize,
    #[serde(default)]
    profile: String,
    #[serde(default)]
    bucket: String,
    #[serde(default)]
    key: String,
}

fn resume_path(identity: &str) -> PathBuf {
    use sha2::Digest;
    let dir = gtk::glib::user_cache_dir().join("ferry").join("resume");
    let _ = std::fs::create_dir_all(&dir);
    let hash: String = sha2::Sha256::digest(identity.as_bytes()).iter().take(16).map(|b| format!("{b:02x}")).collect();
    dir.join(format!("{hash}.json"))
}

struct ProgressBody {
    receiver: tokio::sync::mpsc::Receiver<Result<bytes::Bytes, std::io::Error>>,
    size: u64,
}

impl ProgressBody {
    fn open(path: PathBuf, size: u64, progress: Progress) -> Self {
        let (sender, receiver) = tokio::sync::mpsc::channel(4);
        tokio::spawn(async move {
            let mut file = match tokio::fs::File::open(&path).await {
                Ok(file) => file,
                Err(error) => {
                    let _ = sender.send(Err(error)).await;
                    return;
                }
            };
            loop {
                if progress.cancel.load(Ordering::Relaxed) {
                    let _ = sender.send(Err(std::io::Error::other(CANCELLED))).await;
                    return;
                }
                let mut chunk = vec![0u8; 128 * 1024];
                match file.read(&mut chunk).await {
                    Ok(0) => return,
                    Ok(read) => {
                        chunk.truncate(read);
                        throttle(read as u64).await;
                        progress.add(read as u64);
                        if sender.send(Ok(bytes::Bytes::from(chunk))).await.is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(Err(error)).await;
                        return;
                    }
                }
            }
        });
        ProgressBody { receiver, size }
    }
}

impl http_body::Body for ProgressBody {
    type Data = bytes::Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        self.receiver.poll_recv(cx).map(|item| item.map(|chunk| chunk.map(http_body::Frame::data)))
    }

    fn size_hint(&self) -> http_body::SizeHint {
        http_body::SizeHint::with_exact(self.size)
    }
}

fn chunk_body(data: bytes::Bytes, progress: Progress, counted: Arc<AtomicU64>) -> ByteStream {
    ByteStream::new(aws_sdk_s3::primitives::SdkBody::retryable(move || {
        progress.done.fetch_sub(counted.swap(0, Ordering::Relaxed), Ordering::Relaxed);
        aws_sdk_s3::primitives::SdkBody::from_body_1_x(ChunkBody {
            data: data.clone(),
            progress: progress.clone(),
            counted: counted.clone(),
        })
    }))
}

struct ChunkBody {
    data: bytes::Bytes,
    progress: Progress,
    counted: Arc<AtomicU64>,
}

impl http_body::Body for ChunkBody {
    type Data = bytes::Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        if self.data.is_empty() {
            return std::task::Poll::Ready(None);
        }
        if self.progress.cancel.load(Ordering::Relaxed) {
            return std::task::Poll::Ready(Some(Err(std::io::Error::other(CANCELLED))));
        }
        let take = self.data.len().min(128 * 1024);
        let piece = self.data.split_to(take);
        self.progress.add(take as u64);
        self.counted.fetch_add(take as u64, Ordering::Relaxed);
        std::task::Poll::Ready(Some(Ok(http_body::Frame::data(piece))))
    }

    fn is_end_stream(&self) -> bool {
        self.data.is_empty()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        http_body::SizeHint::with_exact(self.data.len() as u64)
    }
}

enum Source {
    Body(ByteStream, Vec<u8>),
}

impl Source {
    async fn read_part(&mut self, size: usize) -> Res<Vec<u8>> {
        match self {
            Source::Body(body, rest) => {
                let mut buffer = std::mem::take(rest);
                while buffer.len() < size {
                    match body.try_next().await.map_err(|e| e.to_string())? {
                        Some(chunk) => buffer.extend_from_slice(&chunk),
                        None => break,
                    }
                }
                if buffer.len() > size {
                    *rest = buffer.split_off(size);
                }
                Ok(buffer)
            }
        }
    }
}

pub fn listing_csv(items: &[Entry]) -> String {
    let field = |text: &str| {
        if text.contains([',', '"', '\n', '\r']) {
            format!("\"{}\"", text.replace('"', "\"\""))
        } else {
            text.to_string()
        }
    };
    let mut out = String::from("key,size,last_modified,etag,storage_class\n");
    for item in items.iter().filter(|e| !e.key.ends_with('/')) {
        let date = gtk::glib::DateTime::from_unix_utc(item.modified)
            .ok()
            .and_then(|d| d.format_iso8601().ok())
            .map(|s| s.to_string())
            .unwrap_or_default();
        out.push_str(&format!(
            "{},{},{},{},{}\n",
            field(&item.key),
            item.size,
            date,
            field(item.etag.trim_matches('"')),
            field(&item.storage_class)
        ));
    }
    out
}

pub static UPLOAD_SKIP: std::sync::RwLock<Vec<String>> = std::sync::RwLock::new(Vec::new());

pub fn skipped(name: &str) -> bool {
    let name = name.to_lowercase();
    UPLOAD_SKIP
        .read()
        .map(|patterns| patterns.iter().any(|p| crate::window::wildcard_match(&p.to_lowercase(), &name)))
        .unwrap_or(false)
}

pub fn collect_uploads(prefix: &str, paths: &[PathBuf]) -> Res<Vec<(PathBuf, String, u64)>> {
    fn walk(dir: &Path, key: &str, jobs: &mut Vec<(PathBuf, String, u64)>) -> Res<()> {
        let mut entries: Vec<_> = std::fs::read_dir(dir).map_err(|e| e.to_string())?.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            if skipped(&name) {
                continue;
            }
            // Follow symlinks for files but not folders, to avoid loops.
            let Ok(meta) = std::fs::metadata(entry.path()) else { continue };
            let link = entry.file_type().map(|t| t.is_symlink()).unwrap_or(false);
            if meta.is_dir() {
                if !link {
                    walk(&entry.path(), &format!("{key}{name}/"), jobs)?;
                }
            } else if meta.is_file() {
                jobs.push((entry.path(), format!("{key}{name}"), meta.len()));
            }
        }
        Ok(())
    }
    let mut jobs = Vec::new();
    for path in paths {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if meta.is_dir() {
            walk(path, &format!("{prefix}{name}/"), &mut jobs)?;
        } else {
            jobs.push((path.clone(), format!("{prefix}{name}"), meta.len()));
        }
    }
    Ok(jobs)
}

pub fn download_target(dir: &Path, prefix: &str, key: &str) -> Res<PathBuf> {
    let relative = key.strip_prefix(prefix).unwrap_or(key);
    let mut path = dir.to_path_buf();
    for part in relative.split('/').filter(|p| !p.is_empty()) {
        if part == "." || part == ".." || part.contains('\0') {
            return Err(tr("Object name must stay within the selected download directory"));
        }
        path.push(part);
    }
    Ok(path)
}

async fn file_md5(path: &Path) -> Res<String> {
    use md5::Digest;
    let mut file = tokio::fs::File::open(path).await.map_err(|e| e.to_string())?;
    let mut hasher = md5::Md5::new();
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buffer).await.map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn etag_matches(etag: &str, md5_hex: &str) -> Option<bool> {
    let etag = etag.trim_matches('"').to_lowercase();
    (etag.len() == 32 && etag.bytes().all(|b| b.is_ascii_hexdigit())).then(|| etag == md5_hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uploads_leave_out_clutter() {
        let dir = std::env::temp_dir().join(format!("ferry-skip-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        for name in ["a.txt", ".DS_Store", "work.tmp", "node_modules/x.js"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        *UPLOAD_SKIP.write().unwrap() = vec![".DS_Store".into(), "*.TMP".into(), "node_modules".into()];
        let jobs = collect_uploads("p/", std::slice::from_ref(&dir)).unwrap();
        let keys: Vec<String> = jobs.into_iter().map(|(_, k, _)| k).collect();
        UPLOAD_SKIP.write().unwrap().clear();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(keys.len(), 1, "{keys:?}");
        assert!(keys[0].ends_with("/a.txt"));
    }

    #[test]
    fn csv_quoting() {
        let items = vec![
            Entry {
                key: "a/plain.txt".into(),
                size: 5,
                modified: 0,
                etag: "\"abc\"".into(),
                storage_class: "STANDARD".into(),
                ..Default::default()
            },
            Entry { key: "a/with, comma \"quoted\".txt".into(), size: 1, ..Default::default() },
            Entry { key: "a/folder/".into(), is_folder: true, ..Default::default() },
        ];
        let csv = listing_csv(&items);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1], "a/plain.txt,5,1970-01-01T00:00:00Z,abc,STANDARD");
        assert!(lines[2].starts_with("\"a/with, comma \"\"quoted\"\".txt\",1,"));
    }

    #[test]
    fn etags() {
        assert_eq!(
            etag_matches("\"5d41402abc4b2a76b9719d911017c592\"", "5d41402abc4b2a76b9719d911017c592"),
            Some(true)
        );
        assert_eq!(etag_matches("5D41402ABC4B2A76B9719D911017C592", "5d41402abc4b2a76b9719d911017c592"), Some(true));
        assert_eq!(
            etag_matches("\"5d41402abc4b2a76b9719d911017c593\"", "5d41402abc4b2a76b9719d911017c592"),
            Some(false)
        );
        assert_eq!(etag_matches("\"d41d8cd98f00b204e9800998ecf8427e-3\"", "x"), None);
    }

    #[test]
    fn public_urls() {
        let supabase = Profile {
            provider: "supabase".into(),
            endpoint: "https://abc.storage.supabase.co/storage/v1/s3".into(),
            path_style: true,
            ..Default::default()
        };
        assert_eq!(
            public_url(&supabase, "media", "a b/c.png"),
            "https://abc.storage.supabase.co/storage/v1/object/public/media/a%20b/c.png"
        );
        let aws = Profile { provider: "aws".into(), region: "eu-west-1".into(), ..Default::default() };
        assert_eq!(public_url(&aws, "site", "index.html"), "https://site.s3.eu-west-1.amazonaws.com/index.html");
        let minio = Profile {
            provider: "minio".into(),
            endpoint: "http://localhost:9000".into(),
            path_style: true,
            ..Default::default()
        };
        assert_eq!(public_url(&minio, "b", "k"), "http://localhost:9000/b/k");
        let r2 = Profile {
            provider: "r2".into(),
            endpoint: "https://acct.r2.cloudflarestorage.com".into(),
            path_style: false,
            ..Default::default()
        };
        assert_eq!(public_url(&r2, "b", "k"), "https://b.acct.r2.cloudflarestorage.com/k");
    }
}
