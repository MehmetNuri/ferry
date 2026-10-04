//! WebDAV, also Nextcloud and ownCloud (their file space is a WebDAV folder). Folders
//! are listed with PROPFIND; copies and moves happen on the server.
use std::path::Path;

use futures_util::StreamExt;
use quick_xml::events::Event;
use reqwest::{Method, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::Stat;
use crate::i18n::{tr, trf};
use crate::profile::Profile;
use crate::s3::{Progress, Res};

const PROPFIND: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:getcontentlength/><d:getlastmodified/></d:prop></d:propfind>"#;

/// Percent-encodes one path segment.
fn encode(segment: &str) -> String {
    let mut out = String::new();
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@".contains(&byte) { out.push(byte as char) } else { out.push_str(&format!("%{byte:02X}")) }
    }
    out
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16) {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Unix seconds of an HTTP date ("Sun, 06 Nov 1994 08:49:37 GMT").
fn http_date(text: &str) -> i64 {
    let parts: Vec<&str> = text.split_whitespace().collect();
    let [_, day, month, year, time, ..] = parts.as_slice() else { return 0 };
    let month = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"].iter().position(|m| m == month).map(|m| m as i32 + 1);
    let mut clock = time.split(':').filter_map(|p| p.parse::<i32>().ok());
    match (day.parse(), month, year.parse(), clock.next(), clock.next(), clock.next()) {
        (Ok(d), Some(m), Ok(y), Some(h), Some(min), Some(s)) =>
            gtk::glib::DateTime::from_utc(y, m, d, h, min, s as f64).map(|t| t.to_unix()).unwrap_or(0),
        _ => 0,
    }
}

/// The entries of a PROPFIND answer: decoded path, and what is known about it.
fn parse_multistatus(xml: &str) -> Vec<(String, Stat)> {
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut out = Vec::new();
    let (mut href, mut stat, mut field) = (String::new(), Stat::default(), String::new());
    let mut buffer = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(e)) => {
                let name = e.local_name().as_ref().to_lowercase();
                match name.as_str() {
                    "response" => { href.clear(); stat = Stat::default(); }
                    "collection" => stat.is_dir = true,
                    _ => {}
                }
                field = name;
            }
            Ok(Event::Empty(e)) => {
                if e.local_name().as_ref().eq_ignore_ascii_case("collection") { stat.is_dir = true; }
            }
            Ok(event @ (Event::Text(_) | Event::GeneralRef(_))) => {
                // Entities such as &amp; arrive as their own events.
                let text = match event {
                    Event::Text(t) => t.xml10_content().into_owned(),
                    Event::GeneralRef(r) => match r.as_ref() {
                        "amp" => "&".into(), "lt" => "<".into(), "gt" => ">".into(), "quot" => "\"".into(), "apos" => "'".into(),
                        other => other.strip_prefix('#').and_then(|n| n.strip_prefix('x').map(|h| u32::from_str_radix(h, 16).ok()).unwrap_or_else(|| n.parse().ok()))
                            .and_then(char::from_u32).map(String::from).unwrap_or_default(),
                    },
                    _ => String::new(),
                };
                match field.as_str() {
                    "href" => href.push_str(&text),
                    "getcontentlength" => stat.size = text.trim().parse().unwrap_or(0),
                    "getlastmodified" => stat.modified = http_date(text.trim()),
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                if e.local_name().as_ref().eq_ignore_ascii_case("response") {
                    out.push((decode(href.trim()), stat.clone()));
                }
                field.clear();
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buffer.clear();
    }
    out
}

pub struct WebDav {
    client: reqwest::Client,
    /// The address of the connection's folder, ending in "/".
    base: reqwest::Url,
    user: String,
    password: String,
}

impl WebDav {
    pub async fn connect(profile: &Profile) -> Res<WebDav> {
        let mut address = profile.endpoint.trim().to_string();
        if !address.contains("://") { address = format!("https://{address}"); }
        // Nextcloud and ownCloud keep each user's files at a fixed place.
        if profile.provider == "nextcloud" && !address.contains("/remote.php/") {
            address = format!("{}/remote.php/dav/files/{}/", address.trim_end_matches('/'), encode(profile.access_key.trim()));
        }
        if !address.ends_with('/') { address.push('/'); }
        let mut base = reqwest::Url::parse(&address).map_err(|_| tr("Invalid server address"))?;
        for part in profile.remote_path.split('/').filter(|p| !p.is_empty()) {
            base = base.join(&format!("{}/", encode(part))).map_err(|_| tr("Invalid server address"))?;
        }
        let tls = crate::s3::pinned::client_config(&profile.ca_certificate)?;
        let client = reqwest::Client::builder().use_preconfigured_tls(tls).connect_timeout(std::time::Duration::from_secs(20))
            .user_agent(format!("Ferry/{}", crate::config::VERSION)).build().map_err(|e| e.to_string())?;
        let dav = WebDav { client, base, user: profile.access_key.trim().to_string(), password: profile.secret_key.clone() };
        dav.stat("").await?;
        Ok(dav)
    }

    fn url(&self, key: &str) -> reqwest::Url {
        let mut path: Vec<String> = key.split('/').filter(|p| !p.is_empty()).map(encode).collect();
        if key.ends_with('/') && !path.is_empty() { path.push(String::new()); }
        self.base.join(&path.join("/")).unwrap_or_else(|_| self.base.clone())
    }

    fn request(&self, method: Method, key: &str) -> reqwest::RequestBuilder {
        let builder = self.client.request(method, self.url(key));
        if self.user.is_empty() { builder } else { builder.basic_auth(&self.user, Some(&self.password)) }
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Res<reqwest::Response> {
        let response = request.send().await.map_err(|e| {
            if e.is_connect() { tr("The server could not be reached; check the address") }
            else if e.is_timeout() { tr("The server did not answer") }
            else { e.to_string() }
        })?;
        match response.status() {
            s if s.is_success() || s == StatusCode::MULTI_STATUS => Ok(response),
            StatusCode::UNAUTHORIZED => Err(tr("The server did not accept the user name or password")),
            StatusCode::FORBIDDEN => Err(tr("The server does not allow this")),
            StatusCode::NOT_FOUND => Err(tr("The file or folder was not found")),
            StatusCode::INSUFFICIENT_STORAGE => Err(tr("The storage on the server is full")),
            StatusCode::LOCKED => Err(tr("The file is locked on the server")),
            other => Err(trf("The server answered {status}", &[("status", &other.to_string())])),
        }
    }

    async fn propfind(&self, key: &str, depth: &str) -> Res<Vec<(String, Stat)>> {
        let method = Method::from_bytes(b"PROPFIND").expect("valid method");
        let response = self.send(self.request(method, key).header("Depth", depth).header("Content-Type", "application/xml; charset=utf-8").body(PROPFIND)).await?;
        let text = response.text().await.map_err(|e| e.to_string())?;
        Ok(parse_multistatus(&text))
    }

    pub async fn list(&self, dir: &str) -> Res<Vec<(String, Stat)>> {
        let own = decode(self.url(&format!("{}/", dir.trim_end_matches('/'))).path()).trim_end_matches('/').to_string();
        let own = if dir.trim_matches('/').is_empty() { decode(self.base.path()).trim_end_matches('/').to_string() } else { own };
        let mut out = Vec::new();
        for (href, stat) in self.propfind(&format!("{}/", dir.trim_end_matches('/')), "1").await? {
            // Hrefs are paths or full addresses; the folder itself is left out.
            let path = href.split_once("://").map(|(_, r)| r.find('/').map(|i| &r[i..]).unwrap_or("/")).unwrap_or(&href).trim_end_matches('/').to_string();
            if path == own { continue; }
            let Some(name) = path.rsplit('/').next() else { continue };
            out.push((name.to_string(), stat));
        }
        Ok(out)
    }

    pub async fn stat(&self, key: &str) -> Res<Stat> {
        let found = self.propfind(key, "0").await?;
        found.into_iter().next().map(|(_, s)| s).ok_or_else(|| tr("The file or folder was not found"))
    }

    pub async fn read(&self, key: &str, limit: u64) -> Res<Vec<u8>> {
        let response = self.send(self.request(Method::GET, key).header("Range", format!("bytes=0-{}", limit.saturating_sub(1)))).await?;
        let mut stream = response.bytes_stream();
        let mut data = Vec::new();
        // A server that ignores the range must not fill the memory.
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| e.to_string())?;
            let room = (limit as usize).saturating_sub(data.len());
            data.extend_from_slice(&chunk[..chunk.len().min(room)]);
            if data.len() as u64 >= limit { break; }
        }
        Ok(data)
    }

    pub async fn download(&self, key: &str, partial: &Path, progress: &Progress) -> Res<()> {
        let response = self.send(self.request(Method::GET, key)).await?;
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

    pub async fn upload(&self, path: &Path, key: &str, progress: &Progress) -> Res<()> {
        let size = tokio::fs::metadata(path).await.map_err(|e| e.to_string())?.len();
        let file = tokio::fs::File::open(path).await.map_err(|e| e.to_string())?;
        let progress_body = progress.clone();
        // The body streams from the file, with progress and the speed limit.
        let body = futures_util::stream::unfold(file, move |mut file| {
            let progress = progress_body.clone();
            async move {
                let mut buffer = vec![0u8; 128 * 1024];
                match file.read(&mut buffer).await {
                    Ok(0) => None,
                    Ok(n) => {
                        if let Err(e) = progress.advance(n as u64).await { return Some((Err(std::io::Error::other(e)), file)); }
                        buffer.truncate(n);
                        Some((Ok(bytes::Bytes::from(buffer)), file))
                    }
                    Err(e) => Some((Err(e), file)),
                }
            }
        });
        let mut request = self.request(Method::PUT, key).header("Content-Length", size).body(reqwest::Body::wrap_stream(body));
        // Nextcloud and ownCloud keep the file's date when told.
        if let Some(mtime) = std::fs::metadata(path).ok().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()) {
            request = request.header("X-OC-Mtime", mtime.as_secs().to_string());
        }
        let sent = self.send(request).await;
        progress.check()?;
        sent.map(|_| ())
    }

    pub async fn write(&self, key: &str, data: Vec<u8>) -> Res<()> {
        self.send(self.request(Method::PUT, key).body(data)).await.map(|_| ())
    }

    pub async fn mkdir(&self, key: &str) -> Res<()> {
        let method = Method::from_bytes(b"MKCOL").expect("valid method");
        self.send(self.request(method, &format!("{}/", key.trim_end_matches('/')))).await.map(|_| ())
    }

    pub async fn remove(&self, key: &str) -> Res<()> {
        self.send(self.request(Method::DELETE, key)).await.map(|_| ())
    }

    async fn transfer(&self, method: &[u8], from: &str, to: &str) -> Res<()> {
        let method = Method::from_bytes(method).expect("valid method");
        let destination = self.url(to).to_string();
        self.send(self.request(method, from).header("Destination", destination).header("Overwrite", "T")).await.map(|_| ())
    }

    pub async fn rename(&self, from: &str, to: &str) -> Res<()> {
        self.transfer(b"MOVE", from, to).await
    }

    pub async fn copy(&self, from: &str, to: &str) -> Res<()> {
        self.transfer(b"COPY", from, to).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multistatus() {
        let xml = r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:">
<d:response><d:href>/remote.php/dav/files/ada/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>
<d:response><d:href>/remote.php/dav/files/ada/Photos%20%C3%BC/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype><d:getlastmodified>Sun, 06 Nov 1994 08:49:37 GMT</d:getlastmodified></d:prop></d:propstat></d:response>
<D:response xmlns:D="DAV:"><D:href>https://cloud.example.org/remote.php/dav/files/ada/a%26b.txt</D:href><D:propstat><D:prop><D:resourcetype/><D:getcontentlength>42</D:getcontentlength></D:prop></D:propstat></D:response>
</d:multistatus>"#;
        let found = parse_multistatus(xml);
        assert_eq!(found.len(), 3);
        assert_eq!(found[1].0, "/remote.php/dav/files/ada/Photos ü/");
        assert!(found[1].1.is_dir);
        assert_eq!(found[1].1.modified, 784111777);
        assert_eq!(found[2].0, "https://cloud.example.org/remote.php/dav/files/ada/a&b.txt");
        assert_eq!(found[2].1.size, 42);
        assert!(!found[2].1.is_dir);
        assert_eq!(encode("a b/ü"), "a%20b%2F%C3%BC");
    }
}
