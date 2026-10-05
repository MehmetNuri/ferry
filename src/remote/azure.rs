use std::path::Path;

use base64::Engine;
use futures_util::StreamExt;
use hmac::{Hmac, KeyInit, Mac};
use quick_xml::events::Event;
use reqwest::{Method, StatusCode};
use sha2::Sha256;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::Stat;
use crate::i18n::{tr, trf};
use crate::profile::Profile;
use crate::s3::{Progress, Res};

const VERSION: &str = "2021-08-06";
const SINGLE_PUT: u64 = 64 * 1024 * 1024;
const BLOCK: u64 = 8 * 1024 * 1024;

enum Auth {
    Key(Vec<u8>),
    Sas(String),
}

pub struct Azure {
    client: reqwest::Client,
    account: String,
    base: reqwest::Url,
    auth: Auth,
}

fn encode(segment: &str) -> String {
    let mut out = String::new();
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char)
        } else {
            out.push_str(&format!("%{byte:02X}"))
        }
    }
    out
}

fn http_date(text: &str) -> i64 {
    let parts: Vec<&str> = text.split_whitespace().collect();
    let [_, day, month, year, time, ..] = parts.as_slice() else { return 0 };
    let month = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"]
        .iter()
        .position(|m| m == month)
        .map(|m| m as i32 + 1);
    let mut clock = time.split(':').filter_map(|p| p.parse::<i32>().ok());
    match (day.parse(), month, year.parse(), clock.next(), clock.next(), clock.next()) {
        (Ok(d), Some(m), Ok(y), Some(h), Some(min), Some(s)) => {
            gtk::glib::DateTime::from_utc(y, m, d, h, min, s as f64).map(|t| t.to_unix()).unwrap_or(0)
        }
        _ => 0,
    }
}

fn now_http() -> String {
    gtk::glib::DateTime::now_utc()
        .ok()
        .and_then(|d| d.format("%a, %d %b %Y %H:%M:%S GMT").ok())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

#[derive(Default, Debug)]
struct Page {
    blobs: Vec<(String, Stat)>,
    prefixes: Vec<String>,
    containers: Vec<String>,
    marker: String,
}

fn parse_listing(xml: &str) -> Page {
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut page = Page::default();
    let mut path: Vec<String> = Vec::new();
    let (mut name, mut stat) = (String::new(), Stat::default());
    let mut buffer = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(e)) => {
                let tag = e.local_name().as_ref().to_string();
                if tag == "Blob" || tag == "Container" || tag == "BlobPrefix" {
                    name.clear();
                    stat = Stat::default();
                }
                path.push(tag);
            }
            Ok(Event::Text(t)) => {
                let text = t.xml10_content().into_owned();
                match path.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
                    [.., "Blob", "Name"] | [.., "Container", "Name"] | [.., "BlobPrefix", "Name"] => {
                        name.push_str(&text)
                    }
                    [.., "Properties", "Content-Length"] => stat.size = text.trim().parse().unwrap_or(0),
                    [.., "Properties", "Last-Modified"] => stat.modified = http_date(text.trim()),
                    [.., "NextMarker"] => page.marker.push_str(&text),
                    _ => {}
                }
            }
            Ok(Event::GeneralRef(r)) => {
                let text = match r.as_ref() {
                    "amp" => "&",
                    "lt" => "<",
                    "gt" => ">",
                    "quot" => "\"",
                    "apos" => "'",
                    _ => "",
                };
                if path.last().is_some_and(|t| t == "Name") {
                    name.push_str(text);
                }
            }
            Ok(Event::End(e)) => {
                match e.local_name().as_ref() {
                    "Blob" => page.blobs.push((std::mem::take(&mut name), stat.clone())),
                    "BlobPrefix" => page.prefixes.push(std::mem::take(&mut name)),
                    "Container" => page.containers.push(std::mem::take(&mut name)),
                    _ => {}
                }
                path.pop();
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buffer.clear();
    }
    page
}

