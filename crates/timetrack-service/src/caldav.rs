/* service/caldav.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! A read-only CalDAV fetch (REQUIREMENTS §19, ADR 0015).
//!
//! The locator names one calendar *collection* URL. Refreshing is
//! incremental (#151): a `sync-collection` REPORT when the server
//! advertises it, else a `PROPFIND` (Depth: 1) listing each `.ics`
//! resource's ETag with conditional `GET`s — so an unchanged
//! collection costs one cheap probe, not a re-download. The plain
//! list-and-GET shape stays underneath as the full-sync fallback.
//! Nothing here can write: no PUT, no DELETE, no scheduling calls
//! exist in this module by construction.
//!
//! Credentials never rest in the database: a locator may carry a
//! username (`https://user@host/…`) but a password inside the locator
//! is refused at registration; each source's secret lives in the OS
//! credential store instead (see `credentials.rs`), set with
//! `timetrack source-set-credential`. A source with no stored
//! credential still reads `TIMETRACK_CALDAV_PASSWORD` from the service
//! process environment at fetch time — the documented fallback for
//! headless setups — but the moment a credential is stored for a
//! source the fallback stops applying to it. Prefer a per-app
//! password where the provider offers one.

use std::io::Read;
use std::time::Duration;

const MAX_RESOURCES: usize = 500;
const MAX_RESOURCE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_LISTING_BYTES: u64 = 4 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;

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
        return Err("do not put a password in the source URL; store it with \
             `timetrack source-set-credential` instead"
            .into());
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

/// Resolve a redirect `Location` against the URL that produced it:
/// absolute stays, root-relative joins the origin, anything else joins
/// the base's directory (RFC 7231 §7.1.2 allows all three shapes).
fn resolve_against(base_url: &str, location: &str) -> String {
    let location = location.trim();
    if location.contains("://") {
        return location.to_string();
    }
    let Some((scheme, rest)) = base_url.split_once("://") else {
        return location.to_string();
    };
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if let Some(absolute) = location.strip_prefix('/') {
        return format!("{scheme}://{host}/{absolute}");
    }
    let dir = match path.rfind('/') {
        Some(0) | None => "/",
        Some(_) if path.ends_with('/') => path,
        Some(i) => &path[..=i],
    };
    format!("{scheme}://{host}{dir}{location}")
}

/// The `scheme://host[:port]` half of an http(s) URL, or `None` when
/// the endpoint is not one (discovery derives `/.well-known/caldav`
/// from it, so anything else is refused up front).
fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let host = rest.split('/').next().filter(|h| !h.is_empty())?;
    Some(format!("{scheme}://{host}"))
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

/// One discovered calendar/task collection (#150): a URL the user can
/// register as a source without hand-constructing it, with the name
/// and component kinds the server advertises for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaldavCollection {
    /// The collection URL, absolute and redirect-resolved.
    pub url: String,
    /// The server's `displayname`, or the URL's last part when it has none.
    pub display_name: String,
    /// Supported components (`VEVENT`, `VTODO`, ...), uppercased. Both
    /// when the server constrains nothing (RFC 4791 leaves an absent
    /// `supported-calendar-component-set` meaning "any").
    pub components: Vec<String>,
}

/// The inner XML of every element with this local name, in document
/// order. Prefix-blind and case-blind like [`extract_hrefs`], so
/// `d:prop`, `C:prop` and bare `prop` all match; a self-closing
/// element yields an empty string. Sibling same-name elements never
/// nest for the properties discovery reads, so the first close tag
/// with the name ends the block.
fn extract_blocks(xml: &str, local: &str) -> Vec<String> {
    fn tag_local(tag: &str) -> &str {
        match tag.rsplit(':').next() {
            Some(name) => name,
            None => tag,
        }
    }
    let lower = xml.to_ascii_lowercase();
    let want = local.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let Some(rel) = lower[at..].find('<') else {
            break;
        };
        let lt = at + rel;
        // `</`, `<?`, `<!` open no element.
        let opener = bytes.get(lt + 1).copied().unwrap_or(b'>');
        if opener == b'/' || opener == b'?' || opener == b'!' {
            at = lt + 1;
            continue;
        }
        let Some(end_rel) = lower[lt..].find('>') else {
            break;
        };
        let gt = lt + end_rel;
        let tag = lower[lt + 1..gt].trim_end().trim_end_matches('/');
        let name: String = tag
            .split([' ', '\t', '\r', '\n'])
            .next()
            .unwrap_or("")
            .to_string();
        if tag_local(&name) != want {
            at = lt + 1;
            continue;
        }
        if lower[lt..gt].trim_end().ends_with('/') {
            out.push(String::new());
            at = gt + 1;
            continue;
        }
        // The first close tag with this local name ends the block.
        let mut close_lt = None;
        let mut scan = gt + 1;
        while let Some(found) = lower[scan..].find("</") {
            let candidate = scan + found;
            let Some(candidate_end) = lower[candidate..].find('>') else {
                break;
            };
            let close_tag = lower[candidate + 2..candidate + candidate_end]
                .split([' ', '\t', '\r', '\n'])
                .next()
                .unwrap_or("");
            if tag_local(close_tag) == want {
                close_lt = Some((candidate, candidate + candidate_end + 1));
                break;
            }
            scan = candidate + 2;
        }
        match close_lt {
            Some((start, end)) => {
                out.push(xml[gt + 1..start].to_string());
                at = end;
            }
            None => break,
        }
    }
    out
}

/// The trimmed text of the first child element with this local name.
fn first_text(block: &str, local: &str) -> Option<String> {
    extract_blocks(block, local)
        .into_iter()
        .next()
        .map(|text| decode_entities(text.trim()))
}

/// Whether the block contains a child *element* with this local name
/// (a `<prefix:name ...>` or `<prefix:name/>` open tag), so a
/// `resourcetype` holding `<c:calendar/>` reads as a calendar no
/// matter the prefix — while the word "calendar" in prose does not.
fn has_element(block: &str, local: &str) -> bool {
    let lower = block.to_ascii_lowercase();
    let want = local.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        let Some(rel) = lower[at..].find('<') else {
            return false;
        };
        let lt = at + rel;
        let opener = bytes.get(lt + 1).copied().unwrap_or(b'>');
        if opener == b'/' || opener == b'?' || opener == b'!' {
            at = lt + 1;
            continue;
        }
        let Some(end_rel) = lower[lt..].find('>') else {
            return false;
        };
        let tag = lower[lt + 1..lt + end_rel]
            .split([' ', '\t', '\r', '\n', '/'])
            .next()
            .unwrap_or("");
        let name = tag.rsplit(':').next().unwrap_or(tag);
        if name == want {
            return true;
        }
        at = lt + 1;
    }
    false
}

/// The five predefined XML entities; display names and hrefs arrive
/// escaped (`R&amp;D`, query strings), and showing them raw would
/// corrupt both labels and URLs.
fn decode_entities(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

/// The component names a `supported-calendar-component-set` block
/// advertises (`<c:comp name="VEVENT"/>`), uppercased and deduplicated.
fn component_names(block: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut at = 0;
    while let Some(found) = block[at..].find("name=") {
        let mut value = block[at + found + "name=".len()..].trim_start();
        let quote = value.chars().next().unwrap_or(' ');
        if quote != '"' && quote != '\'' {
            at += found + "name=".len();
            continue;
        }
        value = &value[1..];
        if let Some(end) = value.find(quote) {
            let name = value[..end].to_ascii_uppercase();
            if !name.is_empty() && !names.contains(&name) {
                names.push(name);
            }
            at = block.len() - value[end + 1..].len();
        } else {
            break;
        }
    }
    names
}

fn discovery_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .allow_non_standard_methods(true)
        // Redirects are followed by hand below, so a PROPFIND stays a
        // PROPFIND across hops (ureq would only guarantee that for
        // 307/308) and every status stays inspectable for auth errors.
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .into()
}

/// One DAV round trip answered: the final URL after redirects, the
/// status, the response ETag when the server sent one, and the capped
/// body. A 304 carries no body.
struct DavResponse {
    url: String,
    status: u16,
    etag: Option<String>,
    text: String,
}

/// One DAV round trip: a `PROPFIND` or `REPORT` (or `GET` after a
/// 303) following redirects by hand. Read-only by construction — the
/// method is never a write, on any hop.
fn dav_request(
    agent: &ureq::Agent,
    method: &str,
    url: &str,
    depth: Option<&str>,
    body: Option<&[u8]>,
    if_none_match: Option<&str>,
    auth: Option<(&str, &str)>,
) -> Result<DavResponse, String> {
    let mut method = method.to_string();
    let mut url = url.to_string();
    let mut body = body.map(<[u8]>::to_vec);
    let mut depth = depth.map(str::to_string);
    for _ in 0..=MAX_REDIRECTS {
        let mut builder = ureq::http::Request::builder()
            .method(method.as_str())
            .uri(url.as_str())
            .header("Content-Type", "application/xml; charset=utf-8");
        if let Some(deep) = &depth {
            builder = builder.header("Depth", deep.as_str());
        }
        if let Some(etag) = if_none_match {
            builder = builder.header("If-None-Match", etag);
        }
        if let Some((user, password)) = auth {
            builder = builder.header(
                "Authorization",
                format!("Basic {}", base64(format!("{user}:{password}").as_bytes())),
            );
        }
        let request = builder
            .body(body.clone().unwrap_or_default())
            .map_err(|e| format!("requesting {url} failed: {e}"))?;
        let mut response = agent
            .run(request)
            .map_err(|e| format!("requesting {url} failed: {e}"))?;
        let status = response.status().as_u16();
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            let location = response
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            match location {
                Some(next) if !next.trim().is_empty() => {
                    url = resolve_against(&url, &next);
                    if status == 303 {
                        // 303 always becomes a GET; 301/302 keep the
                        // method here (only POST rewrites by history),
                        // as do 307/308 by definition.
                        method = "GET".to_string();
                        body = None;
                        depth = None;
                    }
                    continue;
                }
                _ => {
                    return Err(format!(
                        "requesting {url} failed: the server redirected (HTTP {status}) without saying where"
                    ));
                }
            }
        }
        let etag = response
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(|etag| etag.trim().to_string())
            .filter(|etag| !etag.is_empty());
        if status == 304 {
            return Ok(DavResponse {
                url,
                status,
                etag,
                text: String::new(),
            });
        }
        let text =
            String::from_utf8_lossy(&read_capped(&mut response, MAX_LISTING_BYTES, "the reply")?)
                .into_owned();
        return Ok(DavResponse {
            url,
            status,
            etag,
            text,
        });
    }
    Err(format!(
        "requesting {url} failed: too many redirects (over {MAX_REDIRECTS})"
    ))
}

fn auth_failed(what: &str) -> String {
    format!(
        "{what} needs a login (HTTP 401); give the endpoint a user \
         (https://user@host/…) and set its secret with: \
         timetrack source-set-credential <id> \
         (or TIMETRACK_CALDAV_PASSWORD for headless setups)"
    )
}

fn propfind(
    agent: &ureq::Agent,
    url: &str,
    depth: &str,
    body: &str,
    auth: Option<(&str, &str)>,
) -> Result<(String, String), String> {
    let reply = dav_request(
        agent,
        "PROPFIND",
        url,
        Some(depth),
        Some(body.as_bytes()),
        None,
        auth,
    )?;
    if reply.status == 401 || reply.status == 407 {
        return Err(auth_failed(url));
    }
    if !(200..300).contains(&reply.status) {
        return Err(format!(
            "requesting {url} failed: the server answered HTTP {}",
            reply.status
        ));
    }
    Ok((reply.url, reply.text))
}

const PRINCIPAL_QUERY: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
     <d:propfind xmlns:d=\"DAV:\"><d:prop><d:current-user-principal/></d:prop></d:propfind>";

const HOME_QUERY: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
     <d:propfind xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\">\
     <d:prop><c:calendar-home-set/></d:prop></d:propfind>";

const COLLECTIONS_QUERY: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
     <d:propfind xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\">\
     <d:prop><d:resourcetype/><d:displayname/>\
     <c:supported-calendar-component-set/></d:prop></d:propfind>";

fn principal_of(body: &str) -> Option<String> {
    extract_blocks(body, "current-user-principal")
        .iter()
        .flat_map(|block| extract_hrefs(block))
        .next()
        .map(|href| decode_entities(href.trim()))
        .filter(|href| !href.is_empty())
}

fn home_of(body: &str) -> Option<String> {
    extract_blocks(body, "calendar-home-set")
        .iter()
        .flat_map(|block| extract_hrefs(block))
        .next()
        .map(|href| decode_entities(href.trim()))
        .filter(|href| !href.is_empty())
}

/// Every calendar collection a Depth: 1 home listing names: responses
/// whose `resourcetype` carries a `calendar` element, skipping the
/// home collection itself. In document order.
fn collections_in(home_url: &str, body: &str) -> Vec<CaldavCollection> {
    let home = home_url.trim_end_matches('/');
    let mut found = Vec::new();
    for response in extract_blocks(body, "response") {
        let Some(href) = extract_hrefs(&response)
            .into_iter()
            .next()
            .map(|href| decode_entities(href.trim()))
            .filter(|href| !href.is_empty())
        else {
            continue;
        };
        let url = resolve_href(home_url, &href);
        if url.trim_end_matches('/') == home {
            continue;
        }
        let rtype = extract_blocks(&response, "resourcetype")
            .into_iter()
            .next()
            .unwrap_or_default();
        if !has_element(&rtype, "calendar") {
            continue;
        }
        let display_name = first_text(&response, "displayname")
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| {
                url.trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or(&url)
                    .to_string()
            });
        let supported = extract_blocks(&response, "supported-calendar-component-set")
            .iter()
            .flat_map(|block| component_names(block))
            .collect::<Vec<_>>();
        let components = if supported.is_empty() {
            vec!["VEVENT".to_string(), "VTODO".to_string()]
        } else {
            supported
        };
        found.push(CaldavCollection {
            url,
            display_name,
            components,
        });
    }
    found
}

/// Discover an account's calendar/task collections from a server,
/// account, principal, or home URL (#150): `/.well-known/caldav` and
/// redirects first, then current-user-principal, then
/// calendar-home-set, then a Depth: 1 listing of the home.
///
/// Partial servers are met where they are: a principal answer without
/// a principal, or a home answer without a home-set, falls back to
/// listing the principal or the endpoint itself, and a server that
/// rejects a step answers the next one instead. Every hop is
/// `PROPFIND` or `GET` — discovery never writes — and a direct
/// collection URL still goes through [`fetch_calendar`] unchanged.
pub fn discover_collections(
    endpoint: &str,
    auth: Option<(&str, &str)>,
) -> Result<Vec<CaldavCollection>, String> {
    let (url, _) = split_userinfo(endpoint)?;
    let origin = origin_of(&url).ok_or_else(|| {
        format!("discovering {endpoint} needs an http(s) URL, not whatever that is")
    })?;
    let agent = discovery_agent();
    // The well-known URI first (RFC 6764): providers that publish one
    // redirect it at the account's real path. A server without one
    // 404s here and is discovered from the endpoint itself.
    let well_known = format!("{origin}/.well-known/caldav");
    let mut starts = vec![well_known.as_str(), url.as_str()];
    starts.dedup();
    let mut root: Option<(String, String)> = None;
    let mut last_error = String::new();
    for start in starts {
        match propfind(&agent, start, "0", PRINCIPAL_QUERY, auth) {
            Ok((final_url, body)) => {
                root = Some((final_url, body));
                break;
            }
            Err(e) => last_error = e,
        }
    }
    let (root_url, root_body) = root.ok_or(last_error)?;
    // Candidate homes, richest first: the advertised home-set, then
    // the principal itself, then the endpoint — the first listing
    // that names a collection wins, so each partial shape answers.
    let mut homes: Vec<String> = Vec::new();
    if let Some(principal) = principal_of(&root_body) {
        let principal_url = resolve_href(&root_url, &principal);
        if let Ok((_, home_body)) = propfind(&agent, &principal_url, "0", HOME_QUERY, auth)
            && let Some(home) = home_of(&home_body)
        {
            homes.push(resolve_href(&principal_url, &home));
        }
        homes.push(principal_url);
    }
    if !homes
        .iter()
        .any(|h| h.trim_end_matches('/') == root_url.trim_end_matches('/'))
    {
        homes.push(root_url.clone());
    }
    for home in &homes {
        match propfind(&agent, home, "1", COLLECTIONS_QUERY, auth) {
            Ok((final_url, body)) => {
                let found = collections_in(&final_url, &body);
                if !found.is_empty() {
                    return Ok(found);
                }
            }
            Err(e) if e.contains("HTTP 401") || e.contains("needs a login") => return Err(e),
            Err(_) => continue,
        }
    }
    Err(format!(
        "discovering {endpoint} found no calendar collections \
         (the server answered, but named none)"
    ))
}

/// The sync state a refresh starts from: what the store holds for
/// the source (#151). A source that never synced passes none of it.
#[derive(Debug, Clone, Default)]
pub struct SyncState {
    pub sync_token: Option<String>,
    pub ctag: Option<String>,
}

/// One known resource with the validators its last fetch left
/// behind: the server ETag when it sends one, always a content hash.
#[derive(Debug, Clone)]
pub struct StoredValidator {
    pub href: String,
    pub etag: Option<String>,
    pub content_hash: Option<String>,
}

/// One freshly downloaded resource: the body to parse plus the
/// validators to store once it is applied.
#[derive(Debug, Clone)]
pub struct FetchedResource {
    pub href: String,
    pub etag: Option<String>,
    pub content_hash: String,
    pub body: String,
}

/// What one incremental refresh learned: the changed bodies to
/// reconcile, validator-only updates (same bytes, rotated ETag),
/// remotely deleted hrefs to withdraw, and the sync state to store
/// once all of it is applied.
#[derive(Debug, Clone, Default)]
pub struct IncrementalOutcome {
    pub fetched: Vec<FetchedResource>,
    pub refreshed: Vec<StoredValidator>,
    pub removed_hrefs: Vec<String>,
    pub sync_token: Option<String>,
    pub ctag: Option<String>,
}

/// A stable, process-independent content hash (FNV-1a, hex): the
/// change comparator for servers that send no ETags. The standard
/// hasher is deliberately not used — it is randomly seeded per
/// process, so its values could never be stored across restarts.
fn content_hash(body: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in body.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

const CTAG_QUERY: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
     <d:propfind xmlns:d=\"DAV:\" xmlns:cs=\"http://calendarserver.org/ns/\">\
     <d:prop><cs:getctag/><d:supported-report-set/></d:prop></d:propfind>";

const ETAG_QUERY: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
     <d:propfind xmlns:d=\"DAV:\"><d:prop><d:getetag/></d:prop></d:propfind>";

fn supports_sync_collection(probe_body: &str) -> bool {
    extract_blocks(probe_body, "supported-report-set")
        .iter()
        .any(|block| has_element(block, "sync-collection"))
}

fn collection_ctag(probe_body: &str) -> Option<String> {
    first_text(probe_body, "getctag").filter(|ctag| !ctag.is_empty())
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A `sync-collection` REPORT body (RFC 6578 §3.2): the stored token,
/// or no token element at all for the initial sync.
fn sync_report_body(token: Option<&str>) -> String {
    let token = token
        .map(|t| format!("<d:sync-token>{}</d:sync-token>", xml_escape(t)))
        .unwrap_or_default();
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <d:sync-collection xmlns:d=\"DAV:\">{token}\
         <d:sync-level>1</d:sync-level>\
         <d:prop><d:getetag/></d:prop></d:sync-collection>"
    )
}

/// One listed resource: its resolved URL, its ETag when the server
/// sent one, and whether the server reports it gone (a 404 status in
/// a sync response).
struct ListedResource {
    href: String,
    etag: Option<String>,
    gone: bool,
}

/// Every response in a listing or sync body: hrefs entity-decoded
/// (an escaped `&amp;` would corrupt the URL and the stored
/// identity) and resolved against the answering URL.
fn listed_resources(base_url: &str, body: &str) -> Vec<ListedResource> {
    let mut out = Vec::new();
    for response in extract_blocks(body, "response") {
        let Some(href) = extract_hrefs(&response)
            .into_iter()
            .next()
            .map(|href| decode_entities(href.trim()))
            .filter(|href| !href.is_empty())
        else {
            continue;
        };
        out.push(ListedResource {
            href: resolve_href(base_url, &href),
            etag: first_text(&response, "getetag").filter(|etag| !etag.is_empty()),
            gone: first_text(&response, "status").is_some_and(|s| s.contains("404")),
        });
    }
    out
}

/// Whether the href names a calendar resource: the path part (before
/// any query) ends in `.ics`, exactly as the full fetch filters.
fn is_ics_href(href: &str) -> bool {
    href.split('?').next().is_some_and(|p| p.ends_with(".ics"))
}

struct ReportOk {
    changed: Vec<(String, Option<String>)>,
    removed: Vec<String>,
    token: Option<String>,
}

/// Why a `sync-collection` REPORT did not answer: a login problem
/// (surfaces), a dead token (a full resync replaces it), or anything
/// else (a full resync is still worth one try — the listing may
/// answer where the report did not).
enum SyncError {
    Auth(String),
    TokenDead,
    Other,
}

fn sync_report(
    agent: &ureq::Agent,
    url: &str,
    token: Option<&str>,
    auth: Option<(&str, &str)>,
) -> Result<ReportOk, SyncError> {
    let body = sync_report_body(token);
    let reply = dav_request(
        agent,
        "REPORT",
        url,
        Some("1"),
        Some(body.as_bytes()),
        None,
        auth,
    )
    .map_err(|_| SyncError::Other)?;
    match reply.status {
        401 | 407 => Err(SyncError::Auth(auth_failed(url))),
        // A stored token the server no longer honors (RFC 6578 says
        // 403; gone-token 410s are seen in the wild too).
        403 | 410 if token.is_some() => Err(SyncError::TokenDead),
        207 => {
            let mut changed = Vec::new();
            let mut removed = Vec::new();
            for resource in listed_resources(&reply.url, &reply.text) {
                if resource.gone {
                    removed.push(resource.href);
                } else {
                    changed.push((resource.href, resource.etag));
                }
            }
            Ok(ReportOk {
                changed,
                removed,
                token: first_text(&reply.text, "sync-token").filter(|t| !t.is_empty()),
            })
        }
        _ => Err(SyncError::Other),
    }
}

enum GetOutcome {
    NotModified,
    Fetched { etag: Option<String>, body: String },
}

/// One conditional `GET`: `If-None-Match` carries the stored ETag, a
/// 304 answers "unchanged" with no body, and anything outside 2xx
/// (other than auth, which names the fix) is the resource's skip.
fn conditional_get(
    agent: &ureq::Agent,
    url: &str,
    if_none_match: Option<&str>,
    auth: Option<(&str, &str)>,
) -> Result<GetOutcome, String> {
    let reply = dav_request(agent, "GET", url, None, None, if_none_match, auth)?;
    match reply.status {
        304 => Ok(GetOutcome::NotModified),
        200..=299 => {
            if reply.text.len() as u64 > MAX_RESOURCE_BYTES {
                return Err(format!(
                    "a calendar exceeds the {MAX_RESOURCE_BYTES}-byte cap"
                ));
            }
            Ok(GetOutcome::Fetched {
                etag: reply.etag,
                body: reply.text,
            })
        }
        401 | 407 => Err(auth_failed(url)),
        _ => Err(format!(
            "requesting {url} failed: the server answered HTTP {}",
            reply.status
        )),
    }
}

#[derive(Default)]
struct FetchWork {
    fetched: Vec<FetchedResource>,
    refreshed: Vec<StoredValidator>,
    unchanged: usize,
}

/// Download every candidate the validators cannot prove unchanged:
/// an ETag match skips without a request, a 304 skips without a
/// body, and a body whose hash matches skips the reconcile (only a
/// rotated ETag is recorded). A failed resource is skipped with a
/// warning, as the full fetch has always done.
fn fetch_candidates(
    agent: &ureq::Agent,
    candidates: &[(String, Option<String>)],
    known: &[StoredValidator],
    auth: Option<(&str, &str)>,
) -> FetchWork {
    let mut work = FetchWork::default();
    for (href, listed_etag) in candidates.iter().take(MAX_RESOURCES) {
        let prior = known.iter().find(|k| &k.href == href);
        if let (Some(prior), Some(etag)) = (prior, listed_etag)
            && prior.etag.as_deref() == Some(etag.as_str())
        {
            work.unchanged += 1;
            continue;
        }
        match conditional_get(agent, href, prior.and_then(|p| p.etag.as_deref()), auth) {
            Ok(GetOutcome::NotModified) => {
                work.unchanged += 1;
            }
            Ok(GetOutcome::Fetched { etag, body }) => {
                let hash = content_hash(&body);
                let etag = etag.or_else(|| listed_etag.clone());
                let same = prior.and_then(|p| p.content_hash.as_deref()) == Some(hash.as_str());
                if same {
                    if etag != prior.and_then(|p| p.etag.clone()) {
                        work.refreshed.push(StoredValidator {
                            href: href.clone(),
                            etag,
                            content_hash: Some(hash),
                        });
                    }
                    work.unchanged += 1;
                } else {
                    work.fetched.push(FetchedResource {
                        href: href.clone(),
                        etag,
                        content_hash: hash,
                        body,
                    });
                }
            }
            Err(e) => {
                eprintln!("timetrack-service: caldav GET {href} failed ({e}); skipped");
            }
        }
    }
    work
}

/// Refresh one collection incrementally (#151): a stored sync token
/// REPORTs first; otherwise a Depth: 0 probe reads the CTag (a match
/// ends the refresh — the probe is the whole cost) and the report
/// set, an advertised `sync-collection` syncs, and anything else
/// lists with ETags and downloads only what changed. A dead token or
/// a refusing REPORT falls back to exactly one full PROPFIND sync —
/// bounded, never a loop — and every failure mode a login explains
/// still errors actionably instead of falling back past it.
///
/// `known` carries the stored validators; hrefs in the outcome are
/// resolved absolute URLs, the identity the store reconciles on.
pub fn fetch_calendar_incremental(
    locator: &str,
    auth: Option<(&str, &str)>,
    state: &SyncState,
    known: &[StoredValidator],
) -> Result<IncrementalOutcome, String> {
    let (url, _) = split_userinfo(locator)?;
    let agent = discovery_agent();
    let mut token = state.sync_token.clone();
    if let Some(cached) = token.clone() {
        match sync_report(&agent, &url, Some(&cached), auth) {
            Ok(ok) => return finish_sync_changes(&agent, ok, known, auth, &url),
            Err(SyncError::Auth(e)) => return Err(e),
            Err(SyncError::TokenDead) => {
                token = None;
            }
            Err(SyncError::Other) => {}
        }
    }
    full_sync(&agent, &url, token, state.ctag.as_deref(), known, auth)
}

/// Apply a sync response: download the changed `.ics` resources (the
/// report carries ETags, not bodies) and keep the removals the store
/// can attribute — an href never fetched has nothing to withdraw.
fn finish_sync_changes(
    agent: &ureq::Agent,
    ok: ReportOk,
    known: &[StoredValidator],
    auth: Option<(&str, &str)>,
    url: &str,
) -> Result<IncrementalOutcome, String> {
    let candidates: Vec<(String, Option<String>)> = ok
        .changed
        .into_iter()
        .filter(|(href, _)| is_ics_href(href))
        .collect();
    let work = fetch_candidates(agent, &candidates, known, auth);
    if !candidates.is_empty() && work.fetched.is_empty() && work.unchanged == 0 {
        return Err(format!("no calendar resources found at {url}"));
    }
    Ok(IncrementalOutcome {
        fetched: work.fetched,
        refreshed: work.refreshed,
        removed_hrefs: ok
            .removed
            .into_iter()
            .filter(|href| known.iter().any(|k| &k.href == href))
            .collect(),
        sync_token: ok.token,
        ctag: None,
    })
}

/// The full sync: probe, then an opportunistic initial REPORT, then
/// the ETag listing. `token` is the still-possibly-live stored token
/// (already cleared when the REPORT above proved it dead); the
/// outcome only keeps it when the CTag proves nothing changed.
fn full_sync(
    agent: &ureq::Agent,
    url: &str,
    token: Option<String>,
    cached_ctag: Option<&str>,
    known: &[StoredValidator],
    auth: Option<(&str, &str)>,
) -> Result<IncrementalOutcome, String> {
    let (final_url, probe) = propfind(agent, url, "0", CTAG_QUERY, auth)?;
    let ctag = collection_ctag(&probe);
    if let (Some(cached), Some(current)) = (cached_ctag, ctag.as_deref())
        && cached == current
    {
        // The collection is byte-identical to the last sync: the
        // probe was the whole refresh — no listing, no downloads.
        return Ok(IncrementalOutcome {
            sync_token: token,
            ctag,
            ..IncrementalOutcome::default()
        });
    }
    if supports_sync_collection(&probe) {
        match sync_report(agent, &final_url, None, auth) {
            Ok(ok) => return finish_sync_changes(agent, ok, known, auth, url),
            Err(SyncError::Auth(e)) => return Err(e),
            Err(SyncError::TokenDead) | Err(SyncError::Other) => {}
        }
    }
    let (list_url, listing) = propfind(agent, &final_url, "1", ETAG_QUERY, auth)?;
    let listed: Vec<(String, Option<String>)> = listed_resources(&list_url, &listing)
        .into_iter()
        .filter(|r| is_ics_href(&r.href))
        .map(|r| (r.href, r.etag))
        .collect();
    if listed.is_empty() {
        return finish_empty_listing(agent, url, &list_url, known, auth, ctag);
    }
    let work = fetch_candidates(agent, &listed, known, auth);
    if work.fetched.is_empty() && work.unchanged == 0 {
        // Every download failed (each one logged): the old hard error
        // rather than a silent success.
        return Err(format!("no calendar resources found at {url}"));
    }
    let current: Vec<&str> = listed.iter().map(|(href, _)| href.as_str()).collect();
    Ok(IncrementalOutcome {
        fetched: work.fetched,
        refreshed: work.refreshed,
        removed_hrefs: known
            .iter()
            .filter(|k| !current.contains(&k.href.as_str()))
            .map(|k| k.href.clone())
            .collect(),
        sync_token: None,
        ctag,
    })
}

/// A listing with no `.ics` resources: an emptied collection (every
/// known resource counts as removed) — unless the source never
/// synced, or only ever saw the collection URL itself, in which case
/// the collection may be one `.ics` served at its own URL, the legacy
/// shape the full fetch covers with a plain GET.
fn finish_empty_listing(
    agent: &ureq::Agent,
    url: &str,
    list_url: &str,
    known: &[StoredValidator],
    auth: Option<(&str, &str)>,
    ctag: Option<String>,
) -> Result<IncrementalOutcome, String> {
    let single_ics_mode = known.is_empty() || (known.len() == 1 && known[0].href == list_url);
    let mut outcome = IncrementalOutcome {
        sync_token: None,
        ctag,
        ..IncrementalOutcome::default()
    };
    if !single_ics_mode {
        outcome.removed_hrefs = known.iter().map(|k| k.href.clone()).collect();
        return Ok(outcome);
    }
    let prior = known.first();
    match conditional_get(agent, list_url, prior.and_then(|p| p.etag.as_deref()), auth) {
        Ok(GetOutcome::NotModified) => Ok(outcome),
        Ok(GetOutcome::Fetched { etag, body }) => {
            let hash = content_hash(&body);
            let same = prior.and_then(|p| p.content_hash.as_deref()) == Some(hash.as_str());
            if same {
                if etag != prior.and_then(|p| p.etag.clone()) {
                    outcome.refreshed.push(StoredValidator {
                        href: list_url.to_string(),
                        etag,
                        content_hash: Some(hash),
                    });
                }
            } else {
                outcome.fetched.push(FetchedResource {
                    href: list_url.to_string(),
                    etag,
                    content_hash: hash,
                    body,
                });
            }
            Ok(outcome)
        }
        Err(e) => {
            if known.is_empty() {
                return Err(format!("no calendar resources found at {url}"));
            }
            // The single `.ics` is gone for now (the next refresh
            // retries the GET from a clean slate); withdraw it like
            // any other deletion.
            eprintln!("timetrack-service: caldav GET {list_url} failed ({e}); skipped");
            outcome.removed_hrefs = known.iter().map(|k| k.href.clone()).collect();
            Ok(outcome)
        }
    }
}

/// Fetch every `.ics` resource in the collection, one text each.
/// Failures of individual resources skip that resource; a listing
/// failure is the fetch's error.
///
/// This is the documented environment fallback: the password comes
/// from `TIMETRACK_CALDAV_PASSWORD`, for sources with no stored
/// credential. Sources *with* one go through
/// [`fetch_calendar_with_auth`] instead, which never consults the
/// environment.
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

/// Fetch with an explicit Basic-auth pair — typically the per-source
/// credential resolved from the OS store. The locator's userinfo still
/// supplies the username when `auth` carries none, and a password in
/// the locator is refused exactly as in [`fetch_calendar`].
pub fn fetch_calendar_with_auth(
    locator: &str,
    auth: Option<(&str, &str)>,
) -> Result<Vec<String>, String> {
    let (url, user) = split_userinfo(locator)?;
    // The fallback pair is owned here so the borrow below outlives the
    // match; the passed-in pair borrows from the caller either way.
    let fallback: Option<(String, String)>;
    let auth = match auth {
        Some(pair) => Some(pair),
        None => {
            fallback = user.map(|u| (u, String::new()));
            fallback.as_ref().map(|(u, p)| (u.as_str(), p.as_str()))
        }
    };
    fetch_calendar_with(&url, auth)
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

/// A scripted CalDAV fake for discovery tests (#150), shared by every
/// service test module: each connection is answered by `route`, and
/// the join handle yields the request log (method, path, headers, and
/// body per request, in order) for auth, read-only, and sync-token
/// assertions. The socket handling repeats the fenced test's own
/// pattern (drain the body, graceful close), which is what keeps
/// Windows Winsock from RST-ing the client's read.
#[cfg(test)]
pub(crate) mod fake {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    /// One scripted answer: the status line, extra headers, and the body.
    pub struct Answer {
        pub status: String,
        pub headers: Vec<(String, String)>,
        pub body: String,
    }

    impl Answer {
        pub fn status_body(status: &str, body: &str) -> Self {
            Answer {
                status: status.to_string(),
                headers: Vec::new(),
                body: body.to_string(),
            }
        }

        pub fn redirect(location: &str) -> Self {
            Answer {
                status: "301 Moved Permanently".to_string(),
                headers: vec![("Location".to_string(), location.to_string())],
                body: String::new(),
            }
        }
    }

    pub fn multistatus(inner: &str) -> String {
        format!(
            "<d:multistatus xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\">{inner}</d:multistatus>"
        )
    }

    /// Serve `max_requests` connections, then stop. Panics inside
    /// `route` surface through the join handle, failing the test.
    pub fn spawn(
        max_requests: usize,
        route: impl Fn(&str, &str, &str) -> Answer + Send + 'static,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let mut log = Vec::new();
            // Ten idle seconds end the script: a test that makes fewer
            // requests than `max_requests` still joins, failing on the
            // log instead of hanging on the accept.
            let mut idle_since = std::time::Instant::now();
            while log.len() < max_requests && idle_since.elapsed() < Duration::from_secs(10) {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                };
                idle_since = std::time::Instant::now();
                // Accepted streams inherit the listener's nonblocking
                // mode; back to blocking so the reads below wait.
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                let mut buf = [0u8; 1024];
                let mut raw = Vec::new();
                loop {
                    let n = stream.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..n]);
                    if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                    if raw.len() > 65536 {
                        break;
                    }
                }
                let header_end = raw
                    .windows(4)
                    .position(|w| w == b"\r\n\r\n")
                    .map(|pos| pos + 4)
                    .unwrap_or(raw.len());
                let header_text = String::from_utf8_lossy(&raw[..header_end]).into_owned();
                let content_len: usize = header_text
                    .lines()
                    .skip(1)
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, value)| value.trim().parse().ok())
                    .unwrap_or(0);
                let mut body_bytes: Vec<u8> = raw.get(header_end..).unwrap_or_default().to_vec();
                let mut body_read = body_bytes.len();
                while body_read < content_len {
                    let n = stream.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    body_bytes.extend_from_slice(&buf[..n]);
                    body_read += n;
                }
                let mut parts = header_text.lines().next().unwrap_or("").split(' ');
                let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                if method != "PROPFIND" && method != "GET" && method != "REPORT" {
                    // Not ours: localhost scanners and sandboxes probe
                    // new listeners (commonly `HEAD /`). Answered and
                    // forgotten — never logged, never routed — so the
                    // log holds exactly the client's reads.
                    let answer = "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    stream.write_all(answer.as_bytes()).unwrap();
                    stream.shutdown(std::net::Shutdown::Write).ok();
                    continue;
                }
                log.push(format!(
                    "{method} {path}\n{header_text}\n{}",
                    String::from_utf8_lossy(&body_bytes)
                ));
                let answer = route(method, path, &header_text);
                let mut head = format!(
                    "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                    answer.status,
                    answer.body.len()
                );
                for (name, value) in &answer.headers {
                    head.push_str(&format!("{name}: {value}\r\n"));
                }
                head.push_str("\r\n");
                stream.write_all(head.as_bytes()).unwrap();
                stream.write_all(answer.body.as_bytes()).unwrap();
                stream.shutdown(std::net::Shutdown::Write).ok();
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(_) => continue,
                        Err(_) => break,
                    }
                }
            }
            log
        });
        (base, server)
    }
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
                stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                let mut buf = [0u8; 1024];
                let mut raw = Vec::new();
                // Read the full request headers; a single read may
                // return a partial header block.
                loop {
                    let n = stream.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..n]);
                    if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                    if raw.len() > 65536 {
                        break;
                    }
                }
                let header_end = raw
                    .windows(4)
                    .position(|w| w == b"\r\n\r\n")
                    .map(|pos| pos + 4)
                    .unwrap_or(raw.len());
                let header_text = String::from_utf8_lossy(&raw[..header_end]).into_owned();
                // Drain the request body (PROPFIND carries one) so no
                // unread bytes remain when the socket closes; leftover
                // bytes make Windows Winsock send RST, aborting the
                // client's read.
                let content_len: usize = header_text
                    .lines()
                    .skip(1)
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, value)| value.trim().parse().ok())
                    .unwrap_or(0);
                let mut body_read = raw.len().saturating_sub(header_end);
                while body_read < content_len {
                    let n = stream.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    body_read += n;
                }
                let req = header_text;
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
                // Graceful close: FIN the response, then drain until
                // the client closes so Windows Winsock cannot turn
                // the close into an RST that aborts the client read.
                stream.shutdown(std::net::Shutdown::Write).ok();
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(_) => continue,
                        Err(_) => break,
                    }
                }
            }
            saw_auth
        });
        let texts =
            fetch_calendar_with(&format!("http://{addr}/cal/"), Some(("alice", "secret"))).unwrap();
        assert_eq!(texts.len(), 1);
        assert!(texts[0].contains("UID:ev1"));
        assert!(server.join().unwrap(), "basic auth reached the server");
    }

    #[test]
    fn blocks_parse_across_prefix_styles_and_self_closing() {
        let xml = "<d:multistatus xmlns:d=\"DAV:\">\
            <d:response><d:href>/a/</d:href></d:response>\
            <D:response><D:href>/b/</D:href></D:response>\
            <d:current-user-principal/>\
            <c:supported-calendar-component-set xmlns:c=\"urn:ietf:params:xml:ns:caldav\">\
            <c:comp name=\"VEVENT\"/><c:comp name='VTODO'/></c:supported-calendar-component-set>\
            </d:multistatus>";
        let responses = extract_blocks(xml, "response");
        assert_eq!(responses.len(), 2);
        assert!(responses[0].contains("/a/"));
        assert_eq!(extract_blocks(xml, "current-user-principal"), [""]);
        let supported = extract_blocks(xml, "supported-calendar-component-set");
        assert_eq!(supported.len(), 1);
        assert_eq!(component_names(&supported[0]), ["VEVENT", "VTODO"]);
        assert!(!has_element(&supported[0], "calendar"));
        assert!(has_element(
            "<d:resourcetype xmlns:d=\"DAV:\"><d:collection/><c:calendar xmlns:c=\"urn:ietf:params:xml:ns:caldav\"/></d:resourcetype>",
            "calendar"
        ));
        // The word in prose is not the element.
        assert!(!has_element("a calendar of events", "calendar"));
        assert_eq!(decode_entities("R&amp;D &lt;3"), "R&D <3");
    }

    #[test]
    fn redirects_and_origins_resolve() {
        assert_eq!(
            resolve_against("http://h/.well-known/caldav", "/dav/"),
            "http://h/dav/"
        );
        assert_eq!(
            resolve_against("http://h/.well-known/caldav", "https://x.test/dav/"),
            "https://x.test/dav/"
        );
        assert_eq!(resolve_against("https://h/a/b", "c/"), "https://h/a/c/");
        assert_eq!(origin_of("https://h:8443/a"), Some("https://h:8443".into()));
        assert_eq!(origin_of("file:///a"), None);
        assert_eq!(origin_of("not a url"), None);
    }

    /// Every request the client made was a read: PROPFIND, or GET after
    /// a 303 (the only method change the redirect follower allows).
    fn assert_read_only(log: &[String]) {
        for entry in log {
            let line = entry.lines().next().unwrap_or("");
            assert!(
                line.starts_with("PROPFIND ") || line.starts_with("GET "),
                "discovery must never write, but sent: {line}"
            );
        }
    }

    #[test]
    fn well_known_redirect_principal_home_and_collections() {
        use fake::{Answer, multistatus, spawn};
        // Well-known, redirected DAV root, principal, home: four reads.
        let (base, server) = spawn(4, |method, path, _| {
            if path == "/.well-known/caldav" {
                // The provider publishes RFC 6764 and points at the DAV root.
                return Answer::redirect("/dav/");
            }
            if method == "PROPFIND" && path == "/dav/" {
                return Answer::status_body(
                    "207 Multi-Status",
                    &multistatus(
                        "<d:response><d:href>/dav/</d:href>\
                         <d:propstat><d:prop><d:current-user-principal>\
                         <d:href>/principals/alice/</d:href>\
                         </d:current-user-principal></d:prop>\
                         <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
                    ),
                );
            }
            if method == "PROPFIND" && path == "/principals/alice/" {
                return Answer::status_body(
                    "207 Multi-Status",
                    &multistatus(
                        "<d:response><d:href>/principals/alice/</d:href>\
                         <d:propstat><d:prop><c:calendar-home-set>\
                         <d:href>/calendars/alice/</d:href>\
                         </c:calendar-home-set></d:prop>\
                         <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
                    ),
                );
            }
            if method == "PROPFIND" && path == "/calendars/alice/" {
                return Answer::status_body(
                    "207 Multi-Status",
                    &multistatus(
                        "<d:response><d:href>/calendars/alice/</d:href>\
                         <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype>\
                         </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>\
                         <d:response><d:href>/calendars/alice/work/</d:href>\
                         <d:propstat><d:prop><d:resourcetype><d:collection/><c:calendar/></d:resourcetype>\
                         <d:displayname>Work</d:displayname>\
                         <c:supported-calendar-component-set><c:comp name=\"VEVENT\"/></c:supported-calendar-component-set>\
                         </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>\
                         <d:response><d:href>/calendars/alice/tasks/</d:href>\
                         <d:propstat><d:prop><d:resourcetype><d:collection/><c:calendar/></d:resourcetype>\
                         <d:displayname>Tasks</d:displayname>\
                         <c:supported-calendar-component-set><c:comp name=\"VEVENT\"/><c:comp name=\"VTODO\"/></c:supported-calendar-component-set>\
                         </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>\
                         <d:response><d:href>/calendars/alice/contacts/</d:href>\
                         <d:propstat><d:prop><d:resourcetype><d:collection/><c:addressbook xmlns:c=\"urn:ietf:params:xml:ns:carddav\"/></d:resourcetype>\
                         <d:displayname>Contacts</d:displayname>\
                         </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
                    ),
                );
            }
            Answer::status_body("404 Not Found", "no such path")
        });
        let found = discover_collections(&format!("{base}/"), None).unwrap();
        assert_eq!(
            found,
            [
                CaldavCollection {
                    url: format!("{base}/calendars/alice/work/"),
                    display_name: "Work".into(),
                    components: vec!["VEVENT".into()],
                },
                CaldavCollection {
                    url: format!("{base}/calendars/alice/tasks/"),
                    display_name: "Tasks".into(),
                    components: vec!["VEVENT".into(), "VTODO".into()],
                },
            ]
        );
        let log = server.join().unwrap();
        assert_eq!(log.len(), 4);
        assert!(log[0].starts_with("PROPFIND /.well-known/caldav"));
        assert_read_only(&log);
    }

    #[test]
    fn a_partial_server_lists_from_the_endpoint_itself() {
        use fake::{Answer, multistatus, spawn};
        // Well-known 404, endpoint Depth 0, endpoint Depth 1.
        let (base, server) = spawn(3, |method, path, headers| {
            if path == "/.well-known/caldav" {
                // No RFC 6764 here: discovery continues from the endpoint.
                return Answer::status_body("404 Not Found", "no well-known");
            }
            if method == "PROPFIND" && path == "/calendars/alice/" {
                if headers.contains("Depth: 0") {
                    // A principal answer that names no principal.
                    return Answer::status_body(
                        "207 Multi-Status",
                        &multistatus(
                            "<d:response><d:href>/calendars/alice/</d:href>\
                             <d:propstat><d:prop/>\
                             <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
                        ),
                    );
                }
                return Answer::status_body(
                    "207 Multi-Status",
                    &multistatus(
                        "<d:response><d:href>/calendars/alice/</d:href>\
                         <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype>\
                         </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>\
                         <d:response><d:href>/calendars/alice/plain/</d:href>\
                         <d:propstat><d:prop><d:resourcetype><d:collection/><calendar/></d:resourcetype>\
                         </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
                    ),
                );
            }
            Answer::status_body("404 Not Found", "no such path")
        });
        let found = discover_collections(&format!("{base}/calendars/alice/"), None).unwrap();
        // No displayname: the URL's last part. No component set: both.
        assert_eq!(
            found,
            [CaldavCollection {
                url: format!("{base}/calendars/alice/plain/"),
                display_name: "plain".into(),
                components: vec!["VEVENT".into(), "VTODO".into()],
            }]
        );
        let log = server.join().unwrap();
        assert_eq!(log.len(), 3);
        assert_read_only(&log);
    }

    #[test]
    fn auth_reaches_the_server_and_401_is_actionable() {
        use fake::{Answer, multistatus, spawn};
        const BASIC: &str = "authorization: basic ywxpy2u6c2vjcmv0";
        let route = |method: &str, path: &str, headers: &str| {
            if !headers.to_ascii_lowercase().contains(BASIC) {
                return Answer::status_body("401 Unauthorized", "login first");
            }
            if path == "/.well-known/caldav" {
                return Answer::status_body("404 Not Found", "no well-known");
            }
            if method == "PROPFIND" && path == "/dav/" {
                if headers.contains("Depth: 0") {
                    return Answer::status_body(
                        "207 Multi-Status",
                        &multistatus(
                            "<d:response><d:href>/dav/</d:href>\
                             <d:propstat><d:prop/>\
                             <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
                        ),
                    );
                }
                return Answer::status_body(
                    "207 Multi-Status",
                    &multistatus(
                        "<d:response><d:href>/dav/</d:href>\
                         <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype>\
                         </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>\
                         <d:response><d:href>/dav/cal/</d:href>\
                         <d:propstat><d:prop><d:resourcetype><d:collection/><c:calendar/></d:resourcetype>\
                         <d:displayname>Cal</d:displayname>\
                         </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
                    ),
                );
            }
            Answer::status_body("404 Not Found", "no such path")
        };
        // Two for the failed run, three for the authed one.
        let (base, server) = spawn(5, route);
        let err = discover_collections(&format!("{base}/dav/"), None).unwrap_err();
        assert!(err.contains("401"), "names the status: {err}");
        assert!(
            err.contains("source-set-credential"),
            "says how to fix it: {err}"
        );
        // The failed run made two requests (well-known, endpoint); the
        // authed run below makes three more, all inside the budget.
        let found =
            discover_collections(&format!("{base}/dav/"), Some(("alice", "secret"))).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].display_name, "Cal");
        let log = server.join().unwrap();
        assert_eq!(log.len(), 5);
        assert!(
            log[2..]
                .iter()
                .all(|e| e.to_ascii_lowercase().contains(BASIC)),
            "every authed request carried the credential"
        );
        assert_read_only(&log);
    }

    #[test]
    fn discovery_errors_name_the_problem() {
        use fake::{Answer, multistatus, spawn};
        // A server that fails everything.
        let (base, server) = spawn(2, |_, _, _| Answer::status_body("500 Boom", "broken"));
        let err = discover_collections(&format!("{base}/dav/"), None).unwrap_err();
        assert!(err.contains("HTTP 500"), "carries the status: {err}");
        server.join().unwrap();
        // A server that answers but names no collections.
        let (base, server) = spawn(3, |method, path, _| {
            if path == "/.well-known/caldav" {
                return Answer::status_body("404 Not Found", "no well-known");
            }
            assert_eq!(method, "PROPFIND");
            Answer::status_body(
                "207 Multi-Status",
                &multistatus(
                    "<d:response><d:href>/dav/</d:href>\
                     <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype>\
                     </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
                ),
            )
        });
        let err = discover_collections(&format!("{base}/dav/"), None).unwrap_err();
        assert!(
            err.contains("found no calendar collections"),
            "says what is missing: {err}"
        );
        server.join().unwrap();
        // Not an http(s) endpoint, and a password in the locator.
        assert!(
            discover_collections("file:///cal.ics", None)
                .unwrap_err()
                .contains("http(s)")
        );
        assert!(
            discover_collections("https://alice:secret@x.test/dav/", None)
                .unwrap_err()
                .contains("source-set-credential")
        );
    }

    /// Every request the client made was a read: `PROPFIND`, `GET`, or
    /// the read-only sync `REPORT`.
    fn assert_sync_read_only(log: &[String]) {
        for entry in log {
            let line = entry.lines().next().unwrap_or("");
            assert!(
                line.starts_with("PROPFIND ")
                    || line.starts_with("GET ")
                    || line.starts_with("REPORT "),
                "sync must never write, but sent: {line}"
            );
        }
    }

    /// A Depth: 0 probe answer: the CTag, plus the sync advertisement
    /// when the server speaks `sync-collection`.
    fn probe_body(ctag: &str, sync: bool) -> String {
        let reports = if sync {
            "<d:supported-report-set><d:supported-report><d:report>\
             <d:sync-collection/></d:report></d:supported-report></d:supported-report-set>"
        } else {
            ""
        };
        fake::multistatus(&format!(
            "<d:response><d:href>/cal/</d:href><d:propstat><d:prop>\
             <cs:getctag xmlns:cs=\"http://calendarserver.org/ns/\">\
             {ctag}</cs:getctag>{reports}</d:prop>\
             <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"
        ))
    }

    fn known(href: &str, etag: &str, body: &str) -> StoredValidator {
        StoredValidator {
            href: href.to_string(),
            etag: Some(etag.to_string()),
            content_hash: Some(content_hash(body)),
        }
    }

    #[test]
    fn a_matching_ctag_ends_the_refresh_at_the_probe() {
        use fake::{Answer, spawn};
        // One request only: the Depth: 0 probe. Nothing is listed and
        // nothing is downloaded.
        let (base, server) = spawn(1, |method, path, headers| {
            assert_eq!(method, "PROPFIND");
            assert_eq!(path, "/cal/");
            assert!(
                headers.to_ascii_lowercase().contains("depth: 0"),
                "the probe is Depth: 0"
            );
            Answer::status_body("207 Multi-Status", &probe_body("ctag-7", false))
        });
        let outcome = fetch_calendar_incremental(
            &format!("{base}/cal/"),
            None,
            &SyncState {
                sync_token: None,
                ctag: Some("ctag-7".into()),
            },
            &[known(&format!("{base}/cal/a.ics"), "\"1\"", "BODY-A")],
        )
        .unwrap();
        assert!(outcome.fetched.is_empty());
        assert!(outcome.removed_hrefs.is_empty());
        assert_eq!(outcome.ctag.as_deref(), Some("ctag-7"));
        let log = server.join().unwrap();
        assert_eq!(log.len(), 1);
        assert_sync_read_only(&log);
    }

    #[test]
    fn sync_collection_reports_changes_and_removals() {
        use fake::{Answer, multistatus, spawn};
        use std::sync::{Arc, Mutex};
        // Phase 0 is the initial sync; phase 1 changes b and deletes a.
        let phase = Arc::new(Mutex::new(0));
        let route_phase = phase.clone();
        // Probe, initial REPORT, two GETs, incremental REPORT, one GET.
        let (base, server) = spawn(6, move |method, path, headers| {
            let phase = *route_phase.lock().unwrap();
            if method == "PROPFIND" && path == "/cal/" {
                assert!(
                    headers.to_ascii_lowercase().contains("depth: 0"),
                    "sync never lists"
                );
                return Answer::status_body("207 Multi-Status", &probe_body("ctag-1", true));
            }
            if method == "REPORT" && path == "/cal/" {
                if phase == 0 {
                    return Answer::status_body(
                        "207 Multi-Status",
                        &multistatus(
                            "<d:response><d:href>/cal/a.ics</d:href>\
                             <d:propstat><d:prop><d:getetag>\"a1\"</d:getetag></d:prop>\
                             <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>\
                             <d:response><d:href>/cal/b.ics</d:href>\
                             <d:propstat><d:prop><d:getetag>\"b1\"</d:getetag></d:prop>\
                             <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>\
                             <d:sync-token>tok-1</d:sync-token>",
                        ),
                    );
                }
                return Answer::status_body(
                    "207 Multi-Status",
                    &multistatus(
                        "<d:response><d:href>/cal/b.ics</d:href>\
                         <d:propstat><d:prop><d:getetag>\"b2\"</d:getetag></d:prop>\
                         <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>\
                         <d:response><d:href>/cal/a.ics</d:href>\
                         <d:status>HTTP/1.1 404 Not Found</d:status></d:response>\
                         <d:sync-token>tok-2</d:sync-token>",
                    ),
                );
            }
            if method == "GET" && path == "/cal/a.ics" {
                return Answer {
                    status: "200 OK".to_string(),
                    headers: vec![("ETag".to_string(), "\"a1\"".to_string())],
                    body: "BODY-A".to_string(),
                };
            }
            if method == "GET" && path == "/cal/b.ics" {
                let (etag, body) = if phase == 0 {
                    ("\"b1\"", "BODY-B1")
                } else {
                    ("\"b2\"", "BODY-B2")
                };
                return Answer {
                    status: "200 OK".to_string(),
                    headers: vec![("ETag".to_string(), etag.to_string())],
                    body: body.to_string(),
                };
            }
            Answer::status_body("404 Not Found", "no such path")
        });
        let first =
            fetch_calendar_incremental(&format!("{base}/cal/"), None, &SyncState::default(), &[])
                .unwrap();
        assert_eq!(first.fetched.len(), 2);
        assert!(first.removed_hrefs.is_empty());
        assert_eq!(first.sync_token.as_deref(), Some("tok-1"));
        *phase.lock().unwrap() = 1;
        let known: Vec<StoredValidator> = first
            .fetched
            .iter()
            .map(|f| StoredValidator {
                href: f.href.clone(),
                etag: f.etag.clone(),
                content_hash: Some(f.content_hash.clone()),
            })
            .collect();
        let second = fetch_calendar_incremental(
            &format!("{base}/cal/"),
            None,
            &SyncState {
                sync_token: first.sync_token.clone(),
                ctag: None,
            },
            &known,
        )
        .unwrap();
        assert_eq!(second.fetched.len(), 1);
        assert_eq!(
            second.fetched[0].href,
            format!("{base}/cal/b.ics"),
            "only the changed resource downloads"
        );
        assert_eq!(second.fetched[0].body, "BODY-B2");
        assert_eq!(second.removed_hrefs, [format!("{base}/cal/a.ics")]);
        assert_eq!(second.sync_token.as_deref(), Some("tok-2"));
        let log = server.join().unwrap();
        assert_eq!(log.len(), 6);
        assert!(
            !log[1].contains("sync-token"),
            "the initial REPORT carries no token"
        );
        assert!(
            log[4].contains("tok-1"),
            "the incremental REPORT sends the stored token: {}",
            log[4]
        );
        assert!(
            !log.iter()
                .any(|e| e.starts_with("PROPFIND ") && e.to_ascii_lowercase().contains("depth: 1")),
            "the sync path never lists: {log:?}"
        );
        assert_sync_read_only(&log);
    }

    #[test]
    fn a_dead_sync_token_falls_back_to_one_full_resync() {
        use fake::{Answer, multistatus, spawn};
        // The REPORT 410s on the dead token; exactly one PROPFIND sync
        // follows (probe, listing, GET) and the token is dropped.
        let (base, server) = spawn(4, |method, path, headers| {
            if method == "REPORT" {
                return Answer::status_body("410 Gone", "unknown token");
            }
            if method == "PROPFIND" && path == "/cal/" {
                if headers.to_ascii_lowercase().contains("depth: 0") {
                    return Answer::status_body("207 Multi-Status", &probe_body("ctag-9", false));
                }
                return Answer::status_body(
                    "207 Multi-Status",
                    &multistatus(
                        "<d:response><d:href>/cal/a.ics</d:href>\
                         <d:propstat><d:prop><d:getetag>\"a9\"</d:getetag></d:prop>\
                         <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
                    ),
                );
            }
            if method == "GET" && path == "/cal/a.ics" {
                return Answer {
                    status: "200 OK".to_string(),
                    headers: vec![("ETag".to_string(), "\"a9\"".to_string())],
                    body: "BODY-A9".to_string(),
                };
            }
            Answer::status_body("404 Not Found", "no such path")
        });
        let outcome = fetch_calendar_incremental(
            &format!("{base}/cal/"),
            None,
            &SyncState {
                sync_token: Some("dead".into()),
                ctag: Some("ctag-old".into()),
            },
            &[],
        )
        .unwrap();
        assert_eq!(outcome.fetched.len(), 1);
        assert_eq!(outcome.fetched[0].body, "BODY-A9");
        assert_eq!(outcome.sync_token, None, "the dead token is dropped");
        assert_eq!(outcome.ctag.as_deref(), Some("ctag-9"));
        let log = server.join().unwrap();
        assert_eq!(log.len(), 4);
        assert!(log[0].starts_with("REPORT /cal/"), "the token goes first");
        assert!(log[0].contains("dead"), "carrying the stored token");
        assert!(
            log[1].to_ascii_lowercase().contains("depth: 0"),
            "then one probe"
        );
        assert!(
            log[2].to_ascii_lowercase().contains("depth: 1"),
            "one listing"
        );
        assert!(log[3].starts_with("GET /cal/a.ics"), "one download");
        assert_sync_read_only(&log);
    }

    #[test]
    fn a_304_answers_unchanged_without_a_body() {
        use fake::{Answer, multistatus, spawn};
        // The listing rotated b's ETag, but the conditional GET 304s:
        // nothing downloads, nothing reconciles.
        let (base, server) = spawn(3, |method, path, headers| {
            if method == "PROPFIND" && path == "/cal/" {
                if headers.to_ascii_lowercase().contains("depth: 0") {
                    return Answer::status_body("207 Multi-Status", &probe_body("ctag-2", false));
                }
                return Answer::status_body(
                    "207 Multi-Status",
                    &multistatus(
                        "<d:response><d:href>/cal/a.ics</d:href>\
                         <d:propstat><d:prop><d:getetag>\"a1\"</d:getetag></d:prop>\
                         <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>\
                         <d:response><d:href>/cal/b.ics</d:href>\
                         <d:propstat><d:prop><d:getetag>\"bNEW\"</d:getetag></d:prop>\
                         <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
                    ),
                );
            }
            if method == "GET" && path == "/cal/b.ics" {
                assert!(
                    headers
                        .to_ascii_lowercase()
                        .contains("if-none-match: \"b1\""),
                    "the stored ETag conditions the GET: {headers}"
                );
                return Answer::status_body("304 Not Modified", "");
            }
            Answer::status_body("404 Not Found", "no such path")
        });
        let outcome = fetch_calendar_incremental(
            &format!("{base}/cal/"),
            None,
            &SyncState {
                sync_token: None,
                ctag: Some("ctag-1".into()),
            },
            &[
                known(&format!("{base}/cal/a.ics"), "\"a1\"", "BODY-A"),
                known(&format!("{base}/cal/b.ics"), "\"b1\"", "BODY-B"),
            ],
        )
        .unwrap();
        assert!(outcome.fetched.is_empty());
        assert!(outcome.refreshed.is_empty());
        assert!(outcome.removed_hrefs.is_empty());
        assert_eq!(outcome.ctag.as_deref(), Some("ctag-2"));
        let log = server.join().unwrap();
        assert_eq!(log.len(), 3, "a skips by ETag, b by 304: {log:?}");
        assert_sync_read_only(&log);
    }

    #[test]
    fn an_empty_listing_falls_back_to_the_collection_url_itself() {
        use fake::{Answer, multistatus, spawn};
        // No `.ics` in the listing: the whole calendar is served at
        // the collection URL, the legacy shape the full fetch covers.
        let (base, server) = spawn(3, |method, path, headers| {
            if method == "PROPFIND" && path == "/cal/" {
                if headers.to_ascii_lowercase().contains("depth: 0") {
                    return Answer::status_body("207 Multi-Status", &probe_body("ctag-1", false));
                }
                return Answer::status_body(
                    "207 Multi-Status",
                    &multistatus(
                        "<d:response><d:href>/cal/</d:href>\
                         <d:propstat><d:prop><d:displayname>Cal</d:displayname></d:prop>\
                         <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
                    ),
                );
            }
            if method == "GET" && path == "/cal/" {
                return Answer {
                    status: "200 OK".to_string(),
                    headers: vec![("ETag".to_string(), "\"whole\"".to_string())],
                    body: "WHOLE-CALENDAR".to_string(),
                };
            }
            Answer::status_body("404 Not Found", "no such path")
        });
        let outcome =
            fetch_calendar_incremental(&format!("{base}/cal/"), None, &SyncState::default(), &[])
                .unwrap();
        assert_eq!(outcome.fetched.len(), 1);
        assert_eq!(outcome.fetched[0].href, format!("{base}/cal/"));
        assert_eq!(outcome.fetched[0].body, "WHOLE-CALENDAR");
        assert!(outcome.removed_hrefs.is_empty());
        let log = server.join().unwrap();
        assert_eq!(log.len(), 3);
        assert_sync_read_only(&log);
    }

    #[test]
    fn an_emptied_collection_reports_every_known_resource_removed() {
        use fake::{Answer, multistatus, spawn};
        // Known resources, empty listing, and the known set is real
        // (not the single-`.ics` shape): everything counts as removed
        // and nothing is downloaded — no fallback GET either.
        let (base, server) = spawn(2, |method, path, headers| {
            assert_ne!(method, "GET", "an emptied collection downloads nothing");
            assert_eq!(method, "PROPFIND");
            assert_eq!(path, "/cal/");
            if headers.to_ascii_lowercase().contains("depth: 0") {
                return Answer::status_body("207 Multi-Status", &probe_body("ctag-3", false));
            }
            Answer::status_body(
                "207 Multi-Status",
                &multistatus(
                    "<d:response><d:href>/cal/</d:href>\
                     <d:propstat><d:prop><d:displayname>Cal</d:displayname></d:prop>\
                     <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
                ),
            )
        });
        let outcome = fetch_calendar_incremental(
            &format!("{base}/cal/"),
            None,
            &SyncState {
                sync_token: None,
                ctag: Some("ctag-2".into()),
            },
            &[
                known(&format!("{base}/cal/a.ics"), "\"a1\"", "BODY-A"),
                known(&format!("{base}/cal/b.ics"), "\"b1\"", "BODY-B"),
            ],
        )
        .unwrap();
        assert!(outcome.fetched.is_empty());
        assert_eq!(
            outcome.removed_hrefs,
            [format!("{base}/cal/a.ics"), format!("{base}/cal/b.ics")]
        );
        let log = server.join().unwrap();
        assert_eq!(log.len(), 2);
        assert_sync_read_only(&log);
    }

    #[test]
    fn incremental_failures_stay_actionable() {
        use fake::{Answer, multistatus, spawn};
        // A login problem surfaces instead of falling back past it.
        let (base, server) = spawn(1, |_, _, _| {
            Answer::status_body("401 Unauthorized", "login first")
        });
        let err =
            fetch_calendar_incremental(&format!("{base}/cal/"), None, &SyncState::default(), &[])
                .unwrap_err();
        assert!(err.contains("401"), "names the status: {err}");
        assert!(
            err.contains("source-set-credential"),
            "says how to fix it: {err}"
        );
        server.join().unwrap();
        // A listing whose every download fails is the old hard error,
        // not a silent success.
        let (base, server) = spawn(3, |method, path, headers| {
            if method == "PROPFIND" && path == "/cal/" {
                if headers.to_ascii_lowercase().contains("depth: 0") {
                    return Answer::status_body("207 Multi-Status", &probe_body("ctag-1", false));
                }
                return Answer::status_body(
                    "207 Multi-Status",
                    &multistatus(
                        "<d:response><d:href>/cal/a.ics</d:href>\
                         <d:propstat><d:prop><d:getetag>\"a1\"</d:getetag></d:prop>\
                         <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
                    ),
                );
            }
            Answer::status_body("500 Boom", "broken")
        });
        let err =
            fetch_calendar_incremental(&format!("{base}/cal/"), None, &SyncState::default(), &[])
                .unwrap_err();
        assert!(
            err.contains("no calendar resources found"),
            "the hard error survives: {err}"
        );
        server.join().unwrap();
    }

    #[test]
    fn sync_bodies_carry_or_omit_the_token() {
        assert!(
            !sync_report_body(None).contains("sync-token"),
            "the initial sync omits the token element"
        );
        let body = sync_report_body(Some("tok&<en>"));
        assert!(
            body.contains("<d:sync-token>tok&amp;&lt;en&gt;</d:sync-token>"),
            "the token is escaped: {body}"
        );
        // The content hash is stable and discriminating.
        assert_eq!(content_hash("BODY-A"), content_hash("BODY-A"));
        assert_ne!(content_hash("BODY-A"), content_hash("BODY-B"));
        assert_eq!(content_hash("BODY-A").len(), 16);
    }
}
