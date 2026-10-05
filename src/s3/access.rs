use aws_sdk_s3::primitives::DateTime;
use aws_sdk_s3::types::{
    AccessControlPolicy, BucketLoggingStatus, DefaultRetention, ErrorDocument, GlacierJobParameters, Grant, Grantee,
    IndexDocument, LoggingEnabled, ObjectLockConfiguration, ObjectLockEnabled, ObjectLockLegalHold,
    ObjectLockLegalHoldStatus, ObjectLockRetention, ObjectLockRetentionMode, ObjectLockRule, Owner, Payer, Permission,
    PublicAccessBlockConfiguration, RequestPaymentConfiguration, RestoreRequest, Tier, Type, WebsiteConfiguration,
};

use crate::i18n::tr;
use crate::s3::{Res, S3, describe, error_code};

pub const ALL_USERS: &str = "http://acs.amazonaws.com/groups/global/AllUsers";
pub const AUTHENTICATED_USERS: &str = "http://acs.amazonaws.com/groups/global/AuthenticatedUsers";
pub const LOG_DELIVERY: &str = "http://acs.amazonaws.com/groups/s3/LogDelivery";
pub const PERMISSIONS: [&str; 5] = ["READ", "WRITE", "READ_ACP", "WRITE_ACP", "FULL_CONTROL"];

#[derive(Clone, Debug, PartialEq)]
pub struct AclGrant {
    pub kind: String,
    pub grantee: String,
    pub name: String,
    pub permission: String,
}

#[derive(Clone, Debug, Default)]
pub struct Acl {
    pub owner_id: String,
    pub owner_name: String,
    pub grants: Vec<AclGrant>,
}

impl Acl {
    pub fn is_public(&self) -> bool {
        self.grants.iter().any(|g| g.grantee == ALL_USERS && (g.permission == "READ" || g.permission == "FULL_CONTROL"))
    }
}

#[derive(Clone, Debug, Default)]
pub struct Retention {
    pub mode: String,
    pub until: i64,
    pub legal_hold: bool,
}

#[derive(Clone, Debug, Default)]
pub struct LockConfig {
    pub enabled: bool,
    pub mode: String,
    pub days: i32,
    pub years: i32,
}

#[derive(Clone, Debug, Default)]
pub struct Hosting {
    pub index: String,
    pub error: String,
    pub logging_bucket: String,
    pub logging_prefix: String,
    pub requester_pays: Option<bool>,
    pub public_block: Option<[bool; 4]>,
}

#[derive(Clone, Debug, Default)]
pub struct Distribution {
    pub id: String,
    pub domain: String,
    pub status: String,
    pub enabled: bool,
    pub comment: String,
    pub aliases: Vec<String>,
    pub origin: String,
}

fn acl_of(owner: Option<&Owner>, grants: &[Grant]) -> Acl {
    Acl {
        owner_id: owner.and_then(|o| o.id()).unwrap_or_default().to_string(),
        owner_name: owner.and_then(|o| o.display_name()).unwrap_or_default().to_string(),
        grants: grants
            .iter()
            .filter_map(|g| {
                let grantee = g.grantee()?;
                let kind = grantee.r#type().as_str().to_string();
                let id = grantee.uri().or(grantee.id()).or(grantee.email_address()).unwrap_or_default().to_string();
                Some(AclGrant {
                    kind,
                    grantee: id,
                    name: grantee.display_name().unwrap_or_default().to_string(),
                    permission: g.permission()?.as_str().to_string(),
                })
            })
            .collect(),
    }
}

fn policy_of(acl: &Acl) -> Res<AccessControlPolicy> {
    if acl.owner_id.is_empty() {
        return Err(tr("The provider did not report the owner; permissions cannot be edited"));
    }
    if acl.grants.len() > 100 {
        return Err(tr("Too many permission entries"));
    }
    let mut grants = Vec::new();
    for g in &acl.grants {
        if g.grantee.trim().is_empty() || !PERMISSIONS.contains(&g.permission.as_str()) {
            return Err(tr("A permission entry is invalid: the grantee is empty or the permission is unknown"));
        }
        let builder = Grantee::builder().r#type(Type::from(g.kind.as_str()));
        let grantee = match g.kind.as_str() {
            "Group" => builder.uri(&g.grantee),
            "AmazonCustomerByEmail" => builder.email_address(&g.grantee),
            _ => builder.id(&g.grantee),
        }
        .build()
        .map_err(|e| e.to_string())?;
        grants.push(Grant::builder().grantee(grantee).permission(Permission::from(g.permission.as_str())).build());
    }
    let owner = Owner::builder().id(&acl.owner_id).build();
    Ok(AccessControlPolicy::builder().owner(owner).set_grants(Some(grants)).build())
}