impl Azure {
    pub async fn connect(profile: &Profile) -> Res<Azure> {
        let account = profile.access_key.trim().to_string();
        if account.is_empty() {
            return Err(tr("Enter the name of the storage account"));
        }
        let mut address = profile.endpoint.trim().to_string();
        if address.is_empty() {
            address = format!("https://{account}.blob.core.windows.net/");
        } else if !address.contains("://") {
            address = format!("https://{address}");
        }
        if !address.ends_with('/') {
            address.push('/');
        }
        let base = reqwest::Url::parse(&address).map_err(|_| tr("Invalid server address"))?;
        let secret = profile.secret_key.trim();
        let auth = if secret.starts_with("sv=") || secret.starts_with("?sv=") || secret.contains("&sig=") {
            Auth::Sas(secret.trim_start_matches('?').to_string())
        } else {
            Auth::Key(
                base64::engine::general_purpose::STANDARD
                    .decode(secret)
                    .map_err(|_| tr("The account key is not valid; copy it again from the Azure portal"))?,
            )
        };
        let tls = crate::s3::pinned::client_config(&profile.ca_certificate)?;
        let client = reqwest::Client::builder()
            .use_preconfigured_tls(tls)
            .connect_timeout(std::time::Duration::from_secs(20))
            .user_agent(format!("Ferry/{}", crate::config::VERSION))
            .build()
            .map_err(|e| e.to_string())?;
        let azure = Azure { client, account, base, auth };
        azure.containers().await?;
        Ok(azure)
    }

    fn url(&self, container: &str, blob: &str, query: &[(&str, &str)]) -> reqwest::Url {
        let mut path = String::new();
        if !container.is_empty() {
            path.push_str(&encode(container));
            if !blob.is_empty() {
                path.push('/');
                path.push_str(&blob.split('/').map(encode).collect::<Vec<_>>().join("/"));
            }
        }
        let mut url = self.base.join(&path).unwrap_or_else(|_| self.base.clone());
        {
            let mut pairs = url.query_pairs_mut();
            for (k, v) in query {
                pairs.append_pair(k, v);
            }
        }
        if let Auth::Sas(sas) = &self.auth {
            let joined = match url.query() {
                Some(q) if !q.is_empty() => format!("{q}&{sas}"),
                _ => sas.clone(),
            };
            url.set_query(Some(&joined));
        }
        if url.query() == Some("") {
            url.set_query(None);
        }
        url
    }

