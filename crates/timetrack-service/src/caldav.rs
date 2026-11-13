/* service/caldav.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! A read-only CalDAV fetch (REQUIREMENTS §19, ADR 0015).
//!
//! The locator names one calendar *collection* URL. Fetching is a
//! `PROPFIND` (Depth: 1) to list the collection's `.ics` resources,
//! then a plain `GET` of each — the most widely supported read shape,
//! chosen over `REPORT calendar-query` whose time-range support
//! varies by server. Nothing here can write: no PUT, no DELETE, no
//! scheduling calls exist in this module by construction.
//!
//! Credentials never rest in the store: a locator may carry a
//! username (`https://user@host/…`) but a password inside the locator
//! is refused at registration; the password is read from the service
//! process environment (`TIMETRACK_CALDAV_PASSWORD`) at fetch time
//! until OS credential-store integration lands. Prefer a per-app
//! password where the provider offers one.

use std::io::Read;
use std::time::Duration;

const MAX_RESOURCES: usize = 500;
const MAX_RESOURCE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_LISTING_BYTES: u64 = 4 * 1024 * 1024;

/// Split a locator into its request URL and optional username,
/// refusing an embedded password.
pub fn split_userinfo(locator: &str) -> Result<(String, Option<String>), String> {
    let Some((scheme, rest)) = locator.split_once("://") else {
        return Ok((locator.to_string(), None));
    };
    let Some(at) = rest.rfind('@') else {
        return Ok((locator.to_string(), None));
    };
    // Only an `@` inside the authority (before the first path slash)
    // is userinfo; one in the path is just a path character.
    if rest.find('/').is_some_and(|slash| slash < at) {
        return Ok((locator.to_string(), None));
    }
    let userinfo = &rest[..at];
    if userinfo.contains(':') {
        return Err(
            "do not put a password in the source URL; set TIMETRACK_CALDAV_PASSWORD \
             in the service environment instead"
                .into(),
        );
    }
    Ok((
        format!("{scheme}://{}", &rest[at + 1..]),
        Some(percent_decode(userinfo)),
    ))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3])
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> shift) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Every `href` in a DAV multistatus body, in document order. Tag
/// prefixes vary by server (`d:href`, `D:href`, plain `href`), so the
/// scan keys on the local name only.
pub fn extract_hrefs(xml: &str) -> Vec<String> {
    let lower = xml.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(found) = lower[at..].find("href>") {
        let start = at + found + "href>".len();
        let Some(end) = lower[start..].find('<') else {
            break;
        };
        let href = xml[start..start + end].trim();
        if !href.is_empty() {
            out.push(href.to_string());
        }
        at = start + end;
    }
    out
}

fn resolve_href(collection_url: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    if let Some(path) = href.strip_prefix('/') {
        if let Some((scheme, rest)) = collection_url.split_once("://") {
            let host = rest.split('/').next().unwrap_or(rest);
            return format!("{scheme}://{host}/{path}");
        }
        return href.to_string();
    }
    let base = if collection_url.ends_with('/') {
        collection_url.to_string()
    } else {
        format!("{collection_url}/")
    };
    format!("{base}{href}")
}

fn read_capped(
    response: &mut ureq::http::Response<ureq::Body>,
    cap: u64,
    what: &str,
) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(cap + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("reading {what} failed: {e}"))?;
    if bytes.len() as u64 > cap {
        return Err(format!("{what} exceeds the {cap}-byte cap"));
    }
    Ok(bytes)
}

/// Fetch every `.ics` resource in the collection, one text each.
/// Failures of individual resources skip that resource; a listing
/// failure is the fetch's error.
pub fn fetch_calendar(locator: &str) -> Result<Vec<String>, String> {
    let (url, user) = split_userinfo(locator)?;
    let password = std::env::var("TIMETRACK_CALDAV_PASSWORD").ok();
    let auth = match (user, password) {
        (Some(u), Some(p)) => Some((u, p)),
        (Some(u), None) => Some((u, String::new())),
        _ => None,
    };
    fetch_calendar_with(&url, auth.as_ref().map(|(u, p)| (u.as_str(), p.as_str())))
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        // PROPFIND is a WebDAV method, outside ureq's standard set.
        .allow_non_standard_methods(true)
        .build()
        .into()
}

fn with_auth(
    req: ureq::RequestBuilder<ureq::typestate::WithoutBody>,
    auth: Option<(&str, &str)>,
) -> ureq::RequestBuilder<ureq::typestate::WithoutBody> {
    match auth {
        Some((u, p)) => req.header(
            "Authorization",
            format!("Basic {}", base64(format!("{u}:{p}").as_bytes())),
        ),
        None => req,
    }
}

fn fetch_calendar_with(
    collection_url: &str,
    auth: Option<(&str, &str)>,
) -> Result<Vec<String>, String> {
    let agent = agent();
    let listing = {
        let mut builder = ureq::http::Request::builder()
            .method("PROPFIND")
            .uri(collection_url)
            .header("Depth", "1")
            .header("Content-Type", "application/xml; charset=utf-8");
        if let Some((u, p)) = auth {
            builder = builder.header(
                "Authorization",
                format!("Basic {}", base64(format!("{u}:{p}").as_bytes())),
            );
        }
        let request = builder
            .body(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
                 <d:propfind xmlns:d=\"DAV:\"><d:prop><d:displayname/></d:prop></d:propfind>"
                    .as_bytes()
                    .to_vec(),
            )
            .map_err(|e| format!("listing {collection_url} failed: {e}"))?;
        let mut response = agent
            .run(request)
            .map_err(|e| format!("listing {collection_url} failed: {e}"))?;
        String::from_utf8_lossy(&read_capped(
            &mut response,
            MAX_LISTING_BYTES,
            "the listing",
        )?)
        .into_owned()
    };
    let mut texts = Vec::new();
    for href in extract_hrefs(&listing)
        .into_iter()
        .filter(|h| h.split('?').next().is_some_and(|p| p.ends_with(".ics")))
        .take(MAX_RESOURCES)
    {
        let url = resolve_href(collection_url, &href);
        match with_auth(agent.get(&url), auth).call() {
            Ok(mut response) => {
                if let Ok(bytes) = read_capped(&mut response, MAX_RESOURCE_BYTES, "a calendar") {
                    texts.push(String::from_utf8_lossy(&bytes).into_owned());
                }
            }
            Err(e) => {
                eprintln!("timetrack-service: caldav GET {url} failed ({e}); skipped");
            }
        }
    }
    if texts.is_empty() {
        // Some servers hand out one .ics for the whole collection at
        // the collection URL itself; a plain GET covers that shape.
        if let Ok(mut response) = with_auth(agent.get(collection_url), auth).call()
            && let Ok(bytes) = read_capped(&mut response, MAX_RESOURCE_BYTES, "the calendar")
        {
            texts.push(String::from_utf8_lossy(&bytes).into_owned());
        }
    }
    if texts.is_empty() {
        return Err(format!("no calendar resources found at {collection_url}"));
    }
    Ok(texts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn hrefs_extract_across_prefix_styles() {
        let xml = "<d:multistatus xmlns:d=\"DAV:\">\
            <d:response><d:href>/cal/</d:href></d:response>\
            <d:response><D:href>/cal/a.ics</D:href></d:response>\
            <response><href>https://x.test/cal/b.ics</href></response></d:multistatus>";
        assert_eq!(
            extract_hrefs(xml),
            ["/cal/", "/cal/a.ics", "https://x.test/cal/b.ics"]
        );
        assert_eq!(
            resolve_href("https://x.test/cal/", "/cal/a.ics"),
            "https://x.test/cal/a.ics"
        );
        assert_eq!(
            resolve_href("https://x.test/cal", "a.ics"),
            "https://x.test/cal/a.ics"
        );
    }

    #[test]
    fn userinfo_rules_refuse_stored_passwords() {
        assert_eq!(
            split_userinfo("https://alice@x.test/cal/").unwrap(),
            ("https://x.test/cal/".to_string(), Some("alice".into()))
        );
        assert!(split_userinfo("https://alice:secret@x.test/cal/").is_err());
        // An @ in the path is not userinfo.
        assert_eq!(
            split_userinfo("https://x.test/cal/a@b.ics").unwrap().1,
            None
        );
        assert_eq!(base64(b"alice:secret"), "YWxpY2U6c2VjcmV0");
    }

    #[test]
    fn propfind_then_get_against_a_local_server() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut saw_auth = false;
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap();
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                saw_auth |= req
                    .to_ascii_lowercase()
                    .contains("authorization: basic ywxpy2u6c2vjcmv0");
                let (status, body) = if req.starts_with("PROPFIND") {
                    (
                        "207 Multi-Status",
                        format!(
                            "<d:multistatus xmlns:d=\"DAV:\"><d:response><d:href>http://{addr}/cal/a.ics</d:href></d:response></d:multistatus>"
                        ),
                    )
                } else {
                    (
                        "200 OK",
                        "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:ev1\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n".to_string(),
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
            saw_auth
        });
        let texts =
            fetch_calendar_with(&format!("http://{addr}/cal/"), Some(("alice", "secret"))).unwrap();
        assert_eq!(texts.len(), 1);
        assert!(texts[0].contains("UID:ev1"));
        assert!(server.join().unwrap(), "basic auth reached the server");
    }
}
