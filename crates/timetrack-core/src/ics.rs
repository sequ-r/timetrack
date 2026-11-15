/* core/ics.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! Reading iCalendar data from external sources (REQUIREMENTS §19).
//!
//! Deliberately a small, strict subset: `VEVENT` and `VTODO`
//! components with a UID, a summary, and a concrete interval
//! (`DTSTART` plus `DTEND` or `DURATION`; a task's `DUE` ends it).
//! That is everything a *suggestion* needs — this is not a calendar
//! implementation: recurrences expand only the common `RRULE` subset
//! (see [`parse_ics_in_window`]), all-day (date-only) items carry no
//! interval worth suggesting and are counted as skipped, and anything
//! unparseable skips its item instead of failing the whole fetch, so
//! one exotic event cannot silence a calendar. A file that is not a
//! calendar at all is the one hard error.
//!
//! Pure like the rest of the core: instants resolve through the shared
//! parsers in an explicit zone (ADR 0009) — a `TZID` names its own
//! zone, a floating time takes the caller's.
//!
//! # Conformance matrix (#160)
//!
//! "Refused" always means the component counts once into
//! [`IcsParse::skipped`] and yields no occurrences — never a silently
//! mis-expanded approximation of a rule that was only half read.
//!
//! | Construct | Support |
//! |---|---|
//! | `VEVENT`/`VTODO` with UID, `DTSTART`, `DTEND`/`DURATION` (or `DUE`) | Supported |
//! | Date-only (`VALUE=DATE`, all-day) items | Refused |
//! | `DTSTART` in UTC (`Z`), `TZID` (IANA name), or floating (caller's zone) | Supported |
//! | Unknown `TZID` | Refused |
//! | `VTIMEZONE` | Partial: ignored — `TZID`s resolve against the IANA database, so a zone that exists only as a custom `VTIMEZONE` refuses |
//! | `STATUS:CANCELLED` (single item, series master, or override) | Supported: suggests nothing |
//! | Components other than `VEVENT`/`VTODO` | Ignored, not counted as skipped |
//! | `RRULE` with `FREQ=DAILY`/`WEEKLY`/`MONTHLY`/`YEARLY`, `INTERVAL`, `COUNT`, `UNTIL` | Supported |
//! | `FREQ=HOURLY`/`MINUTELY`/`SECONDLY` | Refused |
//! | `BYDAY` (ordinals in `MONTHLY`, and in `YEARLY` with `BYMONTH`) | Supported |
//! | Ordinal `BYDAY` in `YEARLY` without `BYMONTH` (nth weekday of the year) | Refused |
//! | `BYMONTHDAY`, `BYMONTH` (expanding in `MONTHLY`/`YEARLY`, limiting in `DAILY`/`WEEKLY`) | Supported |
//! | `BYHOUR`, `BYMINUTE` (`DTSTART`'s seconds are kept) | Supported |
//! | `BYSETPOS` (within each `FREQ` period: day, week, month, or year) | Supported |
//! | `BYSECOND`, `BYYEARDAY`, `BYWEEKNO`, unknown `BY*` parts | Refused |
//! | `WKST=MO` | Supported; any other `WKST` is refused |
//! | `EXDATE` (date-time lists; unparseable values are ignored) | Supported |
//! | `RDATE` (date-time lists) | Supported; `VALUE=DATE`/`VALUE=PERIOD` are refused |
//! | `EXRULE`, more than one `RRULE` | Refused |
//! | `RECURRENCE-ID` override moving/retitling/resizing one occurrence (including an `RDATE` one) | Supported |
//! | `RECURRENCE-ID` with `STATUS:CANCELLED` | Supported: the occurrence drops out |
//! | `RECURRENCE-ID;RANGE=...`, or an override carrying its own `RRULE` | Refused: the whole series counts as skipped |
//! | Malformed `RECURRENCE-ID` | Partial: that override counts as skipped, the series still expands |
//! | Override whose master is absent from the same calendar | Partial: the override stands alone as a single occurrence |
//! | Expansion across DST | Supported: arithmetic runs in the wall clock of `DTSTART`'s zone |

use crate::tz::Tz;

/// Whether a fetched item was a calendar event or a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalKind {
    Event,
    Task,
}

/// One fetched event or task with a concrete interval: everything a
/// suggestion (§19) is made of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalItem {
    /// The source's own `UID`: the dedupe identity across fetches.
    pub uid: String,
    pub kind: ExternalKind,
    pub title: String,
    pub started_at: i64,
    pub ended_at: i64,
    /// The item belongs to a recurrence set (its component carried an
    /// `RRULE` or `RDATE`, or it is a `RECURRENCE-ID` override);
    /// expanded occurrences share the UID.
    pub recurring: bool,
}

/// The outcome of parsing one calendar: the usable items and how many
/// components were skipped (no UID, no usable interval, unparseable).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IcsParse {
    pub items: Vec<ExternalItem>,
    pub skipped: usize,
}

/// Why a text is not a calendar at all.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not an iCalendar file (no BEGIN:VCALENDAR)")]
pub struct NotIcs;

/// Parse an iCalendar text in zone `tz` (floating times land there),
/// keeping every occurrence (expansion-capped; see
/// [`parse_ics_in_window`]).
pub fn parse_ics(text: &str, tz: &Tz) -> Result<IcsParse, NotIcs> {
    parse_ics_in_window(text, tz, i64::MIN / 4, i64::MAX / 4)
}

/// Parse, expanding recurrences (`RRULE`) and keeping only occurrences
/// overlapping `[from_ms, until_ms)`. Recurrence arithmetic runs in
/// the wall clock of each item's own zone, so a weekly 10:00 stays
/// 10:00 across DST. The supported subset covers what calendar UIs
/// actually emit: `FREQ` daily/weekly/monthly/yearly, `INTERVAL`,
/// `COUNT`, `UNTIL`, `BYDAY` (with month ordinals), `BYMONTHDAY`,
/// `BYMONTH`, `BYHOUR`, `BYMINUTE`, `BYSETPOS`, `WKST=MO` (the default
/// week start), `EXDATE` exclusions, `RDATE` additions, and
/// `RECURRENCE-ID` overrides of single occurrences (a cancelled
/// override drops its occurrence; components sharing a UID expand as
/// one series). Anything else — another `FREQ`, `BYSECOND`,
/// `BYYEARDAY`, `BYWEEKNO`, a non-Monday `WKST`, an unknown `BY*`
/// part, `EXRULE`, a `RANGE` override, or a malformed token — refuses
/// the whole component into [`IcsParse::skipped`], never silently
/// mis-expanding an ignored rule into wrong occurrences. The module
/// documentation carries the full conformance matrix.
pub fn parse_ics_in_window(
    text: &str,
    tz: &Tz,
    from_ms: i64,
    until_ms: i64,
) -> Result<IcsParse, NotIcs> {
    let lines = unfold(text);
    if !lines
        .iter()
        .any(|l| l.eq_ignore_ascii_case("BEGIN:VCALENDAR"))
    {
        return Err(NotIcs);
    }
    let mut components: Vec<Component> = Vec::new();
    let mut current: Option<Component> = None;
    for line in &lines {
        if let Some(rest) = strip_prefix_ci(line, "BEGIN:") {
            match rest.trim().to_ascii_uppercase().as_str() {
                "VEVENT" => current = Some((ExternalKind::Event, Vec::new())),
                "VTODO" => current = Some((ExternalKind::Task, Vec::new())),
                _ => {}
            }
            continue;
        }
        if let Some(rest) = strip_prefix_ci(line, "END:") {
            let ends = rest.trim().to_ascii_uppercase();
            if (ends == "VEVENT" || ends == "VTODO")
                && let Some(component) = current.take()
            {
                components.push(component);
            }
            continue;
        }
        if let Some((_, props)) = &mut current
            && let Some(prop) = parse_prop(line)
        {
            props.push(prop);
        }
    }
    // Group by (UID, kind) in first-appearance order: a master and its
    // RECURRENCE-ID overrides share a UID and must expand as one
    // series, wherever in the file each component sits.
    let mut out = IcsParse::default();
    let mut order: Vec<(String, bool)> = Vec::new();
    let mut groups: std::collections::HashMap<(String, bool), Vec<&Component>> =
        std::collections::HashMap::new();
    for component in &components {
        let Some(uid) = component_uid(&component.1) else {
            out.skipped += 1;
            continue;
        };
        let key = (uid, matches!(component.0, ExternalKind::Event));
        let members = groups.entry(key.clone()).or_insert_with(|| {
            order.push(key);
            Vec::new()
        });
        members.push(component);
    }
    for key in &order {
        emit_group(&groups[key], tz, from_ms, until_ms, &mut out);
    }
    Ok(out)
}

/// One `VEVENT`/`VTODO` component and its properties.
type Component = (ExternalKind, Vec<Prop>);

/// A component's dedupe identity; absent or blank means the component
/// cannot be a suggestion or part of a series at all.
fn component_uid(props: &[Prop]) -> Option<String> {
    let uid = get(props, "UID")?.value.trim().to_string();
    (!uid.is_empty()).then_some(uid)
}

/// Expand one UID group into `out`. A lone component (the common case)
/// and groups with no single master to resolve overrides against keep
/// the per-component behavior; exactly one master plus
/// `RECURRENCE-ID` overrides expands as one series, each override
/// replacing — or, when cancelled, dropping — its occurrence.
fn emit_group(members: &[&Component], tz: &Tz, from_ms: i64, until_ms: i64, out: &mut IcsParse) {
    let standalone = |members: &[&Component], out: &mut IcsParse| {
        for (kind, props) in members {
            match items_from_props(*kind, props, tz, from_ms, until_ms) {
                Some(items) => out.items.extend(items),
                None => out.skipped += 1,
            }
        }
    };
    let (masters, overrides): (Vec<&Component>, Vec<&Component>) = members
        .iter()
        .partition(|(_, props)| get(props, "RECURRENCE-ID").is_none());
    if overrides.is_empty() || masters.len() != 1 {
        standalone(members, out);
        return;
    }
    // A RANGE override or an override with its own rule rewrites every
    // later occurrence, which this subset cannot expand: refuse the
    // whole series rather than keep suggesting the old times.
    if overrides
        .iter()
        .any(|(_, props)| override_refuses_series(props))
    {
        out.skipped += members.len();
        return;
    }
    let (master_kind, master_props) = masters[0];
    let Some(base) = parse_base(master_props, tz) else {
        out.skipped += 1;
        standalone(&overrides, out);
        return;
    };
    if status_cancelled(master_props) {
        return; // The whole series is cancelled; overrides are moot.
    }
    let Some((mut starts, recurring)) = occurrence_starts(master_props, &base, tz, until_ms) else {
        out.skipped += 1;
        standalone(&overrides, out);
        return;
    };
    // Resolve overrides by the instant they replace, in the master's
    // zone (their own TZID/Z form still wins inside prop_instant). A
    // malformed RECURRENCE-ID cannot name its occurrence: count that
    // override as skipped but keep the series. Last one wins.
    let zone = base.zone.clone();
    let mut by_rid: std::collections::HashMap<i64, &Component> = std::collections::HashMap::new();
    for component in &overrides {
        let rid = get(&component.1, "RECURRENCE-ID").expect("partitioned on it");
        match prop_instant(rid, &zone) {
            Some(instant) => {
                by_rid.insert(instant, component);
            }
            None => out.skipped += 1,
        }
    }
    starts.retain(|s| !by_rid.contains_key(s));
    let duration = base.ended - base.started;
    let mut items: Vec<ExternalItem> = starts
        .into_iter()
        .filter(|s| *s < until_ms && *s + duration > from_ms)
        .map(|s| ExternalItem {
            uid: base.uid.clone(),
            kind: *master_kind,
            title: base.title.clone(),
            started_at: s,
            ended_at: s + duration,
            recurring,
        })
        .collect();
    for (kind, props) in by_rid.values() {
        if status_cancelled(props) {
            continue; // This occurrence simply does not happen.
        }
        match override_item(*kind, props, &base, tz) {
            Some(item) if item.started_at < until_ms && item.ended_at > from_ms => {
                items.push(item);
            }
            Some(_) => {}
            None => out.skipped += 1,
        }
    }
    items.sort_by_key(|i| i.started_at);
    out.items.extend(items);
}

/// Whether an override rewrites more than its own occurrence (a
/// `RANGE` parameter or a rule of its own), which refuses its series.
fn override_refuses_series(props: &[Prop]) -> bool {
    if get(props, "RRULE").is_some() || get(props, "EXRULE").is_some() {
        return true;
    }
    get(props, "RECURRENCE-ID").is_some_and(|rid| rid.param("RANGE").is_some())
}

/// One override as its own item: its written interval and summary,
/// inheriting the master's duration and summary where it omits them.
fn override_item(
    kind: ExternalKind,
    props: &[Prop],
    master: &Base,
    tz: &Tz,
) -> Option<ExternalItem> {
    let started = prop_instant(get(props, "DTSTART")?, tz)?;
    let writes_end = ["DTEND", "DURATION", "DUE"]
        .iter()
        .any(|name| get(props, name).is_some());
    let ended = match interval_end(props, tz, started) {
        Some(end) => end,
        // No end of its own: keep the master's duration. A written
        // but unparseable end refuses the override instead.
        None if !writes_end => started + (master.ended - master.started),
        None => return None,
    };
    if ended <= started || !crate::instant_in_range(started) || !crate::instant_in_range(ended) {
        return None;
    }
    let title =
        get(props, "SUMMARY").map_or_else(|| master.title.clone(), |p| unescape_text(&p.value));
    Some(ExternalItem {
        uid: master.uid.clone(),
        kind,
        title,
        started_at: started,
        ended_at: ended,
        recurring: true,
    })
}

fn status_cancelled(props: &[Prop]) -> bool {
    get(props, "STATUS").is_some_and(|p| p.value.trim().eq_ignore_ascii_case("CANCELLED"))
}

fn strip_prefix_ci<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    line.get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &line[prefix.len()..])
}

/// RFC 5545 line unfolding: a line starting with a space or tab
/// continues the previous one, minus that first character.
fn unfold(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.lines() {
        if (raw.starts_with(' ') || raw.starts_with('\t'))
            && let Some(prev) = out.last_mut()
        {
            prev.push_str(&raw[1..]);
        } else {
            out.push(raw.to_string());
        }
    }
    out
}

struct Prop {
    name: String,
    params: Vec<(String, String)>,
    value: String,
}

impl Prop {
    fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

fn parse_prop(line: &str) -> Option<Prop> {
    let (left, value) = line.split_once(':')?;
    let mut parts = left.split(';');
    let name = parts.next()?.to_ascii_uppercase();
    let params = parts
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (k.to_ascii_uppercase(), v.trim_matches('"').to_string()))
        .collect();
    Some(Prop {
        name,
        params,
        value: value.to_string(),
    })
}

fn get<'a>(props: &'a [Prop], name: &str) -> Option<&'a Prop> {
    props.iter().find(|p| p.name == name)
}

fn unescape_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n' | 'N') => out.push(' '),
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out.trim().to_string()
}

/// A component's written first interval: what every expanded
/// occurrence is a copy of.
struct Base {
    uid: String,
    title: String,
    started: i64,
    ended: i64,
    /// The wall-clock zone of `DTSTART`.
    zone: Tz,
}

/// The end a component writes for an interval starting at `started`:
/// `DTEND`, else `DURATION`, else a task's `DUE`.
fn interval_end(props: &[Prop], tz: &Tz, started: i64) -> Option<i64> {
    match get(props, "DTEND") {
        Some(p) => prop_instant(p, tz),
        None => match get(props, "DURATION") {
            Some(p) => Some(started + parse_duration_ms(&p.value)?),
            None => prop_instant(get(props, "DUE")?, tz),
        },
    }
}

fn parse_base(props: &[Prop], tz: &Tz) -> Option<Base> {
    let uid = component_uid(props)?;
    let dtstart = get(props, "DTSTART")?;
    let started = prop_instant(dtstart, tz)?;
    let ended = interval_end(props, tz, started)?;
    if ended <= started || !crate::instant_in_range(started) || !crate::instant_in_range(ended) {
        return None;
    }
    let title = get(props, "SUMMARY").map_or_else(String::new, |p| unescape_text(&p.value));
    let zone = prop_zone(dtstart, tz);
    Some(Base {
        uid,
        title,
        started,
        ended,
        zone,
    })
}

/// All `EXDATE` instants of a component. Unparseable values are
/// ignored (the occurrence they meant to drop simply stays).
fn excluded_starts(props: &[Prop], tz: &Tz) -> std::collections::HashSet<i64> {
    let mut excluded = std::collections::HashSet::new();
    for p in props.iter().filter(|p| p.name == "EXDATE") {
        for v in p.value.split(',') {
            let clone = Prop {
                name: "EXDATE".into(),
                params: p.params.clone(),
                value: v.to_string(),
            };
            if let Some(instant) = prop_instant(&clone, tz) {
                excluded.insert(instant);
            }
        }
    }
    excluded
}

/// All `RDATE` instants of a component, strictly: a date-only or
/// `PERIOD` value, or any unparseable token, refuses the component
/// rather than silently dropping occurrences the calendar asked for.
fn rdate_starts(props: &[Prop], tz: &Tz) -> Option<Vec<i64>> {
    let mut starts = Vec::new();
    for p in props.iter().filter(|p| p.name == "RDATE") {
        if p.param("VALUE")
            .is_some_and(|v| !v.eq_ignore_ascii_case("DATE-TIME"))
        {
            return None;
        }
        for v in p.value.split(',') {
            let v = v.trim();
            if v.contains('/') {
                return None; // A PERIOD, whatever VALUE claims.
            }
            let clone = Prop {
                name: "RDATE".into(),
                params: p.params.clone(),
                value: v.to_string(),
            };
            let instant = prop_instant(&clone, tz)?;
            if !crate::instant_in_range(instant) {
                return None;
            }
            starts.push(instant);
        }
    }
    Some(starts)
}

/// The occurrence starts a component describes — `RRULE` expansion
/// plus `RDATE`, minus `EXDATE`, ascending and capped — and whether
/// they form a recurrence set. An unsupported or malformed rule
/// refuses the component (it counts as skipped by the caller) instead
/// of expanding to occurrences the rule never described.
fn occurrence_starts(
    props: &[Prop],
    base: &Base,
    tz: &Tz,
    until_ms: i64,
) -> Option<(Vec<i64>, bool)> {
    if get(props, "EXRULE").is_some() || props.iter().filter(|p| p.name == "RRULE").count() > 1 {
        return None;
    }
    let rdates = rdate_starts(props, tz)?;
    let excluded = excluded_starts(props, tz);
    let (mut starts, recurring) = match get(props, "RRULE") {
        Some(rrule) => (
            expand_recurrence(&rrule.value, base.started, &base.zone, until_ms)?,
            true,
        ),
        None => (vec![base.started], !rdates.is_empty()),
    };
    starts.extend(rdates);
    starts.sort_unstable();
    starts.dedup();
    starts.retain(|s| !excluded.contains(s));
    starts.truncate(MAX_OCCURRENCES);
    Some((starts, recurring))
}

fn items_from_props(
    kind: ExternalKind,
    props: &[Prop],
    tz: &Tz,
    from_ms: i64,
    until_ms: i64,
) -> Option<Vec<ExternalItem>> {
    let base = parse_base(props, tz)?;
    if status_cancelled(props) {
        return Some(Vec::new());
    }
    // A standalone RECURRENCE-ID component (its master lives
    // elsewhere) is one occurrence of a series, marked as such.
    let standalone_override = get(props, "RECURRENCE-ID").is_some();
    let (starts, recurring) = occurrence_starts(props, &base, tz, until_ms)?;
    let duration = base.ended - base.started;
    Some(
        starts
            .into_iter()
            .filter(|s| *s < until_ms && *s + duration > from_ms)
            .map(|s| ExternalItem {
                uid: base.uid.clone(),
                kind,
                title: base.title.clone(),
                started_at: s,
                ended_at: s + duration,
                recurring: recurring || standalone_override,
            })
            .collect(),
    )
}

/// The zone a DATE-TIME property's wall clock lives in.
fn prop_zone(prop: &Prop, default_tz: &Tz) -> Tz {
    if prop.value.trim().ends_with(['Z', 'z']) {
        Tz::utc()
    } else if let Some(tzid) = prop.param("TZID") {
        Tz::parse(tzid).unwrap_or_else(|| default_tz.clone())
    } else {
        default_tz.clone()
    }
}

/// Hard cap on generated occurrences per item: an open-ended daily
/// rule must not turn one fetch into unbounded work.
const MAX_OCCURRENCES: usize = 512;

#[derive(Clone, Copy, PartialEq)]
enum Freq {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

struct RRule {
    freq: Freq,
    interval: i64,
    count: Option<usize>,
    until_instant: Option<i64>,
    until_day: Option<i64>,
    byday: Vec<(Option<i64>, i64)>, // (ordinal, Mon=0 weekday)
    bymonthday: Vec<i64>,
    bymonth: Vec<u32>,
    byhour: Vec<u32>,
    byminute: Vec<u32>,
    bysetpos: Vec<i64>,
}

/// A comma-separated list of small unsigned numbers in `lo..=hi`,
/// sorted and deduped; anything else refuses the rule.
fn parse_num_list(value: &str, lo: u32, hi: u32) -> Option<Vec<u32>> {
    let mut out = Vec::new();
    for n in value.split(',') {
        let m: u32 = n.trim().parse().ok()?;
        if !(lo..=hi).contains(&m) {
            return None;
        }
        out.push(m);
    }
    out.sort_unstable();
    out.dedup();
    Some(out)
}

fn parse_rrule(value: &str, zone: &Tz) -> Option<RRule> {
    let mut rule = RRule {
        freq: Freq::Daily,
        interval: 1,
        count: None,
        until_instant: None,
        until_day: None,
        byday: Vec::new(),
        bymonthday: Vec::new(),
        bymonth: Vec::new(),
        byhour: Vec::new(),
        byminute: Vec::new(),
        bysetpos: Vec::new(),
    };
    let mut have_freq = false;
    for part in value.split(';') {
        let Some((key, val)) = part.split_once('=') else {
            continue;
        };
        match key.trim().to_ascii_uppercase().as_str() {
            "FREQ" => {
                rule.freq = match val.trim().to_ascii_uppercase().as_str() {
                    "DAILY" => Freq::Daily,
                    "WEEKLY" => Freq::Weekly,
                    "MONTHLY" => Freq::Monthly,
                    "YEARLY" => Freq::Yearly,
                    _ => return None,
                };
                have_freq = true;
            }
            "INTERVAL" => {
                rule.interval = val.trim().parse::<i64>().ok().filter(|n| *n >= 1)?;
            }
            "COUNT" => {
                let n: usize = val.trim().parse().ok()?;
                if n < 1 {
                    return None;
                }
                rule.count = Some(n);
            }
            "UNTIL" => {
                let v = val.trim();
                if let Some((date, _)) = v.split_once('T') {
                    let probe = Prop {
                        name: "UNTIL".into(),
                        params: vec![],
                        value: v.to_string(),
                    };
                    let _ = date;
                    rule.until_instant = Some(prop_instant(&probe, zone)?);
                } else if v.len() == 8 {
                    let day = format!("{}-{}-{}", &v[0..4], &v[4..6], &v[6..8]);
                    let midday = crate::parse_anchor_day(&day, "until", zone).ok()?;
                    rule.until_day = Some(zone.day_of(midday));
                } else {
                    // Neither a DATE-TIME nor a DATE: malformed.
                    return None;
                }
            }
            "BYDAY" => {
                for token in val.split(',') {
                    let token = token.trim();
                    if token.len() < 2 {
                        return None;
                    }
                    let (ord, wd) = token.split_at(token.len() - 2);
                    let weekday = match wd.to_ascii_uppercase().as_str() {
                        "MO" => 0,
                        "TU" => 1,
                        "WE" => 2,
                        "TH" => 3,
                        "FR" => 4,
                        "SA" => 5,
                        "SU" => 6,
                        _ => return None,
                    };
                    let ordinal = if ord.is_empty() {
                        None
                    } else {
                        let n: i64 = ord.parse().ok()?;
                        // RFC 5545 ordinals run 1..=53; zero never
                        // matches and out-of-range never occurs.
                        if n == 0 || n.abs() > 53 {
                            return None;
                        }
                        Some(n)
                    };
                    rule.byday.push((ordinal, weekday));
                }
            }
            "BYMONTHDAY" => {
                let mut days = Vec::new();
                for n in val.split(',') {
                    let m: i64 = n.trim().parse().ok()?;
                    if m == 0 || m.abs() > 31 {
                        return None;
                    }
                    days.push(m);
                }
                rule.bymonthday = days;
            }
            "BYMONTH" => {
                // Sorted so YEARLY expansion stays chronological
                // (and COUNT counts in occurrence order).
                rule.bymonth = parse_num_list(val, 1, 12)?;
            }
            "BYHOUR" => rule.byhour = parse_num_list(val, 0, 23)?,
            "BYMINUTE" => rule.byminute = parse_num_list(val, 0, 59)?,
            "BYSETPOS" => {
                let mut positions = Vec::new();
                for n in val.split(',') {
                    let p: i64 = n.trim().parse().ok()?;
                    // RFC 5545 positions run 1..=366 from either end.
                    if p == 0 || p.abs() > 366 {
                        return None;
                    }
                    positions.push(p);
                }
                rule.bysetpos = positions;
            }
            // Supported explicitly as "refused": the subset has no
            // expansion for these, so their presence vetoes the rule.
            "BYSECOND" | "BYYEARDAY" | "BYWEEKNO" => {
                return None;
            }
            "WKST" => {
                // Weekly arithmetic assumes Monday week starts.
                if !val.trim().eq_ignore_ascii_case("MO") {
                    return None;
                }
            }
            other => {
                // Unknown BY* parts may silently change the set the
                // rule describes; refuse those, ignore the rest.
                if other.starts_with("BY") {
                    return None;
                }
            }
        }
    }
    if !have_freq {
        return None;
    }
    // An ordinal BYDAY in a YEARLY rule without BYMONTH counts from
    // the year's start (the 20th Monday of the year), not within a
    // month; the month-wise expansion below cannot express that.
    if rule.freq == Freq::Yearly
        && rule.bymonth.is_empty()
        && rule.byday.iter().any(|(ord, _)| ord.is_some())
    {
        return None;
    }
    Some(rule)
}

/// Monday-based weekday of a day index (day 0 was a Thursday).
fn weekday_mon(day: i64) -> i64 {
    (day + 3).rem_euclid(7)
}

/// Day index of the first of `year`-`month`, via the shared anchor
/// parser (midday is always on the named day).
fn first_day_of_month(year: i64, month: u32, zone: &Tz) -> Option<i64> {
    let midday =
        crate::parse_anchor_day(&format!("{year:04}-{month:02}-01"), "month", zone).ok()?;
    Some(zone.day_of(midday))
}

/// Candidate local days inside one month for MONTHLY/YEARLY rules.
fn month_candidates(rule: &RRule, year: i64, month: u32, base_day: i64, zone: &Tz) -> Vec<i64> {
    let Some(first) = first_day_of_month(year, month, zone) else {
        return Vec::new();
    };
    let len = crate::days_in_month(year, month);
    let mut days: Vec<i64> = Vec::new();
    if !rule.bymonthday.is_empty() {
        for md in &rule.bymonthday {
            let d = if *md > 0 { md - 1 } else { len + md };
            if (0..len).contains(&d) {
                days.push(first + d);
            }
        }
    }
    if !rule.byday.is_empty() {
        for (ord, wd) in &rule.byday {
            let matches: Vec<i64> = (0..len)
                .map(|d| first + d)
                .filter(|d| weekday_mon(*d) == *wd)
                .collect();
            match ord {
                Some(n) if *n > 0 => days.extend(matches.get((n - 1) as usize)),
                Some(n) => days.extend(matches.iter().nth_back((-n - 1) as usize)),
                None => days.extend(matches),
            }
        }
    }
    if rule.bymonthday.is_empty() && rule.byday.is_empty() {
        let base_dom = crate::ymd_of(base_day).2 as i64;
        if base_dom <= len {
            days.push(first + base_dom - 1);
        }
    }
    days.sort_unstable();
    days.dedup();
    days
}

/// Whether a day passes the rule's month-level limits (`BYMONTH`,
/// `BYMONTHDAY`), which limit `DAILY`/`WEEKLY` rules.
fn day_matches_month_limits(rule: &RRule, day: i64) -> bool {
    let (year, month, dom) = crate::ymd_of(day);
    if !rule.bymonth.is_empty() && !rule.bymonth.contains(&month) {
        return false;
    }
    if !rule.bymonthday.is_empty() {
        let len = crate::days_in_month(year, month);
        let dom = i64::from(dom);
        let hits = rule
            .bymonthday
            .iter()
            .any(|md| dom == if *md > 0 { *md } else { len + md + 1 });
        if !hits {
            return false;
        }
    }
    true
}

/// Whether a day passes a `DAILY` rule's `BYDAY` limit (the weekday
/// matches; ordinals mean nothing below `MONTHLY` and are ignored,
/// as in the `WEEKLY` expansion).
fn day_matches_byday_limit(rule: &RRule, day: i64) -> bool {
    rule.byday.is_empty() || rule.byday.iter().any(|(_, wd)| weekday_mon(day) == *wd)
}

/// One rule's expansion in progress. Occurrences are built a `FREQ`
/// period at a time — the day, week, month, or year the rule steps
/// over — because `BYSETPOS` picks positions within a whole period's
/// candidate set, so a period must be complete before any of it can
/// count toward `COUNT`.
struct Expander<'a> {
    rule: &'a RRule,
    zone: &'a Tz,
    started: i64,
    until_ms: i64,
    hours: Vec<i64>,
    minutes: Vec<i64>,
    /// `DTSTART`'s sub-minute piece of day, kept for every occurrence
    /// (`BYSECOND` is refused, so nothing may replace it).
    sec_ms: i64,
    out: Vec<i64>,
    generated: usize,
    /// Candidate instants built so far, selected or not.
    candidates: usize,
}

impl Expander<'_> {
    /// Every wall-clock instant a candidate day holds: `DTSTART`'s
    /// time of day, or the `BYHOUR` × `BYMINUTE` cross product when
    /// the rule names them. Anchored at the day's local midnight, so
    /// DST transitions move the UTC instant, not the shown time; on
    /// the transition day itself the offset changes mid-day, so each
    /// instant resolves as a wall clock instead.
    fn day_instants(&self, day: i64, into: &mut Vec<i64>) {
        let midnight = self.zone.day_start(day);
        let steady = self.zone.offset_at_ms(midnight)
            == self.zone.offset_at_ms(self.zone.day_start(day + 1) - 1);
        for hour in &self.hours {
            for minute in &self.minutes {
                let tod = hour * 3_600_000 + minute * 60_000 + self.sec_ms;
                into.push(if steady {
                    midnight + tod
                } else {
                    wall_instant(self.zone, day, tod)
                });
            }
        }
    }

    /// Whether a period opening on `first_day` — and, periods
    /// ascending, every later one — lies past the rule's `UNTIL` and
    /// the window, so expansion can stop. Checked even for periods
    /// whose filters select nothing, which would otherwise never
    /// reach a terminator.
    fn beyond(&self, first_day: i64) -> bool {
        if self.rule.until_day.is_some_and(|d| first_day > d) {
            return true;
        }
        let opens = self.zone.day_start(first_day);
        opens >= self.until_ms || self.rule.until_instant.is_some_and(|u| opens > u)
    }

    /// One selected instant against `UNTIL`, the window, and the
    /// caps. Returns false once expansion must stop entirely.
    fn consider(&mut self, instant: i64) -> bool {
        if instant < self.started {
            return true;
        }
        if self
            .rule
            .until_day
            .is_some_and(|d| self.zone.day_of(instant) > d)
        {
            return false;
        }
        if self.rule.until_instant.is_some_and(|u| instant > u) {
            return false;
        }
        if instant >= self.until_ms {
            // Occurrences ascend, so nothing later re-enters the window.
            return false;
        }
        if self.rule.count.is_some_and(|cap| self.generated >= cap) {
            return false;
        }
        self.generated += 1;
        self.out.push(instant);
        self.generated < MAX_OCCURRENCES
    }

    /// Feed one period of ascending candidate days: build its
    /// instants from `DTSTART` onward, let `BYSETPOS` pick, then run
    /// the selection through [`Expander::consider`]. Returns false
    /// once expansion must stop entirely.
    fn period(&mut self, days: &[i64]) -> bool {
        let Some(first) = days.first() else {
            return true;
        };
        if self.beyond(*first) {
            return false;
        }
        let mut instants = Vec::new();
        for day in days {
            self.day_instants(*day, &mut instants);
        }
        self.candidates += instants.len();
        if self.candidates > MAX_CANDIDATES {
            return false;
        }
        // The recurrence set opens at DTSTART: earlier instants of the
        // first period neither occur nor take a BYSETPOS position.
        instants.retain(|i| *i >= self.started);
        instants.sort_unstable();
        instants.dedup();
        let selected: Vec<i64> = if self.rule.bysetpos.is_empty() {
            instants
        } else {
            let mut picked = Vec::new();
            for pos in &self.rule.bysetpos {
                let idx = if *pos > 0 {
                    *pos - 1
                } else {
                    instants.len() as i64 + *pos
                };
                if let Some(instant) = usize::try_from(idx).ok().and_then(|i| instants.get(i)) {
                    picked.push(*instant);
                }
            }
            picked.sort_unstable();
            picked.dedup();
            picked
        };
        for instant in selected {
            if !self.consider(instant) {
                return false;
            }
        }
        true
    }
}

/// The instant at local wall-clock time `tod_ms` into `day` in
/// `zone`, on a day whose UTC offset changes. Iterates
/// `instant + offset == wall` to a fixed point like
/// [`Tz::day_start`] does for midnight; a repeated fall-back time
/// reads as its first occurrence and a skipped spring-forward time
/// with the offset before the gap (RFC 5545 §3.3.5).
fn wall_instant(zone: &Tz, day: i64, tod_ms: i64) -> i64 {
    let naive = day.saturating_mul(86_400_000).saturating_add(tod_ms);
    let mut t = naive - zone.offset_at_ms(naive);
    for _ in 0..4 {
        let next = naive - zone.offset_at_ms(t);
        if next == t {
            let earlier = naive - zone.offset_at_ms(t - 3_600_000);
            if earlier < t && earlier + zone.offset_at_ms(earlier) == naive {
                return earlier;
            }
            return t;
        }
        t = next;
    }
    naive - zone.offset_at_ms(zone.day_start(day))
}

/// Safety bound on daily/weekly period walks (monthly/yearly keep
/// their own): a rule whose filters never select — so the generation
/// cap never bites — still ends.
const MAX_PERIODS: i64 = 10_000;

/// Safety bound on candidate instants built per rule: a pathological
/// `BYHOUR`×`BYMINUTE` cross product in every period must not turn
/// one fetch into unbounded work either.
const MAX_CANDIDATES: usize = 100_000;

/// All occurrence starts for a rule, ascending, capped. The base
/// occurrence is in the zone's wall clock (`day_start + time-of-day`),
/// so DST transitions move the UTC instant, not the shown time.
fn expand_recurrence(rule_text: &str, started: i64, zone: &Tz, until_ms: i64) -> Option<Vec<i64>> {
    let rule = parse_rrule(rule_text, zone)?;
    let base_day = zone.day_of(started);
    let tod = started - zone.day_start(base_day);
    let mut exp = Expander {
        hours: if rule.byhour.is_empty() {
            vec![tod / 3_600_000]
        } else {
            rule.byhour.iter().map(|h| i64::from(*h)).collect()
        },
        minutes: if rule.byminute.is_empty() {
            vec![(tod % 3_600_000) / 60_000]
        } else {
            rule.byminute.iter().map(|m| i64::from(*m)).collect()
        },
        sec_ms: tod % 60_000,
        rule: &rule,
        zone,
        started,
        until_ms,
        out: Vec::new(),
        generated: 0,
        candidates: 0,
    };

    match rule.freq {
        Freq::Daily => {
            let mut day = base_day;
            let mut periods = 0i64;
            loop {
                if exp.beyond(day) {
                    break;
                }
                if day_matches_month_limits(&rule, day)
                    && day_matches_byday_limit(&rule, day)
                    && !exp.period(&[day])
                {
                    break;
                }
                day += rule.interval;
                periods += 1;
                if periods > MAX_PERIODS {
                    break;
                }
            }
        }
        Freq::Weekly => {
            let base_wd = weekday_mon(base_day);
            let weekdays: Vec<i64> = if rule.byday.is_empty() {
                vec![base_wd]
            } else {
                let mut v: Vec<i64> = rule.byday.iter().map(|(_, wd)| *wd).collect();
                v.sort_unstable();
                v.dedup();
                v
            };
            let week0 = base_day - base_wd;
            let mut week = 0i64;
            loop {
                let monday = week0 + week * 7 * rule.interval;
                if exp.beyond(monday) {
                    break;
                }
                let days: Vec<i64> = weekdays
                    .iter()
                    .map(|wd| monday + wd)
                    .filter(|day| day_matches_month_limits(&rule, *day))
                    .collect();
                if !exp.period(&days) {
                    break;
                }
                week += 1;
                if week > MAX_PERIODS {
                    break;
                }
            }
        }
        Freq::Monthly => {
            let (base_y, base_m, _) = crate::ymd_of(base_day);
            let base_month_idx = base_y * 12 + (base_m as i64 - 1);
            let mut step = 0i64;
            loop {
                let idx = base_month_idx + step * rule.interval;
                let (year, month) = (idx.div_euclid(12), (idx.rem_euclid(12) + 1) as u32);
                let Some(first) = first_day_of_month(year, month, zone) else {
                    break;
                };
                if exp.beyond(first) {
                    break;
                }
                // BYMONTH limits a MONTHLY rule: a stepped-over month
                // outside it simply holds no occurrence.
                if rule.bymonth.is_empty() || rule.bymonth.contains(&month) {
                    let days = month_candidates(&rule, year, month, base_day, zone);
                    if !exp.period(&days) {
                        break;
                    }
                }
                step += 1;
                // Safety: a rule whose candidates never reach the
                // window or a terminator still ends at the generation
                // cap; bound the month walk itself so pathological
                // filters can't spin.
                if step > 1200 {
                    break;
                }
            }
        }
        Freq::Yearly => {
            let (base_y, base_m, _) = crate::ymd_of(base_day);
            // BYMONTH expands a YEARLY rule; with day-level BY parts
            // but no BYMONTH, they expand across the whole year.
            let months: Vec<u32> = if !rule.bymonth.is_empty() {
                rule.bymonth.clone()
            } else if rule.byday.is_empty() && rule.bymonthday.is_empty() {
                vec![base_m]
            } else {
                (1..=12).collect()
            };
            let mut step = 0i64;
            loop {
                let year = base_y + step * rule.interval;
                let Some(first) = first_day_of_month(year, 1, zone) else {
                    break;
                };
                if exp.beyond(first) {
                    break;
                }
                let mut days = Vec::new();
                for month in &months {
                    days.extend(month_candidates(&rule, year, *month, base_day, zone));
                }
                days.sort_unstable();
                // One period per year, so BYSETPOS picks within it.
                if !exp.period(&days) {
                    break;
                }
                step += 1;
                if step > 1200 {
                    break;
                }
            }
        }
    }
    exp.out.sort_unstable();
    exp.out.dedup();
    Some(exp.out)
}

/// One DATE-TIME property to an instant. Date-only values (all-day
/// items) return `None`: they carry no interval worth suggesting.
fn prop_instant(prop: &Prop, default_tz: &Tz) -> Option<i64> {
    let value = prop.value.trim();
    let (date, time) = value.split_once('T')?;
    if date.len() != 8 || !date.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let day = format!("{}-{}-{}", &date[0..4], &date[4..6], &date[6..8]);
    let (time, zone) = if let Some(utc) = time.strip_suffix(['Z', 'z']) {
        (utc, Tz::utc())
    } else if let Some(tzid) = prop.param("TZID") {
        (time, Tz::parse(tzid)?)
    } else {
        (time, default_tz.clone())
    };
    if time.len() < 6 || !time[..6].chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let midday = crate::parse_anchor_day(&day, "ics day", &zone).ok()?;
    let at = crate::parse_moment(
        &format!("{}:{}", &time[0..2], &time[2..4]),
        "ics time",
        midday,
        &zone,
    )
    .ok()?;
    let secs: i64 = time[4..6].parse().ok()?;
    Some(at + secs * 1_000)
}

/// The ISO-8601 duration subset iCalendar uses: `PnW`, `PnDTnHnMnS`
/// (either part optional, no sign, no years/months — those are not
/// fixed lengths and never appear in practice here).
fn parse_duration_ms(value: &str) -> Option<i64> {
    let body = value.trim().strip_prefix('P')?;
    if let Some(weeks) = body.strip_suffix('W') {
        return weeks.parse::<i64>().ok()?.checked_mul(7 * 86_400_000);
    }
    let (days, times) = match body.split_once('T') {
        Some((d, t)) => (d, t),
        None => (body, ""),
    };
    let mut total: i64 = 0;
    if let Some(d) = days.strip_suffix('D') {
        total += d.parse::<i64>().ok()? * 86_400_000;
    } else if !days.is_empty() {
        return None;
    }
    let mut rest = times;
    for (mark, unit) in [('H', 3_600_000i64), ('M', 60_000), ('S', 1_000)] {
        if let Some(at) = rest.find(mark) {
            total += rest[..at].parse::<i64>().ok()? * unit;
            rest = &rest[at + 1..];
        }
    }
    if !rest.is_empty() || total <= 0 {
        return None;
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAL: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:ev1@x\r\nSUMMARY:Design review\r\nDTSTART:20260105T100000Z\r\nDTEND:20260105T110000Z\r\nEND:VEVENT\r\nBEGIN:VTODO\r\nUID:task1@x\r\nSUMMARY:Write\\, then ship\r\nDTSTART:20260105T120000Z\r\nDURATION:PT30M\r\nEND:VTODO\r\nBEGIN:VEVENT\r\nUID:allday@x\r\nSUMMARY:Holiday\r\nDTSTART;VALUE=DATE:20260106\r\nDTEND;VALUE=DATE:20260107\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nSUMMARY:No uid here\r\nDTSTART:20260105T100000Z\r\nDTEND:20260105T110000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn events_and_tasks_parse_and_the_rest_skips() {
        let out = parse_ics(CAL, &Tz::utc()).unwrap();
        assert_eq!(out.items.len(), 2);
        assert_eq!(out.skipped, 2, "all-day and uid-less items skip");
        let ev = &out.items[0];
        assert_eq!(ev.kind, ExternalKind::Event);
        assert_eq!(ev.title, "Design review");
        assert_eq!(ev.ended_at - ev.started_at, 3_600_000);
        let task = &out.items[1];
        assert_eq!(task.kind, ExternalKind::Task);
        assert_eq!(task.title, "Write, then ship");
        assert_eq!(task.ended_at - task.started_at, 1_800_000);
    }

    #[test]
    fn floating_and_tzid_times_land_in_their_zones() {
        let rome = Tz::parse("Europe/Rome").unwrap();
        // The same wall clock, summer: floating resolves in the passed
        // zone (UTC here), TZID in Rome (UTC+2) -- two hours apart.
        let text = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:a\nDTSTART:20260701T100000\nDTEND:20260701T110000\nEND:VEVENT\nBEGIN:VEVENT\nUID:b\nDTSTART;TZID=Europe/Rome:20260701T100000\nDTEND;TZID=Europe/Rome:20260701T110000\nEND:VEVENT\nEND:VCALENDAR\n";
        let out = parse_ics(text, &Tz::utc()).unwrap();
        assert_eq!(out.items.len(), 2, "{out:?}");
        assert_eq!(
            out.items[0].started_at - out.items[1].started_at,
            2 * 3_600_000
        );
        // A folded summary line rejoins without its leading space.
        let folded = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:c\nSUMMARY:Long ti\n tle here\nDTSTART:20260105T100000Z\nDURATION:P1DT1H30M\nRRULE:FREQ=WEEKLY\nEND:VEVENT\nEND:VCALENDAR\n";
        let out = parse_ics(folded, &rome).unwrap();
        assert_eq!(out.items[0].title, "Long title here");
        assert_eq!(
            out.items[0].ended_at - out.items[0].started_at,
            86_400_000 + 5_400_000
        );
        assert!(out.items[0].recurring);
    }

    #[test]
    fn recurrences_expand_count_byday_exdate_and_monthly_ordinals() {
        // Weekly Mon+Wed, three occurrences, the second excluded.
        let text = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:w\nSUMMARY:Standup\nDTSTART:20260105T100000Z\nDTEND:20260105T103000Z\nRRULE:FREQ=WEEKLY;BYDAY=MO,WE;COUNT=3\nEXDATE:20260107T100000Z\nEND:VEVENT\nBEGIN:VEVENT\nUID:m\nSUMMARY:Monthly sync\nDTSTART:20260113T090000Z\nDURATION:PT1H\nRRULE:FREQ=MONTHLY;BYDAY=2TU;COUNT=3\nEND:VEVENT\nEND:VCALENDAR\n";
        let out = parse_ics(text, &Tz::utc()).unwrap();
        let weekly: Vec<_> = out.items.iter().filter(|i| i.uid == "w").collect();
        assert_eq!(weekly.len(), 2, "COUNT=3 minus one EXDATE: {weekly:?}");
        assert_eq!(weekly[1].started_at - weekly[0].started_at, 7 * 86_400_000);
        let monthly: Vec<_> = out.items.iter().filter(|i| i.uid == "m").collect();
        assert_eq!(monthly.len(), 3);
        // Second Tuesdays: 13 Jan, 10 Feb, 10 Mar 2026.
        assert_eq!(
            monthly[1].started_at - monthly[0].started_at,
            28 * 86_400_000
        );
        assert_eq!(
            monthly[2].started_at - monthly[1].started_at,
            28 * 86_400_000
        );
        // A window after the first occurrence keeps only later ones.
        let from = weekly[0].ended_at + 1;
        let out = parse_ics_in_window(text, &Tz::utc(), from, i64::MAX / 4).unwrap();
        assert_eq!(out.items.iter().filter(|i| i.uid == "w").count(), 1);
    }

    #[test]
    fn weekly_wall_clock_survives_dst() {
        // Rome springs forward 2026-03-29: a weekly floating 10:00 in
        // Rome stays 10:00 local, so consecutive UTC instants across
        // the transition are 7 days minus the lost hour apart.
        let rome = Tz::parse("Europe/Rome").unwrap();
        let text = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:dst\nSUMMARY:Weekly\nDTSTART;TZID=Europe/Rome:20260323T100000\nDTEND;TZID=Europe/Rome:20260323T110000\nRRULE:FREQ=WEEKLY;COUNT=2\nEND:VEVENT\nEND:VCALENDAR\n";
        let out = parse_ics(text, &rome).unwrap();
        assert_eq!(out.items.len(), 2, "{out:?}");
        assert_eq!(
            out.items[1].started_at - out.items[0].started_at,
            7 * 86_400_000 - 3_600_000
        );
        assert_eq!(out.items[0].ended_at - out.items[0].started_at, 3_600_000);
    }

    /// RFC 5545 golden fixtures for the supported subset: every
    /// occurrence is an absolute instant, so a zone or arithmetic
    /// regression moves a number here, not just a delta.
    #[test]
    fn supported_subset_matches_rfc5545_golden_dates() {
        // All starts are 10:00Z on the named day; each event lasts an hour.
        let text = "BEGIN:VCALENDAR\n\
            BEGIN:VEVENT\nUID:daily-interval\nDTSTART:20260105T100000Z\nDTEND:20260105T110000Z\nRRULE:FREQ=DAILY;INTERVAL=2;COUNT=3\nEND:VEVENT\n\
            BEGIN:VEVENT\nUID:weekly-byday\nDTSTART:20260105T100000Z\nDTEND:20260105T110000Z\nRRULE:FREQ=WEEKLY;BYDAY=TU,TH;COUNT=4\nEND:VEVENT\n\
            BEGIN:VEVENT\nUID:weekly-wkst-mo\nDTSTART:20260105T100000Z\nDTEND:20260105T110000Z\nRRULE:FREQ=WEEKLY;WKST=MO;BYDAY=MO;COUNT=2\nEND:VEVENT\n\
            BEGIN:VEVENT\nUID:monthly-bymonthday\nDTSTART:20260115T100000Z\nDTEND:20260115T110000Z\nRRULE:FREQ=MONTHLY;BYMONTHDAY=15,-1;COUNT=4\nEND:VEVENT\n\
            BEGIN:VEVENT\nUID:monthly-ordinal\nDTSTART:20260105T100000Z\nDTEND:20260105T110000Z\nRRULE:FREQ=MONTHLY;BYDAY=1MO;COUNT=3\nEND:VEVENT\n\
            BEGIN:VEVENT\nUID:yearly\nDTSTART:20260105T100000Z\nDTEND:20260105T110000Z\nRRULE:FREQ=YEARLY;BYMONTH=1;BYMONTHDAY=5;COUNT=2\nEND:VEVENT\n\
            BEGIN:VEVENT\nUID:until-date\nDTSTART:20260105T100000Z\nDTEND:20260105T110000Z\nRRULE:FREQ=DAILY;UNTIL=20260107\nEND:VEVENT\n\
            BEGIN:VEVENT\nUID:until-instant\nDTSTART:20260105T100000Z\nDTEND:20260105T110000Z\nRRULE:FREQ=DAILY;UNTIL=20260106T100000Z\nEND:VEVENT\n\
            END:VCALENDAR\n";
        let out = parse_ics(text, &Tz::utc()).unwrap();
        assert_eq!(out.skipped, 0, "{out:?}");
        let starts = |uid: &str| -> Vec<i64> {
            let mut v: Vec<i64> = out
                .items
                .iter()
                .filter(|i| i.uid == uid)
                .map(|i| i.started_at)
                .collect();
            v.sort_unstable();
            v
        };
        // Every other day: Jan 5, 7, 9.
        assert_eq!(
            starts("daily-interval"),
            vec![1_767_607_200_000, 1_767_780_000_000, 1_767_952_800_000]
        );
        // Tue+Thu from Mon Jan 5: Jan 6, 8, 13, 15 (the base Monday
        // itself is not in the BYDAY set).
        assert_eq!(
            starts("weekly-byday"),
            vec![
                1_767_693_600_000,
                1_767_866_400_000,
                1_768_298_400_000,
                1_768_471_200_000
            ]
        );
        // An explicit default WKST=MO still expands: Jan 5 and 12.
        assert_eq!(
            starts("weekly-wkst-mo"),
            vec![1_767_607_200_000, 1_768_212_000_000]
        );
        // 15th and last day: Jan 15, Jan 31, Feb 15, Feb 28 (2026 is
        // not a leap year).
        assert_eq!(
            starts("monthly-bymonthday"),
            vec![
                1_768_471_200_000,
                1_769_853_600_000,
                1_771_149_600_000,
                1_772_272_800_000
            ]
        );
        // First Mondays: Jan 5, Feb 2, Mar 2.
        assert_eq!(
            starts("monthly-ordinal"),
            vec![1_767_607_200_000, 1_770_026_400_000, 1_772_445_600_000]
        );
        // Same month-day next year.
        assert_eq!(starts("yearly"), vec![1_767_607_200_000, 1_799_143_200_000]);
        // Date UNTIL is inclusive: Jan 5, 6, 7.
        assert_eq!(
            starts("until-date"),
            vec![1_767_607_200_000, 1_767_693_600_000, 1_767_780_000_000]
        );
        // Instant UNTIL is inclusive: Jan 5, 6.
        assert_eq!(
            starts("until-instant"),
            vec![1_767_607_200_000, 1_767_693_600_000]
        );
        for item in &out.items {
            assert!(item.recurring, "{item:?}");
            assert_eq!(item.ended_at - item.started_at, 3_600_000, "{item:?}");
        }
    }

    /// Unsupported or malformed rules refuse the component into
    /// `skipped` instead of expanding wrong occurrences.
    #[test]
    fn unsupported_rrule_parts_refuse_the_component() {
        let rules = [
            "FREQ=HOURLY;COUNT=2",
            "FREQ=MINUTELY;COUNT=2",
            "FREQ=SECONDLY;COUNT=2",
            "FREQ=WEEKLY;BYDAY=MO;COUNT=2;BYSETPOS=0",
            "FREQ=WEEKLY;BYDAY=MO;COUNT=2;BYSETPOS=367",
            "FREQ=DAILY;COUNT=2;BYHOUR=24",
            "FREQ=DAILY;COUNT=2;BYHOUR=ten",
            "FREQ=DAILY;COUNT=2;BYMINUTE=60",
            "FREQ=DAILY;COUNT=2;BYSECOND=0",
            "FREQ=YEARLY;COUNT=2;BYYEARDAY=32",
            "FREQ=YEARLY;COUNT=2;BYWEEKNO=3",
            "FREQ=YEARLY;BYDAY=20MO;COUNT=2",
            "FREQ=WEEKLY;BYDAY=MO;COUNT=2;WKST=SU",
            "FREQ=DAILY;COUNT=2;BYEASTER=1",
            "FREQ=WEEKLY;BYDAY=XX;COUNT=2",
            "FREQ=MONTHLY;BYDAY=0MO;COUNT=2",
            "FREQ=YEARLY;BYMONTH=13;COUNT=2",
            "FREQ=MONTHLY;BYMONTHDAY=0;COUNT=2",
            "FREQ=MONTHLY;BYMONTHDAY=xx;COUNT=2",
            "FREQ=DAILY;UNTIL=soon",
            "FREQ=DAILY;INTERVAL=0;COUNT=2",
            "FREQ=DAILY;COUNT=0",
            "FREQ=DAILY;COUNT=lots",
        ];
        let mut text = String::from("BEGIN:VCALENDAR\n");
        for (n, rule) in rules.iter().enumerate() {
            text.push_str(&format!(
                "BEGIN:VEVENT\nUID:bad{n}@x\nDTSTART:20260105T100000Z\nDTEND:20260105T110000Z\nRRULE:{rule}\nEND:VEVENT\n"
            ));
        }
        text.push_str("END:VCALENDAR\n");
        let out = parse_ics(&text, &Tz::utc()).unwrap();
        assert_eq!(out.items.len(), 0, "{out:?}");
        assert_eq!(out.skipped, rules.len(), "{out:?}");
    }

    // 2026-01-05T10:00:00Z, a Monday (the golden fixture's anchor).
    const ANCHOR: i64 = 1_767_607_200_000;
    const DAY: i64 = 86_400_000;
    const HOUR: i64 = 3_600_000;

    fn starts_of(out: &IcsParse, uid: &str) -> Vec<i64> {
        let mut v: Vec<i64> = out
            .items
            .iter()
            .filter(|i| i.uid == uid)
            .map(|i| i.started_at)
            .collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn rdate_adds_occurrences_and_exdate_still_removes() {
        // Weekly COUNT=2 (Jan 5, 12) plus two RDATEs, one EXDATE'd; a
        // rule-less event whose extra occurrences are RDATE-only.
        let text = "BEGIN:VCALENDAR\n\
            BEGIN:VEVENT\nUID:rdate-weekly\nSUMMARY:Weekly\nDTSTART:20260105T100000Z\nDTEND:20260105T110000Z\nRRULE:FREQ=WEEKLY;COUNT=2\nRDATE:20260120T100000Z,20260122T100000Z\nEXDATE:20260122T100000Z\nEND:VEVENT\n\
            BEGIN:VEVENT\nUID:rdate-only\nSUMMARY:One-offs\nDTSTART:20260105T100000Z\nDTEND:20260105T110000Z\nRDATE:20260108T100000Z\nRDATE:20260115T100000Z\nEND:VEVENT\n\
            END:VCALENDAR\n";
        let out = parse_ics(text, &Tz::utc()).unwrap();
        assert_eq!(out.skipped, 0, "{out:?}");
        // RDATEs ride on top of COUNT (they are not counted by it),
        // and EXDATE removes RDATE occurrences too.
        assert_eq!(
            starts_of(&out, "rdate-weekly"),
            vec![ANCHOR, ANCHOR + 7 * DAY, ANCHOR + 15 * DAY]
        );
        assert_eq!(
            starts_of(&out, "rdate-only"),
            vec![ANCHOR, ANCHOR + 3 * DAY, ANCHOR + 10 * DAY]
        );
        assert!(out.items.iter().all(|i| i.recurring), "{out:?}");
        // A window opening after Jan 12 keeps only the Jan 20 RDATE.
        let out = parse_ics_in_window(text, &Tz::utc(), ANCHOR + 8 * DAY, i64::MAX / 4).unwrap();
        assert_eq!(starts_of(&out, "rdate-weekly"), vec![ANCHOR + 15 * DAY]);
    }

    #[test]
    fn rdate_periods_dates_and_garbage_refuse_the_component() {
        let text = "BEGIN:VCALENDAR\n\
            BEGIN:VEVENT\nUID:period\nDTSTART:20260105T100000Z\nDTEND:20260105T110000Z\nRDATE;VALUE=PERIOD:20260106T100000Z/20260106T110000Z\nEND:VEVENT\n\
            BEGIN:VEVENT\nUID:date\nDTSTART:20260105T100000Z\nDTEND:20260105T110000Z\nRDATE;VALUE=DATE:20260106\nEND:VEVENT\n\
            BEGIN:VEVENT\nUID:garbage\nDTSTART:20260105T100000Z\nDTEND:20260105T110000Z\nRDATE:next-tuesday\nEND:VEVENT\n\
            END:VCALENDAR\n";
        let out = parse_ics(text, &Tz::utc()).unwrap();
        assert_eq!(out.items.len(), 0, "{out:?}");
        assert_eq!(out.skipped, 3, "{out:?}");
    }

    #[test]
    fn recurrence_id_overrides_move_and_cancel_one_occurrence() {
        // Weekly Jan 5/12/19: the 12th moves to Jan 13 15:00 for 90
        // minutes under a new title, the 19th is cancelled. The move
        // is written *before* its master: grouping is by UID, not by
        // file position.
        let text = "BEGIN:VCALENDAR\n\
            BEGIN:VEVENT\nUID:series\nSUMMARY:Moved sync\nRECURRENCE-ID:20260112T100000Z\nDTSTART:20260113T150000Z\nDTEND:20260113T163000Z\nEND:VEVENT\n\
            BEGIN:VEVENT\nUID:series\nSUMMARY:Sync\nDTSTART:20260105T100000Z\nDTEND:20260105T103000Z\nRRULE:FREQ=WEEKLY;COUNT=3\nEND:VEVENT\n\
            BEGIN:VEVENT\nUID:series\nSUMMARY:Sync\nRECURRENCE-ID:20260119T100000Z\nDTSTART:20260119T100000Z\nDTEND:20260119T103000Z\nSTATUS:CANCELLED\nEND:VEVENT\n\
            END:VCALENDAR\n";
        let out = parse_ics(text, &Tz::utc()).unwrap();
        assert_eq!(out.skipped, 0, "{out:?}");
        assert_eq!(
            starts_of(&out, "series"),
            vec![ANCHOR, ANCHOR + 8 * DAY + 5 * HOUR],
            "{out:?}"
        );
        let moved = out
            .items
            .iter()
            .find(|i| i.started_at == ANCHOR + 8 * DAY + 5 * HOUR)
            .unwrap();
        assert_eq!(moved.title, "Moved sync");
        assert_eq!(moved.ended_at - moved.started_at, 90 * 60_000);
        assert!(moved.recurring);
        let kept = out.items.iter().find(|i| i.started_at == ANCHOR).unwrap();
        assert_eq!(kept.title, "Sync");
        assert_eq!(kept.ended_at - kept.started_at, 30 * 60_000);
    }

    #[test]
    fn overrides_resolve_in_the_masters_zone_across_dst() {
        // Rome springs forward 2026-03-29. The Mar 30 occurrence of a
        // weekly 10:00 Rome series moves to Apr 1 09:00 Rome. The
        // RECURRENCE-ID is written in the master's TZID, and the whole
        // parse runs with a UTC caller zone: matching is by instant.
        let text = "BEGIN:VCALENDAR\n\
            BEGIN:VEVENT\nUID:dst-series\nSUMMARY:Weekly\nDTSTART;TZID=Europe/Rome:20260323T100000\nDTEND;TZID=Europe/Rome:20260323T110000\nRRULE:FREQ=WEEKLY;COUNT=2\nEND:VEVENT\n\
            BEGIN:VEVENT\nUID:dst-series\nSUMMARY:Moved\nRECURRENCE-ID;TZID=Europe/Rome:20260330T100000\nDTSTART;TZID=Europe/Rome:20260401T090000\nDTEND;TZID=Europe/Rome:20260401T100000\nEND:VEVENT\n\
            END:VCALENDAR\n";
        let out = parse_ics(text, &Tz::utc()).unwrap();
        assert_eq!(out.skipped, 0, "{out:?}");
        // Mar 23 10:00 CET is 09:00Z (anchor + 77d - 1h); Apr 1 09:00
        // CEST is 07:00Z, nine days minus two hours later. The unmoved
        // Mar 30 08:00Z occurrence is gone.
        assert_eq!(
            starts_of(&out, "dst-series"),
            vec![
                ANCHOR + 77 * DAY - HOUR,
                ANCHOR + 77 * DAY - HOUR + 9 * DAY - 2 * HOUR
            ],
            "{out:?}"
        );
    }

    #[test]
    fn cancelled_items_exrule_and_range_overrides_suggest_nothing() {
        let text = "BEGIN:VCALENDAR\n\
            BEGIN:VEVENT\nUID:cancelled\nDTSTART:20260105T100000Z\nDTEND:20260105T110000Z\nSTATUS:CANCELLED\nEND:VEVENT\n\
            BEGIN:VEVENT\nUID:exrule\nDTSTART:20260105T100000Z\nDTEND:20260105T110000Z\nRRULE:FREQ=DAILY;COUNT=3\nEXRULE:FREQ=WEEKLY;BYDAY=SA\nEND:VEVENT\n\
            BEGIN:VEVENT\nUID:range\nDTSTART:20260105T100000Z\nDTEND:20260105T110000Z\nRRULE:FREQ=WEEKLY;COUNT=3\nEND:VEVENT\n\
            BEGIN:VEVENT\nUID:range\nRECURRENCE-ID;RANGE=THISANDFUTURE:20260112T100000Z\nDTSTART:20260112T120000Z\nDTEND:20260112T130000Z\nEND:VEVENT\n\
            END:VCALENDAR\n";
        let out = parse_ics(text, &Tz::utc()).unwrap();
        assert_eq!(out.items.len(), 0, "{out:?}");
        // Cancelled suggests nothing but is not an error (no skip);
        // EXRULE refuses its component; a RANGE override refuses its
        // whole two-component series.
        assert_eq!(out.skipped, 3, "{out:?}");
    }

    #[test]
    fn byhour_and_byminute_multiply_occurrences_within_a_day() {
        let text = "BEGIN:VCALENDAR\n\
            BEGIN:VEVENT\nUID:byhour\nDTSTART:20260105T093000Z\nDURATION:PT30M\nRRULE:FREQ=DAILY;BYHOUR=9,17;BYMINUTE=30;COUNT=4\nEND:VEVENT\n\
            BEGIN:VEVENT\nUID:byminute\nDTSTART:20260105T100000Z\nDURATION:PT15M\nRRULE:FREQ=DAILY;BYMINUTE=0,30;COUNT=3\nEND:VEVENT\n\
            END:VCALENDAR\n";
        let out = parse_ics(text, &Tz::utc()).unwrap();
        assert_eq!(out.skipped, 0, "{out:?}");
        // Anchor is 10:00Z: 09:30 and 17:30 on Jan 5, then Jan 6 —
        // COUNT counts the multiplied instants, not days.
        assert_eq!(
            starts_of(&out, "byhour"),
            vec![
                ANCHOR - 30 * 60_000,
                ANCHOR + 7 * HOUR + 30 * 60_000,
                ANCHOR + DAY - 30 * 60_000,
                ANCHOR + DAY + 7 * HOUR + 30 * 60_000,
            ]
        );
        // BYMINUTE alone keeps DTSTART's hour.
        assert_eq!(
            starts_of(&out, "byminute"),
            vec![ANCHOR, ANCHOR + 30 * 60_000, ANCHOR + DAY]
        );
    }

    #[test]
    fn bysetpos_picks_within_each_period() {
        let text = "BEGIN:VCALENDAR\n\
            BEGIN:VEVENT\nUID:last-weekday\nDTSTART:20260130T100000Z\nDTEND:20260130T110000Z\nRRULE:FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1;COUNT=3\nEND:VEVENT\n\
            BEGIN:VEVENT\nUID:last-hour\nDTSTART:20260105T090000Z\nDURATION:PT1H\nRRULE:FREQ=DAILY;BYHOUR=9,12,17;BYSETPOS=-1;COUNT=2\nEND:VEVENT\n\
            BEGIN:VEVENT\nUID:first-of-week\nDTSTART:20260105T100000Z\nDTEND:20260105T110000Z\nRRULE:FREQ=WEEKLY;BYDAY=MO,WE,FR;BYSETPOS=1;COUNT=2\nEND:VEVENT\n\
            END:VCALENDAR\n";
        let out = parse_ics(text, &Tz::utc()).unwrap();
        assert_eq!(out.skipped, 0, "{out:?}");
        // Last weekday of the month: Fri Jan 30 (the 31st is a
        // Saturday), Fri Feb 27, Tue Mar 31.
        assert_eq!(
            starts_of(&out, "last-weekday"),
            vec![ANCHOR + 25 * DAY, ANCHOR + 53 * DAY, ANCHOR + 85 * DAY]
        );
        // Last of the day's BYHOUR set: 17:00 daily.
        assert_eq!(
            starts_of(&out, "last-hour"),
            vec![ANCHOR + 7 * HOUR, ANCHOR + DAY + 7 * HOUR]
        );
        // First of each week's Mo/We/Fr set: the Mondays.
        assert_eq!(
            starts_of(&out, "first-of-week"),
            vec![ANCHOR, ANCHOR + 7 * DAY]
        );
    }

    #[test]
    fn byhour_wall_clock_survives_dst() {
        // Daily 09:00 and 17:00 in Rome across the 2026-03-29 spring
        // forward: local times hold, so same-time-next-day gaps are
        // 23 hours, not 24.
        let text = "BEGIN:VCALENDAR\n\
            BEGIN:VEVENT\nUID:dst-hours\nDTSTART;TZID=Europe/Rome:20260328T090000\nDTEND;TZID=Europe/Rome:20260328T100000\nRRULE:FREQ=DAILY;BYHOUR=9,17;COUNT=4\nEND:VEVENT\n\
            BEGIN:VEVENT\nUID:dst-fall\nDTSTART;TZID=Europe/Rome:20261018T100000\nDTEND;TZID=Europe/Rome:20261018T110000\nRRULE:FREQ=WEEKLY;COUNT=2\nEND:VEVENT\n\
            END:VCALENDAR\n";
        let out = parse_ics(text, &Tz::utc()).unwrap();
        assert_eq!(out.skipped, 0, "{out:?}");
        // Mar 28 09:00 CET is 08:00Z (anchor + 82d - 2h).
        let first = ANCHOR + 82 * DAY - 2 * HOUR;
        assert_eq!(
            starts_of(&out, "dst-hours"),
            vec![
                first,
                first + 8 * HOUR,
                first + 23 * HOUR,
                first + 31 * HOUR
            ],
            "{out:?}"
        );
        // A plain weekly landing *on* the 2026-10-25 fall-back day:
        // Oct 18 10:00 CEST to Oct 25 10:00 CET is a week plus the
        // regained hour (elapsed-since-midnight used to drop it).
        let fall = starts_of(&out, "dst-fall");
        assert_eq!(fall.len(), 2, "{out:?}");
        assert_eq!(fall[1] - fall[0], 7 * DAY + HOUR);
    }

    #[test]
    fn daily_and_weekly_by_parts_limit_instead_of_being_ignored() {
        // Weekdays-only daily and a weekly rule limited to January:
        // both BY parts used to be parsed but silently ignored.
        let text = "BEGIN:VCALENDAR\n\
            BEGIN:VEVENT\nUID:weekdays\nDTSTART:20260109T100000Z\nDTEND:20260109T110000Z\nRRULE:FREQ=DAILY;BYDAY=MO,TU,WE,TH,FR;COUNT=3\nEND:VEVENT\n\
            BEGIN:VEVENT\nUID:jan-mondays\nDTSTART:20260126T100000Z\nDTEND:20260126T110000Z\nRRULE:FREQ=WEEKLY;BYMONTH=1;COUNT=3\nEND:VEVENT\n\
            END:VCALENDAR\n";
        let out = parse_ics_in_window(text, &Tz::utc(), 0, ANCHOR + 40 * DAY).unwrap();
        // Fri Jan 9, then Mon Jan 12 and Tue Jan 13 (no weekend).
        assert_eq!(
            starts_of(&out, "weekdays"),
            vec![ANCHOR + 4 * DAY, ANCHOR + 7 * DAY, ANCHOR + 8 * DAY]
        );
        // COUNT=3 wants a third Monday, but February is out of
        // BYMONTH: only Jan 26 lands before the window closes the walk.
        assert_eq!(starts_of(&out, "jan-mondays"), vec![ANCHOR + 21 * DAY]);
    }

    #[test]
    fn not_a_calendar_is_the_one_hard_error() {
        assert!(parse_ics("just text", &Tz::utc()).is_err());
        assert!(parse_ics("", &Tz::utc()).is_err());
        // Durations outside the fixed-length subset refuse.
        assert_eq!(parse_duration_ms("P1Y"), None);
        assert_eq!(parse_duration_ms("PT"), None);
        assert_eq!(parse_duration_ms("P2W"), Some(14 * 86_400_000));
    }
}
