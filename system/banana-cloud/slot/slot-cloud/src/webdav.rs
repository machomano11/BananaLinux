//! Just enough WebDAV for sync: list a folder, fetch, store, make a folder
//! and delete. Works against Nextcloud, ownCloud, Apache mod_dav, rclone
//! serve webdav and most NAS servers.

use std::io::{Read, Write};

use slot_net::ureq;

use crate::{Error, Listed, Remote};

pub struct WebDav {
    /// The folder's URL, always ending in `/`.
    base: String,
    /// The folder's path as the server's hrefs spell it, decoded, ending in `/`.
    base_path: String,
    auth: Option<String>,
}

impl WebDav {
    pub fn new(url: &str, user: Option<&str>, password: Option<&str>) -> WebDav {
        let mut base = url.trim().to_string();
        if !base.ends_with('/') {
            base.push('/');
        }
        let base_path = decode(path_of(&base));
        let auth = user.map(|u| {
            let pair = format!("{u}:{}", password.unwrap_or(""));
            format!("Basic {}", base64(pair.as_bytes()))
        });
        WebDav {
            base,
            base_path,
            auth,
        }
    }

    fn url(&self, rel: &str) -> String {
        let encoded: Vec<String> = rel
            .split('/')
            .filter(|s| !s.is_empty())
            .map(encode)
            .collect();
        format!("{}{}", self.base, encoded.join("/"))
    }

    fn request(&self, method: &str, rel: &str) -> ureq::Request {
        let r = slot_net::transfer_agent().request(method, &self.url(rel));
        match &self.auth {
            Some(a) => r.set("Authorization", a),
            None => r,
        }
    }

    /// The entries of a PROPFIND, or `None` when the path doesn't exist.
    fn propfind(&self, rel: &str, depth: &str) -> Result<Option<Vec<(String, Listed)>>, Error> {
        let body = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:getcontentlength/><d:getetag/><d:getlastmodified/></d:prop></d:propfind>"#;
        let r = self
            .request("PROPFIND", rel)
            .set("Depth", depth)
            .set("Content-Type", "application/xml; charset=utf-8")
            .send_string(body);
        let xml = match r {
            Ok(r) => r
                .into_string()
                .map_err(|e| Error::new(format!("reading {rel}: {e}")))?,
            Err(ureq::Error::Status(404, _)) => return Ok(None),
            Err(e) => return Err(failed("listing", rel, e)),
        };
        Ok(Some(parse_multistatus(&xml, &self.base_path)))
    }
}

impl Remote for WebDav {
    fn list(&mut self, dir: &str) -> Result<Option<Vec<Listed>>, Error> {
        let Some(entries) = self.propfind(dir, "1")? else {
            return Ok(None);
        };
        let me = dir.trim_matches('/');
        Ok(Some(
            entries
                .into_iter()
                .filter(|(rel, _)| rel != me)
                .map(|(_, l)| l)
                .collect(),
        ))
    }

    fn stat(&mut self, rel: &str) -> Result<Option<Listed>, Error> {
        Ok(self
            .propfind(rel, "0")?
            .and_then(|e| e.into_iter().next())
            .map(|(_, l)| l))
    }

    fn get(&mut self, rel: &str, out: &mut dyn Write) -> Result<(), Error> {
        let r = self
            .request("GET", rel)
            .call()
            .map_err(|e| failed("downloading", rel, e))?;
        std::io::copy(&mut r.into_reader(), out)
            .map_err(|e| Error::offline(format!("downloading {rel}: {e}")))?;
        Ok(())
    }

