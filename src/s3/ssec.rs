use aws_sdk_s3::config::interceptors::{
    BeforeDeserializationInterceptorContextMut, BeforeTransmitInterceptorContextMut,
};
use aws_sdk_s3::config::{ConfigBag, Intercept, RuntimeComponents};
use aws_smithy_runtime_api::box_error::BoxError;
use aws_smithy_runtime_api::client::orchestrator::Metadata;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use md5::Digest;

use crate::i18n::tr;

pub const ENCRYPTION: &str = "sse-c";

tokio::task_local! {
    // set while retrying a read of an object stored without the key
    pub static PLAIN_SOURCE: bool;
}

pub fn parse_key(text: &str) -> Result<[u8; 32], String> {
    let invalid = || tr("The encryption key must be 32 bytes, written in Base64");
    let bytes = STANDARD.decode(text.trim()).map_err(|_| invalid())?;
    bytes.try_into().map_err(|_| invalid())
}

pub fn generate_key() -> Result<String, String> {
    let mut key = [0u8; 32];
    crate::profile::getrandom(&mut key)?;
    Ok(STANDARD.encode(key))
}

#[derive(Debug)]
pub struct CustomerKey {
    key: String,
    md5: String,
}

impl CustomerKey {
    pub fn new(text: &str) -> Result<Self, String> {
        let key = parse_key(text)?;
        Ok(CustomerKey { key: STANDARD.encode(key), md5: STANDARD.encode(md5::Md5::digest(key)) })
    }
}

impl Intercept for CustomerKey {
    fn name(&self) -> &'static str {
        "CustomerKey"
    }

    fn modify_before_signing(
        &self,
        context: &mut BeforeTransmitInterceptorContextMut<'_>,
        _runtime_components: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        let operation = cfg.load::<Metadata>().map(|m| m.name().to_string()).unwrap_or_default();
        let plain_source = PLAIN_SOURCE.try_with(|plain| *plain).unwrap_or(false);
        let (target, source) = match operation.as_str() {
            "GetObject" | "HeadObject" => (!plain_source, false),
            "PutObject" | "CreateMultipartUpload" | "UploadPart" | "CompleteMultipartUpload" => (true, false),
            "CopyObject" | "UploadPartCopy" => (true, !plain_source),
            _ => (false, false),
        };
        let headers = context.request_mut().headers_mut();
        let mut add = |prefix: &str| {
            headers.insert(format!("{prefix}-algorithm"), "AES256");
            headers.insert(format!("{prefix}-key"), self.key.clone());
            headers.insert(format!("{prefix}-key-MD5"), self.md5.clone());
        };
        if target {
            add("x-amz-server-side-encryption-customer");
        }
        if source {
            add("x-amz-copy-source-server-side-encryption-customer");
        }
        Ok(())
    }

    // SeaweedFS repeats the key MD5 header, which the SDK refuses to parse.
    fn modify_before_deserialization(
        &self,
        context: &mut BeforeDeserializationInterceptorContextMut<'_>,
        _runtime_components: &RuntimeComponents,
        _cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        let headers = context.response_mut().headers_mut();
        for name in ["x-amz-server-side-encryption-customer-algorithm", "x-amz-server-side-encryption-customer-key-md5"]
        {
            if headers.get_all(name).count() > 1
                && let Some(first) = headers.get(name).map(str::to_string)
            {
                headers.insert(name.to_string(), first);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        assert!(parse_key(&generate_key().unwrap()).is_ok());
        assert!(parse_key("c2hvcnQ=").is_err());
        assert!(parse_key("not base64!").is_err());
        let zero = CustomerKey::new(&STANDARD.encode([0u8; 32])).unwrap();
        assert_eq!(zero.md5, "cLyPS3KoaSFGi/joRB3OUQ==");
    }
}