fn missing<E: aws_sdk_s3::error::ProvideErrorMetadata>(error: &aws_sdk_s3::error::SdkError<E>, codes: &[&str]) -> bool {
    codes.contains(&error_code(error).as_str())
}

impl S3 {
    pub async fn object_acl(&self, bucket: &str, key: &str) -> Res<Acl> {
        let out = self.client.get_object_acl().bucket(bucket).key(key).send().await.map_err(describe)?;
        Ok(acl_of(out.owner(), out.grants()))
    }

    pub async fn set_object_acl(&self, bucket: &str, key: &str, acl: &Acl) -> Res<()> {
        self.client
            .put_object_acl()
            .bucket(bucket)
            .key(key)
            .access_control_policy(policy_of(acl)?)
            .send()
            .await
            .map_err(describe)?;
        Ok(())
    }

    pub async fn bucket_acl(&self, bucket: &str) -> Res<Acl> {
        let out = self.client.get_bucket_acl().bucket(bucket).send().await.map_err(describe)?;
        Ok(acl_of(out.owner(), out.grants()))
    }

    pub async fn set_bucket_acl(&self, bucket: &str, acl: &Acl) -> Res<()> {
        self.client
            .put_bucket_acl()
            .bucket(bucket)
            .access_control_policy(policy_of(acl)?)
            .send()
            .await
            .map_err(describe)?;
        Ok(())
    }

    pub async fn retention(&self, bucket: &str, key: &str) -> Res<Retention> {
        let mut result = Retention::default();
        match self.client.get_object_retention().bucket(bucket).key(key).send().await {
            Ok(out) => {
                if let Some(r) = out.retention() {
                    result.mode = r.mode().map(|m| m.as_str().to_string()).unwrap_or_default();
                    result.until = r.retain_until_date().map(|d| d.secs()).unwrap_or(0);
                }
            }
            Err(e) if missing(&e, &["NoSuchObjectLockConfiguration"]) => {}
            Err(e) => return Err(describe(e)),
        }
        if let Ok(out) = self.client.get_object_legal_hold().bucket(bucket).key(key).send().await {
            result.legal_hold =
                out.legal_hold().and_then(|h| h.status()).is_some_and(|s| *s == ObjectLockLegalHoldStatus::On);
        }
        Ok(result)
    }

    pub async fn set_retention(&self, bucket: &str, key: &str, mode: &str, until: i64, bypass: bool) -> Res<()> {
        if !matches!(mode, "GOVERNANCE" | "COMPLIANCE") {
            return Err(tr("The retention mode must be GOVERNANCE or COMPLIANCE"));
        }
        if until
            <= std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        {
            return Err(tr("The retention date must be in the future"));
        }
        let retention = ObjectLockRetention::builder()
            .mode(ObjectLockRetentionMode::from(mode))
            .retain_until_date(DateTime::from_secs(until))
            .build();
        let mut request = self.client.put_object_retention().bucket(bucket).key(key).retention(retention);
        if bypass {
            request = request.bypass_governance_retention(true);
        }
        request.send().await.map_err(describe)?;
        Ok(())
    }

    pub async fn set_legal_hold(&self, bucket: &str, key: &str, on: bool) -> Res<()> {
        let status = if on { ObjectLockLegalHoldStatus::On } else { ObjectLockLegalHoldStatus::Off };
        self.client
            .put_object_legal_hold()
            .bucket(bucket)
            .key(key)
            .legal_hold(ObjectLockLegalHold::builder().status(status).build())
            .send()
            .await
            .map_err(describe)?;
        Ok(())
    }

    pub async fn restore_archived(&self, bucket: &str, key: &str, days: i32, tier: &str) -> Res<()> {
        if !(1..=365).contains(&days) {
            return Err(tr("The restore must last between 1 and 365 days"));
        }
        let parameters = GlacierJobParameters::builder().tier(Tier::from(tier)).build().map_err(|e| e.to_string())?;
        self.client
            .restore_object()
            .bucket(bucket)
            .key(key)
            .restore_request(RestoreRequest::builder().days(days).glacier_job_parameters(parameters).build())
            .send()
            .await
            .map_err(describe)?;
        Ok(())
    }

    pub async fn restore_state(&self, bucket: &str, key: &str) -> Res<String> {
        let out = self.client.head_object().bucket(bucket).key(key).send().await.map_err(describe)?;
        Ok(out.restore().unwrap_or_default().to_string())
    }

