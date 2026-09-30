// SPDX-License-Identifier: MIT
//! NANU lifecycle and causal orbit discontinuity policy; never opens a camera.
use super::{
    cep,
    data::{DAY, Frames, GPS_JD, Sample, V3},
    validation,
};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Datelike, NaiveDateTime, Timelike, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[cfg(test)]
#[path = "health_tests.rs"]
mod tests;

pub fn utc_gps(t: DateTime<Utc>) -> Result<f64> {
    let ((a, b), warning) = erfars::timescales::Dtf2d(
        true,
        t.year(),
        t.month() as i32,
        t.day() as i32,
        t.hour() as i32,
        t.minute() as i32,
        t.second() as f64 + t.nanosecond() as f64 / 1e9,
    )
    .map_err(|e| anyhow::anyhow!("ERFA UTC date: {e:?}"))?;
    ensure!(
        warning == 0,
        "UTC date exceeds the leap-second table's supported range"
    );
    let ((a, b), warning) =
        erfars::timescales::Utctai(a, b).map_err(|e| anyhow::anyhow!("ERFA UTC/TAI: {e:?}"))?;
    ensure!(
        warning == 0,
        "UTC/TAI conversion needs an updated leap-second table"
    );
    Ok(((((a - GPS_JD) + b) * DAY - 19.) * 1e6).round() / 1e6)
}
pub fn now() -> Result<f64> {
    utc_gps(Utc::now())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Notice {
    pub number: String,
    pub kind: String,
    pub reference: Option<String>,
    pub issued_gps: f64,
    pub issued_from_dtg: bool,
    pub prn: Option<usize>,
    pub svn: Option<usize>,
    pub start_gps: Option<f64>,
    pub stop_gps: Option<f64>,
    pub review_required: bool,
    pub mentioned_prns: Vec<usize>,
}
fn regex(s: &str) -> Regex {
    Regex::new(s).expect("Static NANU pattern")
}
fn field(text: &str, name: &str) -> Option<String> {
    regex(&format!(
        r"(?m)^\s*(?:\d+\.\s*)?{}:\s*([^\r\n]+)",
        regex::escape(name)
    ))
    .captures(text)
    .map(|m| m[1].trim().to_owned())
}
fn date(text: &str, format: &str) -> Result<f64> {
    utc_gps(NaiveDateTime::parse_from_str(text, format)?.and_utc())
}
pub fn parse_notice(text: &str, known: f64) -> Result<Notice> {
    let number = field(text, "NANU NUMBER")
        .or_else(|| {
            regex(r"\(NANU\)\s+(\d{7})")
                .captures(text)
                .map(|c| c[1].into())
        })
        .context("Missing NANU number")?;
    ensure!(regex(r"^\d{7}$").is_match(&number), "Invalid NANU number");
    let kind = field(text, "NANU TYPE")
        .or_else(|| {
            regex(r"NANU TYPE:\s*(\w+)")
                .captures(text)
                .map(|c| c[1].into())
        })
        .context("Missing NANU type")?;
    ensure!(
        [
            "FCSTDV",
            "FCSTMX",
            "FCSTUUFN",
            "FCSTEXTD",
            "FCSTSUMM",
            "FCSTCANC",
            "FCSTRESCD",
            "UNUSUFN",
            "UNUSABLE",
            "UNUNOREF",
            "USABINIT",
            "DECOM",
            "GENERAL",
            "LAUNCH",
            "LEAPSEC"
        ]
        .contains(&kind.as_str()),
        "Unsupported NANU type {kind}"
    );
    let issued = field(text, "NANU DTG");
    let issued_gps = match issued.as_ref() {
        Some(s) => date(s, "%d%H%MZ %b %Y")?,
        None => known,
    };
    ensure!(
        issued_gps.is_finite(),
        "Notice lacks a valid DTG or retrieval time"
    );
    let reference =
        field(text, "REFERENCE NANU").filter(|s| !["N/A", "NA", "NONE"].contains(&s.as_str()));
    ensure!(
        reference
            .as_ref()
            .is_none_or(|s| regex(r"^\d{7}$").is_match(s)),
        "Invalid NANU reference"
    );
    let epoch = |prefix: &str| -> Result<Option<f64>> {
        let d = field(text, &format!("{prefix} CALENDAR DATE"));
        let t = field(text, &format!("{prefix} TIME ZULU"));
        match (d, t) {
            (Some(d), Some(t))
                if !["N/A", "NA"].contains(&d.to_uppercase().as_str())
                    && !["N/A", "NA"].contains(&t.to_uppercase().as_str())
                    && !d.to_uppercase().contains("FURTHER")
                    && !t.to_uppercase().contains("FURTHER") =>
            {
                Ok(Some(date(&format!("{d} {t}"), "%d %b %Y %H%M")?))
            }
            _ => Ok(None),
        }
    };
    let start_gps = epoch("START")?
        .or(epoch("UNUSABLE START")?)
        .or(epoch("USABLE START")?);
    let stop_gps = epoch("STOP")?;
    ensure!(
        start_gps.zip(stop_gps).is_none_or(|(a, b)| b >= a),
        "NANU stop precedes start"
    );
    let mentioned_prns: Vec<_> = regex(r"\bPRN\s*0*(\d{1,2})\b")
        .captures_iter(text)
        .map(|c| c[1].parse::<usize>().unwrap())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let review_required = ["GENERAL", "LAUNCH", "LEAPSEC"].contains(&kind.as_str())
        && (mentioned_prns.is_empty() || mentioned_prns.iter().any(|p| (1..=32).contains(p)));
    Ok(Notice {
        number,
        kind,
        reference,
        issued_gps,
        issued_from_dtg: issued.is_some(),
        prn: field(text, "PRN").map(|s| s.parse()).transpose()?,
        svn: field(text, "SVN").map(|s| s.parse()).transpose()?,
        start_gps,
        stop_gps,
        review_required,
        mentioned_prns,
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Outage {
    pub prn: usize,
    pub svn: Option<usize>,
    pub start_gps: f64,
    pub stop_gps: Option<f64>,
    pub kind: String,
    pub number: String,
    pub root_notice: String,
    pub issued_gps: f64,
}
pub fn resolve(notices: &[Notice], as_of: f64) -> Result<(Vec<Outage>, Vec<Notice>)> {
    let mut known: Vec<_> = notices.iter().filter(|n| n.issued_gps <= as_of).collect();
    known.sort_by(|a, b| {
        a.issued_gps
            .total_cmp(&b.issued_gps)
            .then(a.number.cmp(&b.number))
    });
    let by_number: BTreeMap<_, _> = known.iter().map(|n| (n.number.as_str(), *n)).collect();
    ensure!(by_number.len() == known.len(), "Duplicate NANU numbers");
    fn visit<'a>(
        n: &'a Notice,
        all: &BTreeMap<&str, &'a Notice>,
        visiting: &mut BTreeSet<String>,
        visited: &mut BTreeSet<String>,
        out: &mut Vec<&'a Notice>,
    ) -> Result<()> {
        ensure!(!visiting.contains(&n.number), "Cyclic NANU references");
        if visited.contains(&n.number) {
            return Ok(());
        }
        visiting.insert(n.number.clone());
        if let Some(parent) = n.reference.as_deref().and_then(|r| all.get(r)) {
            visit(parent, all, visiting, visited, out)?;
        }
        visiting.remove(&n.number);
        visited.insert(n.number.clone());
        out.push(n);
        Ok(())
    }
    let mut ordered = Vec::new();
    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    for n in known {
        visit(n, &by_number, &mut visiting, &mut visited, &mut ordered)?;
    }
    let mut roots: BTreeMap<String, Outage> = BTreeMap::new();
    let mut aliases: BTreeMap<String, String> = BTreeMap::new();
    let mut reviews = Vec::new();
    for n in ordered {
        if n.review_required {
            reviews.push(n.clone());
        }
        let root = n
            .reference
            .as_ref()
            .map(|r| aliases.get(r).unwrap_or(r).clone())
            .unwrap_or(n.number.clone());
        let previous = roots.get(&root).cloned();
        ensure!(
            previous
                .as_ref()
                .and_then(|p| p.svn)
                .zip(n.svn)
                .is_none_or(|(a, b)| a == b),
            "NANU reference crosses spacecraft identities"
        );
        aliases.insert(n.number.clone(), root.clone());
        if ["GENERAL", "LAUNCH", "LEAPSEC"].contains(&n.kind.as_str()) {
            continue;
        }
        if ["FCSTCANC", "FCSTRESCD"].contains(&n.kind.as_str()) {
            ensure!(
                n.reference.is_some(),
                "Cancellation/reschedule lacks a reference"
            );
            if n.kind == "FCSTCANC" {
                roots.remove(&root);
                continue;
            }
        }
        if n.kind == "USABINIT" {
            let prn = n.prn.context("USABINIT lacks PRN")?;
            let start = n.start_gps.context("USABINIT lacks start")?;
            for outage in roots.values_mut() {
                if outage.prn == prn && outage.stop_gps.is_none() && outage.start_gps < start {
                    outage.stop_gps = Some(start);
                }
            }
            continue;
        }
        let prn = n
            .prn
            .or(previous.as_ref().map(|p| p.prn))
            .context("Outage lacks a resolvable PRN")?;
        let start = n
            .start_gps
            .or(previous.as_ref().map(|p| p.start_gps))
            .context("Outage lacks a resolvable start")?;
        if !(1..=32).contains(&prn) {
            continue;
        }
        let stop = if ["FCSTEXTD", "FCSTUUFN", "UNUSUFN", "DECOM"].contains(&n.kind.as_str()) {
            None
        } else {
            n.stop_gps
        };
        ensure!(
            stop.is_none_or(|s| s >= start),
            "Resolved outage stop precedes start"
        );
        roots.insert(
            root.clone(),
            Outage {
                prn,
                svn: n.svn.or(previous.and_then(|p| p.svn)),
                start_gps: start,
                stop_gps: stop,
                kind: n.kind.clone(),
                number: n.number.clone(),
                root_notice: root,
                issued_gps: n.issued_gps,
            },
        );
    }
    Ok((roots.into_values().collect(), reviews))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub prn: usize,
    pub first_gps: f64,
    pub last_gps: f64,
    pub peak_innovation_m: f64,
    pub samples: usize,
    pub reason: String,
}
pub fn detect(samples: &[Sample], frames: &Frames, as_of: f64) -> Result<Vec<Event>> {
    let samples: Vec<_> = samples.iter().filter(|s| s.time <= as_of).collect();
    ensure!(
        samples.windows(2).all(|w| w[0].time < w[1].time),
        "Orbit samples must be ordered and unique"
    );
    let inertial: Vec<_> = samples
        .iter()
        .map(|s| -> Result<_> {
            let r = frames.matrix(s.time)?.transpose();
            Ok(s.positions
                .iter()
                .map(|p| p.map(|p| r * V3::from(p)))
                .collect::<Vec<_>>())
        })
        .collect::<Result<_>>()?;
    let weights = [1., -9., 36., -84., 126., -126., 84., -36., 9.];
    let mut events: Vec<Event> = Vec::new();
    for prn in 1..=32 {
        let mut active: Option<usize> = None;
        for i in 9..samples.len() {
            if !samples[i - 9..=i]
                .windows(2)
                .all(|w| w[1].time - w[0].time == 900.)
            {
                continue;
            }
            let Some(actual) = inertial[i][prn - 1] else {
                continue;
            };
            let predicted = (0..9).try_fold(V3::zeros(), |acc, k| {
                inertial[i - 9 + k][prn - 1].map(|v| acc + weights[k] * v)
            });
            let Some(predicted) = predicted else { continue };
            let innovation = (actual - predicted).norm();
            if innovation <= 25. {
                continue;
            }
            let when = samples[i].time;
            if let Some(index) = active.filter(|&k| when - events[k].last_gps <= 10800.) {
                events[index].last_gps = when;
                events[index].peak_innovation_m = events[index].peak_innovation_m.max(innovation);
                events[index].samples += 1;
            } else {
                active = Some(events.len());
                events.push(Event {
                    prn,
                    first_gps: when,
                    last_gps: when,
                    peak_innovation_m: innovation,
                    samples: 1,
                    reason: "observed orbit discontinuity or product anomaly".into(),
                });
            }
        }
    }
    Ok(events)
}
pub fn merge_events(old: &[Event], new: &[Event]) -> Vec<Event> {
    let mut all: Vec<_> = old.iter().chain(new).cloned().collect();
    all.sort_by(|a, b| a.prn.cmp(&b.prn).then(a.first_gps.total_cmp(&b.first_gps)));
    let mut out: Vec<Event> = Vec::new();
    for e in all {
        if let Some(p) = out
            .last_mut()
            .filter(|p| p.prn == e.prn && e.first_gps <= p.last_gps + 10800.)
        {
            p.last_gps = p.last_gps.max(e.last_gps);
            p.peak_innovation_m = p.peak_innovation_m.max(e.peak_innovation_m);
            p.samples = p.samples.max(e.samples);
        } else {
            out.push(e);
        }
    }
    out
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub schema_version: u32,
    pub refresh_ok: bool,
    pub checked_at_gps: f64,
    pub latest_observed_gps: f64,
    pub latest_observed_by_prn: BTreeMap<String, f64>,
    pub notices: Vec<Notice>,
    pub orbit_events: Vec<Event>,
    #[serde(default)]
    pub refresh_error: Option<String>,
    #[serde(default)]
    pub latest_nanu: String,
}
impl Snapshot {
    pub fn failures(&self, now: f64) -> Vec<String> {
        let mut failures = Vec::new();
        if self.schema_version != 1 {
            failures.push("Unsupported health snapshot version".into());
        }
        if !self.refresh_ok {
            failures.push("Official health refresh failed".into());
        }
        if !(0. ..=21600.).contains(&(now - self.checked_at_gps)) {
            failures.push("Notice snapshot is stale or future dated".into());
        }
        if !(0. ..=36. * 3600.).contains(&(now - self.latest_observed_gps)) {
            failures.push("Observed orbit data are stale or future dated".into());
        }
        failures
    }
}

pub fn guard(bytes: &[u8], training: f64, state: &Snapshot, now: f64) -> Result<(Vec<u8>, Value)> {
    validation::validate(bytes)?;
    let start = validation::start(bytes)?;
    ensure!(
        training.is_finite() && training < start && now.is_finite(),
        "Training must precede forecast epoch; check time must be finite"
    );
    let mut failures = state.failures(now);
    if !(start..start + 2. * validation::WEEK).contains(&now) {
        failures.push("Predictions are outside their two-week window".into());
    }
    let (outages, reviews) = match resolve(&state.notices, now) {
        Ok(r) => r,
        Err(e) => {
            failures.push(format!("Health policy could not be resolved: {e:#}"));
            (Vec::new(), Vec::new())
        }
    };
    let mut flags = Vec::new();
    let mut removals = Vec::new();
    let mut weeks = Vec::new();
    for week in 0..4 {
        let end = start + (week + 1) as f64 * validation::WEEK;
        let mut records = Vec::new();
        for prn in 1..=32 {
            let pos = week * validation::BLOCK + 6 + (prn - 1) * validation::RECORD;
            let mut record = bytes[pos..pos + validation::RECORD].to_vec();
            let fresh = state
                .latest_observed_by_prn
                .get(&prn.to_string())
                .is_some_and(|t| (0. ..=36. * 3600.).contains(&(now - t)));
            let mut unsafe_record = false;
            for e in &outages {
                if e.prn == prn
                    && e.stop_gps.is_none_or(|stop| training <= stop)
                    && end > e.start_gps
                {
                    unsafe_record = true;
                    {
                        let mut flag = serde_json::to_value(e)?;
                        flag["reason"] = json!("Official outage intersects or follows fitted arc");
                        flags.push(flag);
                    }
                }
            }
            for e in &state.orbit_events {
                if e.prn == prn && e.first_gps <= now && e.last_gps >= training && end > e.first_gps
                {
                    unsafe_record = true;
                    {
                        let mut flag = serde_json::to_value(e)?;
                        flag["reason"] = json!("Observed discontinuity requires a new clean fit");
                        flags.push(flag);
                    }
                }
            }
            for n in &reviews {
                if training <= n.issued_gps
                    && n.issued_gps <= now
                    && (n.mentioned_prns.is_empty() || n.mentioned_prns.contains(&prn))
                {
                    unsafe_record = true;
                    {
                        flags.push(json!({"prn":prn,"number":n.number,"reason":"Recent unstructured advisory needs interpretation"}));
                    }
                }
            }
            if record[1] == 0 && (!failures.is_empty() || !fresh || unsafe_record) {
                record = cep::unavailable();
                removals.push(json!({"week":week+1,"prn":prn,"stale_observations":!fresh}));
            }
            records.push(record);
        }
        weeks.push(records);
    }
    let mut unique = BTreeSet::new();
    flags.retain(|flag| unique.insert(flag.to_string()));
    let available: Vec<Vec<_>> = weeks
        .iter()
        .map(|w| {
            w.iter()
                .enumerate()
                .filter(|(_, r)| r[1] == 0)
                .map(|(i, _)| i + 1)
                .collect()
        })
        .collect();
    let selected = ((now - start) / validation::WEEK).floor() as isize;
    if (0..2).contains(&selected) && available[selected as usize].len() < 4 {
        failures.push("Fewer than four usable satellites in the current prediction week".into());
    }
    let allowed = failures.is_empty()
        && (0..2).contains(&selected)
        && available[selected as usize].len() >= 4;
    let guarded = cep::archive(start, &weeks)?;
    Ok((
        guarded,
        json!({"upload_allowed":allowed,"checked_at_gps":now,"failures":failures,"removed_records":removals,"available_prns_by_week":available,"quality_flags":flags,"rule":"Removal only; recovery requires a new fit after the event/outage"}),
    ))
}
