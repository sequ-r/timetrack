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
    /// The item recurs (`RRULE` present); only its written first
    /// occurrence is represented here.
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
/// 10:00 across DST. The supported rule subset covers what calendar
/// UIs actually emit: `FREQ` daily/weekly/monthly/yearly, `INTERVAL`,
/// `COUNT`, `UNTIL`, `BYDAY` (with month ordinals), `BYMONTHDAY`,
/// `BYMONTH`, and `EXDATE` exclusions. Anything richer degrades to
/// the occurrences this subset generates, never to a failed fetch.
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
    let mut out = IcsParse::default();
    let mut current: Option<(ExternalKind, Vec<Prop>)> = None;
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
                && let Some((kind, props)) = current.take()
            {
                match items_from_props(kind, &props, tz, from_ms, until_ms) {
                    Some(items) => out.items.extend(items),
                    None => out.skipped += 1,
                }
            }
            continue;
        }
        if let Some((_, props)) = &mut current
            && let Some(prop) = parse_prop(line)
        {
            props.push(prop);
        }
    }
    Ok(out)
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

fn items_from_props(
    kind: ExternalKind,
    props: &[Prop],
    tz: &Tz,
    from_ms: i64,
    until_ms: i64,
) -> Option<Vec<ExternalItem>> {
    let uid = get(props, "UID")?.value.trim().to_string();
    if uid.is_empty() {
        return None;
    }
    let dtstart = get(props, "DTSTART")?;
    let started = prop_instant(dtstart, tz)?;
    let ended = match get(props, "DTEND") {
        Some(p) => prop_instant(p, tz)?,
        None => match get(props, "DURATION") {
            Some(p) => started + parse_duration_ms(&p.value)?,
            None => {
                let p = get(props, "DUE")?;
                prop_instant(p, tz)?
            }
        },
    };
    if ended <= started || !crate::instant_in_range(started) || !crate::instant_in_range(ended) {
        return None;
    }
    let title = get(props, "SUMMARY").map_or_else(String::new, |p| unescape_text(&p.value));
    let duration = ended - started;
    let overlaps = |s: i64| s < until_ms && s + duration > from_ms;
    let Some(rrule) = get(props, "RRULE") else {
        return Some(if overlaps(started) {
            vec![ExternalItem {
                uid,
                kind,
                title,
                started_at: started,
                ended_at: ended,
                recurring: false,
            }]
        } else {
            Vec::new()
        });
    };
    let zone = prop_zone(dtstart, tz);
    let mut excluded: std::collections::HashSet<i64> = std::collections::HashSet::new();
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
    let starts = expand_recurrence(&rrule.value, started, &zone, until_ms);
    Some(
        starts
            .into_iter()
            .filter(|s| overlaps(*s) && !excluded.contains(s))
            .map(|s| ExternalItem {
                uid: uid.clone(),
                kind,
                title: title.clone(),
                started_at: s,
                ended_at: s + duration,
                recurring: true,
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
            "INTERVAL" => rule.interval = val.parse::<i64>().ok().filter(|n| *n >= 1)?,
            "COUNT" => rule.count = Some(val.parse().ok()?),
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
                }
            }
            "BYDAY" => {
                for token in val.split(',') {
                    let token = token.trim();
                    if token.len() < 2 {
                        continue;
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
                        _ => continue,
                    };
                    rule.byday.push((ord.parse::<i64>().ok(), weekday));
                }
            }
            "BYMONTHDAY" => {
                rule.bymonthday = val
                    .split(',')
                    .filter_map(|n| n.trim().parse().ok())
                    .collect();
            }
            "BYMONTH" => {
                rule.bymonth = val
                    .split(',')
                    .filter_map(|n| n.trim().parse().ok())
                    .collect();
            }
            _ => {}
        }
    }
    have_freq.then_some(rule)
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

/// All occurrence starts for a rule, ascending, capped. The base
/// occurrence is in the zone's wall clock (`day_start + time-of-day`),
/// so DST transitions move the UTC instant, not the shown time.
fn expand_recurrence(rule_text: &str, started: i64, zone: &Tz, until_ms: i64) -> Vec<i64> {
    let Some(rule) = parse_rrule(rule_text, zone) else {
        return vec![started];
    };
    let base_day = zone.day_of(started);
    let tod = started - zone.day_start(base_day);
    let mut out: Vec<i64> = Vec::new();
    let mut generated = 0usize;
    let consider = |day: i64, out: &mut Vec<i64>, generated: &mut usize| -> bool {
        // Returns false once expansion must stop entirely.
        if day < base_day {
            return true;
        }
        if let Some(until_day) = rule.until_day
            && day > until_day
        {
            return false;
        }
        let instant = zone.day_start(day) + tod;
        if instant < started {
            return true;
        }
        if let Some(until) = rule.until_instant
            && instant > until
        {
            return false;
        }
        if instant >= until_ms {
            // Occurrences ascend, so nothing later re-enters the window.
            return false;
        }
        if let Some(cap) = rule.count
            && *generated >= cap
        {
            return false;
        }
        *generated += 1;
        out.push(instant);
        *generated < MAX_OCCURRENCES
    };

    match rule.freq {
        Freq::Daily => {
            let mut day = base_day;
            loop {
                if !consider(day, &mut out, &mut generated)
                    || out.len() + generated > MAX_OCCURRENCES && rule.count.is_none()
                {
                    break;
                }
                day += rule.interval;
                if generated >= MAX_OCCURRENCES && rule.count.is_none() {
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
            'outer: loop {
                for wd in &weekdays {
                    let day = week0 + week * 7 * rule.interval + wd;
                    if !consider(day, &mut out, &mut generated) {
                        break 'outer;
                    }
                    if generated >= MAX_OCCURRENCES && rule.count.is_none() {
                        break 'outer;
                    }
                }
                week += 1;
            }
        }
        Freq::Monthly | Freq::Yearly => {
            let (base_y, base_m, _) = crate::ymd_of(base_day);
            let base_month_idx = base_y * 12 + (base_m as i64 - 1);
            let mut step = 0i64;
            'outer: loop {
                let months: Vec<(i64, u32)> = if rule.freq == Freq::Monthly {
                    let idx = base_month_idx + step * rule.interval;
                    vec![(idx.div_euclid(12), (idx.rem_euclid(12) + 1) as u32)]
                } else {
                    let year = base_y + step * rule.interval;
                    let list = if rule.bymonth.is_empty() {
                        vec![base_m]
                    } else {
                        rule.bymonth.clone()
                    };
                    list.into_iter().map(|m| (year, m)).collect()
                };
                for (y, m) in months {
                    if rule.freq == Freq::Monthly
                        && !rule.bymonth.is_empty()
                        && !rule.bymonth.contains(&m)
                    {
                        continue;
                    }
                    for day in month_candidates(&rule, y, m, base_day, zone) {
                        if !consider(day, &mut out, &mut generated) {
                            break 'outer;
                        }
                        if generated >= MAX_OCCURRENCES && rule.count.is_none() {
                            break 'outer;
                        }
                    }
                }
                step += 1;
                // Safety: a rule whose candidates never reach the window
                // or a terminator still ends at the generation cap; bound
                // the month walk itself so pathological filters can't spin.
                if step > 1200 {
                    break;
                }
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
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