    fn sign(
        &self,
        method: &Method,
        url: &reqwest::Url,
        headers: &[(String, String)],
        length: Option<u64>,
    ) -> Option<String> {
        let Auth::Key(key) = &self.auth else { return None };
        let header = |name: &str| {
            headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str()).unwrap_or("")
        };
        let mut ms: Vec<(String, String)> = headers
            .iter()
            .filter(|(k, _)| k.to_lowercase().starts_with("x-ms-"))
            .map(|(k, v)| (k.to_lowercase(), v.trim().to_string()))
            .collect();
        ms.sort();
        let canonical_headers: String = ms.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
        let mut resource = format!("/{}{}", self.account, url.path());
        let mut params: Vec<(String, String)> =
            url.query_pairs().map(|(k, v)| (k.to_lowercase(), v.into_owned())).collect();
        params.sort();
        let mut grouped: Vec<(String, Vec<String>)> = Vec::new();
        for (k, v) in params {
            match grouped.last_mut() {
                Some((last, values)) if *last == k => values.push(v),
                _ => grouped.push((k, vec![v])),
            }
        }
        for (k, values) in grouped {
            resource.push_str(&format!("\n{k}:{}", values.join(",")));
        }
        let length = match length {
            Some(0) | None => String::new(),
            Some(n) => n.to_string(),
        };
        let lines = [
            method.as_str(),
            header("Content-Encoding"),
            header("Content-Language"),
            &length,
            header("Content-MD5"),
            header("Content-Type"),
            "",
            header("If-Modified-Since"),
            header("If-Match"),
            header("If-None-Match"),
            header("If-Unmodified-Since"),
            header("Range"),
        ];
        let to_sign = format!("{}\n{canonical_headers}{resource}", lines.join("\n"));
        let mut mac = Hmac::<Sha256>::new_from_slice(key).ok()?;
        mac.update(to_sign.as_bytes());
        let signature = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        Some(format!("SharedKey {}:{signature}", self.account))
    }

    async fn send(
        &self,
        method: Method,
        container: &str,
        blob: &str,
        query: &[(&str, &str)],
        mut headers: Vec<(String, String)>,
        body: Option<reqwest::Body>,
        length: Option<u64>,
    ) -> Res<reqwest::Response> {
        let url = self.url(container, blob, query);
        headers.push(("x-ms-date".into(), now_http()));
        headers.push(("x-ms-version".into(), VERSION.into()));
        let auth = self.sign(&method, &url, &headers, length);
        let mut request = self.client.request(method, url);
        for (k, v) in &headers {
            request = request.header(k, v);
        }
        if let Some(auth) = auth {
            request = request.header("Authorization", auth);
        }
        if let Some(body) = body {
            request = request.body(body);
        }
        let response = request.send().await.map_err(|e| {
            if e.is_connect() {
                tr("The server could not be reached; check the address")
            } else if e.is_timeout() {
                tr("The server did not answer")
            } else {
                e.to_string()
            }
        })?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let code = response.headers().get("x-ms-error-code").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let body = response.text().await.unwrap_or_default();
        let message = body
            .split_once("<Message>")
            .and_then(|(_, rest)| rest.split_once("</Message>"))
            .map(|(m, _)| m.lines().next().unwrap_or("").to_string())
            .unwrap_or_default();
        Err(match (status, code.as_str()) {
            (StatusCode::FORBIDDEN, "AuthenticationFailed" | "AuthorizationFailure") => {
                tr("The account name or key is not right, or the computer's clock is wrong")
            }
            (StatusCode::FORBIDDEN, _) => tr("The server does not allow this"),
            (StatusCode::NOT_FOUND, "ContainerNotFound") => tr("The container was not found"),
            (StatusCode::NOT_FOUND, _) => tr("The file or folder was not found"),
            (StatusCode::CONFLICT, "BlobAlreadyExists") => tr("It already exists"),
            _ => trf("The server answered {status}", &[("status", format!("{status} {code} {message}").trim())]),
        })
    }

    pub async fn containers(&self) -> Res<Vec<String>> {
        let mut names = Vec::new();
        let mut marker = String::new();
        loop {
            let mut query = vec![("comp", "list")];
            if !marker.is_empty() {
                query.push(("marker", marker.as_str()));
            }
            let text = self
                .send(Method::GET, "", "", &query, Vec::new(), None, None)
                .await?
                .text()
                .await
                .map_err(|e| e.to_string())?;
            let page = parse_listing(&text);
            names.extend(page.containers);
            if page.marker.is_empty() {
                return Ok(names);
            }
            marker = page.marker;
        }
    }

    pub async fn create_container(&self, name: &str) -> Res<()> {
        self.send(Method::PUT, name, "", &[("restype", "container")], Vec::new(), Some(Vec::new().into()), Some(0))
            .await
            .map(|_| ())
    }

    pub async fn delete_container(&self, name: &str) -> Res<()> {
        self.send(Method::DELETE, name, "", &[("restype", "container")], Vec::new(), None, None).await.map(|_| ())
    }

    pub async fn list(&self, container: &str, dir: &str) -> Res<Vec<(String, Stat)>> {
        let prefix = if dir.is_empty() || dir.ends_with('/') { dir.to_string() } else { format!("{dir}/") };
        let mut out = Vec::new();
        let mut marker = String::new();
        loop {
            let mut query =
                vec![("restype", "container"), ("comp", "list"), ("delimiter", "/"), ("prefix", prefix.as_str())];
            if !marker.is_empty() {
                query.push(("marker", marker.as_str()));
            }
            let text = self
                .send(Method::GET, container, "", &query, Vec::new(), None, None)
                .await?
                .text()
                .await
                .map_err(|e| e.to_string())?;
            let page = parse_listing(&text);
            for folder in page.prefixes {
                let name = folder.strip_prefix(&prefix).unwrap_or(&folder).trim_end_matches('/').to_string();
                out.push((name, Stat { is_dir: true, ..Default::default() }));
            }
            for (name, stat) in page.blobs {
                if name == prefix {
                    continue;
                }
                out.push((name.strip_prefix(&prefix).unwrap_or(&name).to_string(), stat));
            }
            if page.marker.is_empty() {
                return Ok(out);
            }
            marker = page.marker;
        }
    }

    pub async fn stat(&self, container: &str, key: &str) -> Res<Stat> {
        let key = key.trim_end_matches('/');
        if key.is_empty() {
            return Ok(Stat { is_dir: true, ..Default::default() });
        }
        if let Ok(response) = self.send(Method::HEAD, container, key, &[], Vec::new(), None, None).await {
            let header = |n: &str| response.headers().get(n).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            return Ok(Stat {
                size: header("Content-Length").parse().unwrap_or(0),
                modified: http_date(&header("Last-Modified")),
                is_dir: false,
            });
        }
        let prefix = format!("{key}/");
        let query = [("restype", "container"), ("comp", "list"), ("prefix", prefix.as_str()), ("maxresults", "1")];
        let text = self
            .send(Method::GET, container, "", &query, Vec::new(), None, None)
            .await?
            .text()
            .await
            .map_err(|e| e.to_string())?;
        let page = parse_listing(&text);
        if page.blobs.is_empty() && page.prefixes.is_empty() {
            Err(tr("The file or folder was not found"))
        } else {
            Ok(Stat { is_dir: true, ..Default::default() })
        }
    }

    pub async fn read(&self, container: &str, key: &str, limit: u64) -> Res<Vec<u8>> {
        let range = vec![("x-ms-range".to_string(), format!("bytes=0-{}", limit.saturating_sub(1)))];
        let response = self.send(Method::GET, container, key, &[], range, None, None).await?;
        let mut stream = response.bytes_stream();
        let mut data = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| e.to_string())?;
            let room = (limit as usize).saturating_sub(data.len());
            data.extend_from_slice(&chunk[..chunk.len().min(room)]);
            if data.len() as u64 >= limit {
                break;
            }
        }
        Ok(data)
    }

    pub async fn download(&self, container: &str, key: &str, partial: &Path, progress: &Progress) -> Res<()> {
        let response = self.send(Method::GET, container, key, &[], Vec::new(), None, None).await?;
        let mut stream = response.bytes_stream();
        let mut local = tokio::fs::File::create(partial).await.map_err(|e| e.to_string())?;
        while let Some(chunk) = stream.next().await {
            progress.check()?;
            let chunk = chunk.map_err(|e| e.to_string())?;
            local.write_all(&chunk).await.map_err(|e| e.to_string())?;
            progress.advance(chunk.len() as u64).await?;
        }
        local.flush().await.map_err(|e| e.to_string())
    }

    fn content_type(key: &str) -> (String, String) {
        ("x-ms-blob-content-type".into(), crate::s3::content_type_of(Path::new(key)))
    }

    pub async fn upload(&self, container: &str, path: &Path, key: &str, progress: &Progress) -> Res<()> {
        let size = tokio::fs::metadata(path).await.map_err(|e| e.to_string())?.len();
        let mut file = tokio::fs::File::open(path).await.map_err(|e| e.to_string())?;
        if size <= SINGLE_PUT {
            let mut data = Vec::with_capacity(size as usize);
            file.read_to_end(&mut data).await.map_err(|e| e.to_string())?;
            progress.advance(size).await?;
            let headers = vec![("x-ms-blob-type".to_string(), "BlockBlob".to_string()), Self::content_type(key)];
            self.send(Method::PUT, container, key, &[], headers, Some(data.into()), Some(size)).await?;
            return Ok(());
        }
        let mut ids = Vec::new();
        let mut index = 0u32;
        loop {
            progress.check()?;
            let mut chunk = vec![0u8; BLOCK as usize];
            let mut filled = 0;
            while filled < chunk.len() {
                let n = file.read(&mut chunk[filled..]).await.map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 {
                break;
            }
            chunk.truncate(filled);
            let id = base64::engine::general_purpose::STANDARD.encode(format!("ferry-{index:08}"));
            self.send(
                Method::PUT,
                container,
                key,
                &[("comp", "block"), ("blockid", id.as_str())],
                Vec::new(),
                Some(chunk.into()),
                Some(filled as u64),
            )
            .await?;
            progress.advance(filled as u64).await?;
            ids.push(id);
            index += 1;
        }
        let list = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList>{}</BlockList>",
            ids.iter().map(|i| format!("<Latest>{i}</Latest>")).collect::<String>()
        );
        let length = list.len() as u64;
        self.send(
            Method::PUT,
            container,
            key,
            &[("comp", "blocklist")],
            vec![Self::content_type(key)],
            Some(list.into()),
            Some(length),
        )
        .await?;
        Ok(())
    }

    pub async fn write(&self, container: &str, key: &str, data: Vec<u8>) -> Res<()> {
        let length = data.len() as u64;
        let headers = vec![("x-ms-blob-type".to_string(), "BlockBlob".to_string()), Self::content_type(key)];
        self.send(Method::PUT, container, key, &[], headers, Some(data.into()), Some(length)).await.map(|_| ())
    }

    pub async fn mkdir(&self, container: &str, key: &str) -> Res<()> {
        let headers = vec![
            ("x-ms-blob-type".to_string(), "BlockBlob".to_string()),
            ("x-ms-meta-hdi_isfolder".to_string(), "true".to_string()),
        ];
        self.send(
            Method::PUT,
            container,
            &format!("{}/", key.trim_end_matches('/')),
            &[],
            headers,
            Some(Vec::new().into()),
            Some(0),
        )
        .await
        .map(|_| ())
    }

    pub async fn remove(&self, container: &str, key: &str) -> Res<()> {
        self.send(Method::DELETE, container, key, &[], Vec::new(), None, None).await.map(|_| ())
    }

    pub async fn remove_dir(&self, container: &str, key: &str) -> Res<()> {
        match self.remove(container, &format!("{}/", key.trim_end_matches('/'))).await {
            Err(e) if e == tr("The file or folder was not found") => Ok(()),
            other => other,
        }
    }

    pub async fn copy(&self, container: &str, from: &str, to: &str) -> Res<()> {
        let source = self.url(container, from, &[]).to_string();
        let response = self
            .send(
                Method::PUT,
                container,
                to,
                &[],
                vec![("x-ms-copy-source".into(), source)],
                Some(Vec::new().into()),
                Some(0),
            )
            .await?;
        let mut status =
            response.headers().get("x-ms-copy-status").and_then(|v| v.to_str().ok()).unwrap_or("success").to_string();
        let mut waited = 0;
        while status == "pending" && waited < 600 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            waited += 1;
            let head = self.send(Method::HEAD, container, to, &[], Vec::new(), None, None).await?;
            status =
                head.headers().get("x-ms-copy-status").and_then(|v| v.to_str().ok()).unwrap_or("success").to_string();
        }
        if status == "success" {
            Ok(())
        } else {
            Err(trf("The copy on the server ended as “{status}”", &[("status", &status)]))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?><EnumerationResults><Blobs>
<Blob><Name>docs/a &amp; b.txt</Name><Properties><Last-Modified>Sun, 06 Nov 1994 08:49:37 GMT</Last-Modified><Content-Length>42</Content-Length></Properties></Blob>
<BlobPrefix><Name>docs/sub/</Name></BlobPrefix></Blobs><NextMarker>abc</NextMarker></EnumerationResults>"#;
        let page = parse_listing(xml);
        assert_eq!(page.blobs[0].0, "docs/a & b.txt");
        assert_eq!(page.blobs[0].1.size, 42);
        assert_eq!(page.blobs[0].1.modified, 784111777);
        assert_eq!(page.prefixes, vec!["docs/sub/"]);
        assert_eq!(page.marker, "abc");
        let containers = parse_listing(
            "<EnumerationResults><Containers><Container><Name>photos</Name></Container></Containers><NextMarker/></EnumerationResults>",
        );
        assert_eq!(containers.containers, vec!["photos"]);
    }
}