    pub async fn lock_config(&self, bucket: &str) -> Res<LockConfig> {
        match self.client.get_object_lock_configuration().bucket(bucket).send().await {
            Ok(out) => {
                let config = out.object_lock_configuration();
                let retention = config.and_then(|c| c.rule()).and_then(|r| r.default_retention());
                Ok(LockConfig {
                    enabled: config
                        .and_then(|c| c.object_lock_enabled())
                        .is_some_and(|e| *e == ObjectLockEnabled::Enabled),
                    mode: retention.and_then(|r| r.mode()).map(|m| m.as_str().to_string()).unwrap_or_default(),
                    days: retention.and_then(|r| r.days()).unwrap_or(0),
                    years: retention.and_then(|r| r.years()).unwrap_or(0),
                })
            }
            Err(e) if missing(&e, &["ObjectLockConfigurationNotFoundError"]) => Ok(LockConfig::default()),
            Err(e) => Err(describe(e)),
        }
    }

    pub async fn set_lock_config(&self, bucket: &str, mode: &str, days: i32, years: i32) -> Res<()> {
        let mut config = ObjectLockConfiguration::builder().object_lock_enabled(ObjectLockEnabled::Enabled);
        if !mode.is_empty() {
            if (days > 0) == (years > 0) {
                return Err(tr("Give the retention period in days or in years, not both"));
            }
            let mut retention = DefaultRetention::builder().mode(ObjectLockRetentionMode::from(mode));
            retention = if days > 0 { retention.days(days) } else { retention.years(years) };
            config = config.rule(ObjectLockRule::builder().default_retention(retention.build()).build());
        }
        self.client
            .put_object_lock_configuration()
            .bucket(bucket)
            .object_lock_configuration(config.build())
            .send()
            .await
            .map_err(describe)?;
        Ok(())
    }

    pub async fn hosting(&self, bucket: &str) -> Hosting {
        let mut hosting = Hosting::default();
        if let Ok(out) = self.client.get_bucket_website().bucket(bucket).send().await {
            hosting.index = out.index_document().map(|d| d.suffix().to_string()).unwrap_or_default();
            hosting.error = out.error_document().map(|d| d.key().to_string()).unwrap_or_default();
        }
        if let Ok(out) = self.client.get_bucket_logging().bucket(bucket).send().await
            && let Some(logging) = out.logging_enabled()
        {
            hosting.logging_bucket = logging.target_bucket().to_string();
            hosting.logging_prefix = logging.target_prefix().to_string();
        }
        hosting.requester_pays = self
            .client
            .get_bucket_request_payment()
            .bucket(bucket)
            .send()
            .await
            .ok()
            .map(|out| out.payer().is_some_and(|p| *p == Payer::Requester));
        hosting.public_block = match self.client.get_public_access_block().bucket(bucket).send().await {
            Ok(out) => out.public_access_block_configuration().map(|c| {
                [
                    c.block_public_acls().unwrap_or(false),
                    c.ignore_public_acls().unwrap_or(false),
                    c.block_public_policy().unwrap_or(false),
                    c.restrict_public_buckets().unwrap_or(false),
                ]
            }),
            Err(e) if missing(&e, &["NoSuchPublicAccessBlockConfiguration"]) => Some([false; 4]),
            Err(_) => None,
        };
        hosting
    }

    pub async fn set_website(&self, bucket: &str, index: &str, error: &str) -> Res<()> {
        if index.trim().is_empty() {
            self.client.delete_bucket_website().bucket(bucket).send().await.map_err(describe)?;
            return Ok(());
        }
        if index.contains('/') {
            return Err(tr("The index document must be a file name (for example index.html)"));
        }
        let mut config = WebsiteConfiguration::builder()
            .index_document(IndexDocument::builder().suffix(index.trim()).build().map_err(|e| e.to_string())?);
        if !error.trim().is_empty() {
            config =
                config.error_document(ErrorDocument::builder().key(error.trim()).build().map_err(|e| e.to_string())?);
        }
        self.client
            .put_bucket_website()
            .bucket(bucket)
            .website_configuration(config.build())
            .send()
            .await
            .map_err(describe)?;
        Ok(())
    }

    pub async fn set_logging(&self, bucket: &str, target: &str, prefix: &str) -> Res<()> {
        let mut status = BucketLoggingStatus::builder();
        if !target.trim().is_empty() {
            status = status.logging_enabled(
                LoggingEnabled::builder()
                    .target_bucket(target.trim())
                    .target_prefix(prefix.trim())
                    .build()
                    .map_err(|e| e.to_string())?,
            );
        }
        self.client
            .put_bucket_logging()
            .bucket(bucket)
            .bucket_logging_status(status.build())
            .send()
            .await
            .map_err(describe)?;
        Ok(())
    }