    fn put(&mut self, rel: &str, body: &mut dyn Read, len: u64) -> Result<(), Error> {
        self.request("PUT", rel)
            .set("Content-Type", "application/octet-stream")
            .set("Content-Length", &len.to_string())
            .send(body)
            .map_err(|e| failed("uploading", rel, e))?;
        Ok(())
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<(), Error> {
        self.request("MOVE", from)
            .set("Destination", &self.url(to))
            .set("Overwrite", "T")
            .call()
            .map_err(|e| failed("renaming", from, e))?;
        Ok(())
    }

    fn mkdir(&mut self, rel: &str) -> Result<(), Error> {
        match self.request("MKCOL", rel).call() {
            // 405: it's there already.
            Ok(_) | Err(ureq::Error::Status(405, _)) => Ok(()),
            Err(e) => Err(failed("making folder", rel, e)),
        }
    }

    fn delete(&mut self, rel: &str) -> Result<(), Error> {
        match self.request("DELETE", rel).call() {
            Ok(_) | Err(ureq::Error::Status(404, _)) => Ok(()),
            Err(e) => Err(failed("deleting", rel, e)),
        }
    }
}

fn failed(what: &str, rel: &str, e: ureq::Error) -> Error {
    match e {
        ureq::Error::Status(401 | 403, _) => {
            Error::new("the server refused the username or password")
        }
        ureq::Error::Status(413, _) => Error::new(format!(
            "{what} {rel}: the server refused a file this big (HTTP 413); raise its upload limit"
        )),
        ureq::Error::Status(code, r) => {
            Error::new(format!("{what} {rel}: HTTP {code} {}", r.status_text()))
        }
        ureq::Error::Transport(t) => Error::offline(format!("{what} {rel}: {t}")),
    }
}

/// Every `response` in a 207 multistatus, as (path relative to the folder,
/// what it is). Entries outside the folder are dropped.
pub fn parse_multistatus(xml: &str, base_path: &str) -> Vec<(String, Listed)> {
    let mut out = Vec::new();
    for response in elements(xml, "response") {
        let Some(href) = elements(response, "href").first().map(|h| text(h)) else {
            continue;
        };
        let path = decode(path_of(&href));
        let Some(rel) = path.strip_prefix(base_path).or_else(|| {
            // Some servers drop the trailing slash on the folder itself.
            (path == base_path.trim_end_matches('/')).then_some("")
        }) else {
            continue;
        };
        let rel = rel.trim_matches('/').to_string();
        let is_dir = elements(response, "resourcetype")
            .first()
            .is_some_and(|rt| !elements(rt, "collection").is_empty());
        let prop = |name: &str| {
            elements(response, name)
                .first()
                .map(|v| text(v))
                .unwrap_or_default()
        };
        let size = prop("getcontentlength").parse().unwrap_or(0);
        let modified = parse_http_date(&prop("getlastmodified"));
        let etag = prop("getetag");
        let tag = match etag.is_empty() {
            false => etag,
            true => format!("{size}@{}", modified.unwrap_or(0)),
        };
        let name = rel.rsplit('/').next().unwrap_or("").to_string();
        out.push((
            rel,
            Listed {
                name,
                is_dir,
                size,
                tag,
                modified,
            },
        ));
    }
    out
}

/// The insides of every element named `local`, whatever its namespace prefix.
/// A self-closing element gives "".
fn elements<'a>(xml: &'a str, local: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(open) = xml[at..].find('<').map(|i| at + i) {
        let rest = &xml[open + 1..];
        let name_len = rest
            .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
            .unwrap_or(rest.len());
        let name = &rest[..name_len];
        let Some(close) = rest.find('>').map(|i| open + 1 + i) else {
            break;
        };
        at = close + 1;
        if name.is_empty() || name.starts_with(['/', '?', '!']) {
            continue;
        }
        if name.rsplit(':').next() != Some(local) {
            continue;
        }
        if xml[..close].ends_with('/') {
            out.push("");
            continue;
        }
        let end_tag = format!("</{name}");
        match xml[at..].find(&end_tag) {
            Some(end) => {
                out.push(&xml[at..at + end]);
                at += end;
            }
            None => break,
        }
    }
    out
}