    pub async fn set_requester_pays(&self, bucket: &str, requester: bool) -> Res<()> {
        let payer = if requester { Payer::Requester } else { Payer::BucketOwner };
        self.client
            .put_bucket_request_payment()
            .bucket(bucket)
            .request_payment_configuration(
                RequestPaymentConfiguration::builder().payer(payer).build().map_err(|e| e.to_string())?,
            )
            .send()
            .await
            .map_err(describe)?;
        Ok(())
    }

    pub async fn set_public_block(&self, bucket: &str, flags: [bool; 4]) -> Res<()> {
        let config = PublicAccessBlockConfiguration::builder()
            .block_public_acls(flags[0])
            .ignore_public_acls(flags[1])
            .block_public_policy(flags[2])
            .restrict_public_buckets(flags[3])
            .build();
        self.client
            .put_public_access_block()
            .bucket(bucket)
            .public_access_block_configuration(config)
            .send()
            .await
            .map_err(describe)?;
        Ok(())
    }

    fn cloudfront(&self) -> aws_sdk_cloudfront::Client {
        let mut builder = aws_sdk_cloudfront::Config::builder()
            .behavior_version(aws_sdk_cloudfront::config::BehaviorVersion::latest())
            .region(aws_sdk_cloudfront::config::Region::new("us-east-1"));
        if !self.profile.access_key.is_empty() {
            let token = (!self.profile.session_token.is_empty()).then(|| self.profile.session_token.clone());
            builder = builder.credentials_provider(aws_sdk_cloudfront::config::Credentials::new(
                self.profile.access_key.clone(),
                self.profile.secret_key.clone(),
                token,
                None,
                "profile",
            ));
        }
        aws_sdk_cloudfront::Client::from_conf(builder.build())
    }

    pub async fn distributions(&self, bucket: &str) -> Res<Vec<Distribution>> {
        if self.profile.provider != "aws" {
            return Err(tr("CloudFront is only available for Amazon S3 connections"));
        }
        let client = self.cloudfront();
        let mut result = Vec::new();
        let mut marker: Option<String> = None;
        loop {
            let out = client
                .list_distributions()
                .set_marker(marker.clone())
                .send()
                .await
                .map_err(|e| format!("{}", aws_sdk_cloudfront::error::DisplayErrorContext(e)))?;
            let Some(list) = out.distribution_list() else { break };
            for d in list.items() {
                let origins: Vec<String> = d
                    .origins()
                    .map(|o| o.items().iter().map(|i| i.domain_name().to_string()).collect())
                    .unwrap_or_default();
                let Some(origin) = origins
                    .into_iter()
                    .find(|o| o == &format!("{bucket}.s3.amazonaws.com") || o.starts_with(&format!("{bucket}.s3.")))
                else {
                    continue;
                };
                result.push(Distribution {
                    id: d.id().into(),
                    domain: d.domain_name().into(),
                    status: d.status().into(),
                    enabled: d.enabled(),
                    comment: d.comment().into(),
                    aliases: d.aliases().map(|a| a.items().to_vec()).unwrap_or_default(),
                    origin,
                });
            }
            marker = list.next_marker().map(str::to_string);
            if !list.is_truncated() || marker.is_none() {
                break;
            }
        }
        Ok(result)
    }

    pub async fn invalidate(&self, distribution: &str, paths: Vec<String>) -> Res<String> {
        if paths.is_empty() || paths.len() > 100 || paths.iter().any(|p| !p.starts_with('/')) {
            return Err(tr("At least one path is needed (at most 100); paths start with /"));
        }
        let batch = aws_sdk_cloudfront::types::InvalidationBatch::builder()
            .paths(
                aws_sdk_cloudfront::types::Paths::builder()
                    .quantity(paths.len() as i32)
                    .set_items(Some(paths))
                    .build()
                    .map_err(|e| e.to_string())?,
            )
            .caller_reference(format!("ferry-{}", glib_millis()))
            .build()
            .map_err(|e| e.to_string())?;
        let out = self
            .cloudfront()
            .create_invalidation()
            .distribution_id(distribution)
            .invalidation_batch(batch)
            .send()
            .await
            .map_err(|e| format!("{}", aws_sdk_cloudfront::error::DisplayErrorContext(e)))?;
        Ok(out.invalidation().map(|i| i.id().to_string()).unwrap_or_default())
    }
}

fn glib_millis() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}