fn text(inner: &str) -> String {
    let mut s = inner.trim();
    if let Some(cdata) = s
        .strip_prefix("<![CDATA[")
        .and_then(|c| c.strip_suffix("]]>"))
    {
        s = cdata;
        return s.to_string();
    }
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The path of an absolute URL, or the string itself if it is a path.
fn path_of(url: &str) -> &str {
    match url.find("://") {
        Some(i) => {
            let after = &url[i + 3..];
            after.find('/').map_or("/", |p| &after[p..])
        }
        None => url,
    }
}

fn encode(segment: &str) -> String {
    let mut out = String::new();
    for b in segment.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ABC[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// `Tue, 14 Oct 2026 10:03:00 GMT` as seconds since 1970.
pub fn parse_http_date(s: &str) -> Option<u64> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    let [_, day, month, year, time, ..] = parts.as_slice() else {
        return None;
    };
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = MONTHS.iter().position(|m| m.eq_ignore_ascii_case(month))? as u32 + 1;
    let day: u32 = day.parse().ok()?;
    let year: i64 = year.parse().ok()?;
    let mut hms = time.split(':').map(|v| v.parse::<u64>().ok());
    let (h, m, sec) = (hms.next()??, hms.next()??, hms.next()??);
    let days = crate::days_from_civil(year, month, day);
    u64::try_from(days)
        .ok()
        .map(|d| d * 86_400 + h * 3600 + m * 60 + sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nextcloud_listing_reads_back() {
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:s="http://sabredav.org/ns">
 <d:response><d:href>/remote.php/dav/files/me/Slot/Saves/</d:href>
  <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype><d:getetag>&quot;abc&quot;</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
 <d:response><d:href>/remote.php/dav/files/me/Slot/Saves/GBA/</d:href>
  <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>
 <d:response><d:href>/remote.php/dav/files/me/Slot/Saves/Pok%C3%A9mon%20Emerald%20(U).sav</d:href>
  <d:propstat><d:prop><d:resourcetype/><d:getcontentlength>131072</d:getcontentlength>
  <d:getetag>&quot;5f1e&quot;</d:getetag><d:getlastmodified>Tue, 14 Oct 2026 10:03:00 GMT</d:getlastmodified></d:prop></d:propstat></d:response>
</d:multistatus>"#;
        let got = parse_multistatus(xml, "/remote.php/dav/files/me/Slot/");
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].0, "Saves");
        assert!(got[0].1.is_dir);
        assert_eq!(got[1].0, "Saves/GBA");
        assert!(got[1].1.is_dir);
        let (rel, f) = &got[2];
        assert_eq!(rel, "Saves/Pokémon Emerald (U).sav");
        assert_eq!(f.name, "Pokémon Emerald (U).sav");
        assert!(!f.is_dir);
        assert_eq!(f.size, 131072);
        assert_eq!(f.tag, "\"5f1e\"");
        assert_eq!(f.modified, Some(1_791_972_180));
    }

    #[test]
    fn absolute_hrefs_and_other_prefixes_work() {
        let xml = r#"<multistatus xmlns="DAV:"><response><href>https://dav.example.com/slot/States/a%2Bb.state</href>
<propstat><prop><resourcetype></resourcetype><getcontentlength>4</getcontentlength><getlastmodified>Thu, 01 Jan 1970 00:01:00 GMT</getlastmodified></prop></propstat></response></multistatus>"#;
        let got = parse_multistatus(xml, "/slot/");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "States/a+b.state");
        assert_eq!(got[0].1.tag, "4@60");
    }

    #[test]
    fn urls_encode_each_segment() {
        let dav = WebDav::new("https://h.example/dav/Slot", Some("me"), Some("pw"));
        assert_eq!(
            dav.url("Saves/GBA/Zelda - Minish Cap (E).sav"),
            "https://h.example/dav/Slot/Saves/GBA/Zelda%20-%20Minish%20Cap%20%28E%29.sav"
        );
        assert_eq!(dav.base_path, "/dav/Slot/");
        assert_eq!(dav.auth.as_deref(), Some("Basic bWU6cHc="));
    }

    #[test]
    fn base64_pads() {
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"abc"), "YWJj");
    }
}
