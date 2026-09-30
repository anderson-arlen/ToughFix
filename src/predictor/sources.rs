// SPDX-License-Identifier: MIT
use super::{
    data,
    health::{self, Notice, Snapshot},
    physics,
};
use crate::files;
use anyhow::{Context, Result, ensure};
use chrono::{Datelike, Duration as Days, Utc};
use quick_xml::{Reader, events::Event};
use rayon::prelude::*;
use regex::Regex;
use reqwest::{StatusCode, blocking::Client, redirect::Policy};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

const NAVCEN: &str = "https://www.navcen.uscg.gov/sites/default/files/gps/";
const NOAA: &str = "https://noaa-cors-pds.s3.amazonaws.com/";
const EOP: &str = "https://maia.usno.navy.mil/ser7/finals2000A.all";
const GRAVITY: &str = "https://earth-info.nga.mil/php/download.php?file=egm-96spherical";
const GRAVITY_HASH: &str = "1f21ab8151c1b9fe25f483a4f6b78acdbf5306daf923725017b83d87a5f33472";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Download {
    pub file: String,
    pub url: String,
    pub bytes: usize,
    pub sha256: String,
    pub retrieved_at_gps: f64,
    pub known_at_gps: f64,
    pub http_last_modified: Option<String>,
}
pub struct Sources {
    root: PathBuf,
    client: Client,
    manifest: BTreeMap<String, Download>,
}
impl Sources {
    pub fn new(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)?;
        let client = Client::builder()
            .https_only(true)
            .user_agent("ToughFix/0.1 (open-source GPS assistance)")
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .redirect(Policy::limited(5))
            .build()?;
        let manifest = fs::read(root.join("manifest.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Ok(Self {
            root: root.into(),
            client,
            manifest,
        })
    }
    fn fetch(&self, url: &str, file: &str, missing_ok: bool) -> Result<Option<Download>> {
        ensure!(
            Path::new(file).is_relative()
                && Path::new(file)
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_))),
            "Invalid source cache path"
        );
        let mut last = None;
        for _ in 0..2 {
            match self.fetch_once(url, file, missing_ok) {
                Ok(v) => return Ok(v),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap())
    }
    fn fetch_once(&self, url: &str, file: &str, missing_ok: bool) -> Result<Option<Download>> {
        let mut response = self
            .client
            .get(url)
            .send()
            .with_context(|| format!("Downloading {url}"))?;
        if missing_ok && response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        ensure!(
            response.status() == StatusCode::OK,
            "Source {url} returned {}",
            response.status()
        );
        ensure!(
            response
                .content_length()
                .is_none_or(|n| n <= 16 * 1024 * 1024),
            "Oversized source response"
        );
        let modified = response
            .headers()
            .get(reqwest::header::LAST_MODIFIED)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let mut bytes = Vec::new();
        Read::by_ref(&mut response)
            .take(16 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            !bytes.is_empty() && bytes.len() <= 16 * 1024 * 1024,
            "Empty or oversized source response"
        );
        let checked = health::now()?;
        let known = modified
            .as_ref()
            .and_then(|s| chrono::DateTime::parse_from_rfc2822(s).ok())
            .and_then(|d| health::utc_gps(d.with_timezone(&Utc)).ok())
            .map_or(checked, |t| checked.min(t));
        let meta = Download {
            file: file.into(),
            url: url.into(),
            bytes: bytes.len(),
            sha256: crate::camera::hash(&bytes),
            retrieved_at_gps: checked,
            known_at_gps: known,
            http_last_modified: modified,
        };
        files::atomic(&self.root.join(file), &bytes)?;
        Ok(Some(meta))
    }
    fn cached(&self, key: &str, max_age: f64, now: f64) -> Option<Download> {
        let m = self.manifest.get(key)?;
        // Cache entries are evidence, not authority to read paths outside this cache.
        if !Path::new(&m.file).is_relative()
            || !Path::new(&m.file)
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_)))
            || !(0. ..max_age).contains(&(now - m.retrieved_at_gps))
        {
            return None;
        }
        let bytes = fs::read(self.root.join(&m.file)).ok()?;
        (bytes.len() == m.bytes && crate::camera::hash(&bytes) == m.sha256).then(|| m.clone())
    }
    fn required(&mut self, key: &str, url: &str, file: &str) -> Result<Download> {
        let m = self
            .fetch(url, file, false)?
            .context("Missing required official source")?;
        self.manifest.insert(key.into(), m.clone());
        Ok(m)
    }
    pub fn refresh(&mut self, progress: &(impl Fn(String) + Sync)) -> Result<Snapshot> {
        let previous: Option<Snapshot> = fs::read(self.root.join("health-state.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        let result = self.refresh_inner(previous.as_ref(), progress);
        // Save completed downloads on failure too, but revoke eligibility explicitly.
        files::json(&self.root.join("manifest.json"), &self.manifest)?;
        match result {
            Ok(state) => {
                files::json(&self.root.join("health-state.json"), &state)?;
                Ok(state)
            }
            Err(e) => {
                let state = Snapshot {
                    schema_version: 1,
                    refresh_ok: false,
                    checked_at_gps: health::now()?,
                    latest_observed_gps: previous.as_ref().map_or(0., |s| s.latest_observed_gps),
                    latest_observed_by_prn: previous
                        .as_ref()
                        .map(|s| s.latest_observed_by_prn.clone())
                        .unwrap_or_default(),
                    notices: previous
                        .as_ref()
                        .map(|s| s.notices.clone())
                        .unwrap_or_default(),
                    orbit_events: previous.map(|s| s.orbit_events).unwrap_or_default(),
                    refresh_error: Some(format!("{e:#}")),
                    latest_nanu: String::new(),
                };
                files::json(&self.root.join("health-state.json"), &state)?;
                Err(e)
            }
        }
    }
    fn refresh_inner(
        &mut self,
        previous: Option<&Snapshot>,
        progress: &(impl Fn(String) + Sync),
    ) -> Result<Snapshot> {
        let checked = health::now()?;
        progress("Checking Coast Guard satellite notices".into());
        let latest = self.required(
            "current_nanu",
            &format!("{NAVCEN}nanu/current_nanu.nnu"),
            "current_nanu.nnu",
        )?;
        let latest_notice = health::parse_notice(
            &fs::read_to_string(self.root.join(&latest.file))?,
            latest.known_at_gps,
        )?;
        let advisory = self.required(
            "operational_advisory",
            &format!("{NAVCEN}opsadvisory/current_opsadvisory.oa1"),
            "current_opsadvisory.oa1",
        )?;
        let advisory = fs::read_to_string(self.root.join(advisory.file))?;
        let date = Regex::new(r"SUBJ:\s*GPS STATUS\s+(\d{1,2}\s+\w{3}\s+\d{4})")?
            .captures(&advisory)
            .context("Operational advisory lacks a date")?[1]
            .to_string();
        let date = chrono::NaiveDate::parse_from_str(&date, "%d %b %Y")?
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        ensure!(
            (-86400. ..=48. * 3600.).contains(&(checked - health::utc_gps(date)?)),
            "Operational advisory is stale or future dated"
        );
        let number_pattern = Regex::new(r"^\d{7}$")?;
        let year: &str = &latest_notice.number[..4];
        let last: usize = latest_notice.number[4..].parse()?;
        ensure!(last > 0 && last < 1000, "Invalid NANU archive sequence");
        let mut numbers: BTreeSet<String> = (1..=last).map(|n| format!("{year}{n:03}")).collect();
        numbers.extend(
            self.manifest
                .keys()
                .filter(|s| number_pattern.is_match(s))
                .cloned(),
        );
        // Include structured operational entries, including prior-year active outages.
        let section = advisory.split("C. GENERAL:").next().unwrap_or(&advisory);
        numbers.extend(
            Regex::new(r"(?m)^(\d{7})\s")?
                .captures_iter(section)
                .map(|c| c[1].to_owned()),
        );
        let file = format!("notices/{}.nnu", latest_notice.number);
        files::atomic(
            &self.root.join(&file),
            &fs::read(self.root.join(&latest.file))?,
        )?;
        self.manifest
            .insert(latest_notice.number.clone(), Download { file, ..latest });
        let mut seen = BTreeSet::new();
        let mut notices: Vec<Notice> = Vec::new();
        let pool = rayon::ThreadPoolBuilder::new().num_threads(4).build()?;
        loop {
            let todo: Vec<_> = numbers.difference(&seen).cloned().collect();
            if todo.is_empty() {
                break;
            }
            ensure!(
                numbers.len() <= 2000,
                "NANU reference chain is unexpectedly large"
            );
            let failed = AtomicBool::new(false);
            let results: Vec<_> = pool.install(|| {
                todo.par_iter()
                    .filter_map(|n| {
                        if failed.load(Ordering::Relaxed) {
                            return None;
                        }
                        let result = (|| -> Result<_> {
                            let file = format!("notices/{n}.nnu");
                            let url = format!("{NAVCEN}nanu/{}/{n}.nnu", &n[..4]);
                            let m = match self.cached(n, 86400., checked) {
                                Some(m) => m,
                                None => self.fetch(&url, &file, false)?.context("Missing NANU")?,
                            };
                            let notice = health::parse_notice(
                                &fs::read_to_string(self.root.join(&m.file))?,
                                m.known_at_gps,
                            )?;
                            ensure!(notice.number == *n, "Archive notice number mismatch");
                            Ok((n.clone(), m, notice))
                        })();
                        if result.is_err() {
                            failed.store(true, Ordering::Relaxed);
                        }
                        Some(result)
                    })
                    .collect()
            });
            // Retain successful requests even if a sibling request fails.
            let mut error = None;
            for result in results {
                match result {
                    Ok((n, m, notice)) => {
                        self.manifest.insert(n.clone(), m);
                        seen.insert(n);
                        if let Some(r) = &notice.reference {
                            numbers.insert(r.clone());
                        }
                        notices.push(notice);
                    }
                    Err(e) => error = Some(e),
                }
            }
            if let Some(e) = error {
                return Err(e);
            }
            progress(format!("Checked {} satellite notices", seen.len()));
        }
        health::resolve(&notices, checked)?;
        progress("Downloading NOAA observed GPS orbits".into());
        let today = Utc::now().date_naive();
        let jobs: Vec<_> = (0..7)
            .map(|offset| {
                let d = today - Days::days(offset);
                let name = format!(
                    "IGS0OPSRAP_{}{:03}0000_01D_15M_ORB.SP3.gz",
                    d.year(),
                    d.ordinal()
                );
                let url = format!("{NOAA}rinex/{}/{:03}/{name}", d.year(), d.ordinal());
                (name, url)
            })
            .collect();
        let results: Vec<_> = pool.install(|| {
            jobs.par_iter()
                .map(|(name, url)| self.fetch(url, name, true))
                .collect()
        });
        for r in results {
            if let Some(m) = r? {
                self.manifest.insert(m.file.clone(), m);
            }
        }
        let mut ultra = Vec::new();
        for offset in 0..3 {
            let d = today - Days::days(offset);
            let prefix = format!("rinex/{}/{:03}/igu", d.year(), d.ordinal());
            let index = self.required(
                &format!("ultra-index-{offset}"),
                &format!("{NOAA}?list-type=2&prefix={prefix}"),
                &format!("ultra-index-{offset}.xml"),
            )?;
            for key in listing_keys(&fs::read(self.root.join(index.file))?)? {
                ensure!(key.starts_with(&prefix), "Unexpected NOAA archive prefix");
                if key.ends_with(".sp3.gz") {
                    ultra.push((ultra_epoch(&key)?, key));
                }
            }
        }
        // The newest file contains only 24 hours of observations. Download the
        // preceding products too so an older Rapid cutoff cannot leave a gap.
        for (_, key) in fitting_ultra(ultra, checked) {
            let file = Path::new(&key)
                .file_name()
                .unwrap()
                .to_str()
                .context("Invalid archive filename")?
                .to_string();
            if self.cached(&file, 4. * data::DAY, checked).is_none() {
                self.required(&file, &format!("{NOAA}{key}"), &file)?;
            }
        }
        progress("Refreshing Earth orientation and gravity inputs".into());
        if self
            .cached("earth_orientation", 86400., checked)
            .filter(|m| m.file == "finals2000A.all")
            .is_none()
        {
            self.required("earth_orientation", EOP, "finals2000A.all")?;
        }
        let gravity_ok = fs::read(self.root.join("egm96.zip"))
            .is_ok_and(|b| crate::camera::hash(&b) == GRAVITY_HASH);
        if !gravity_ok {
            self.required("gravity", GRAVITY, "egm96.zip")?;
            ensure!(
                crate::camera::hash(&fs::read(self.root.join("egm96.zip"))?) == GRAVITY_HASH,
                "Official EGM96 differs from the pinned model"
            );
        }
        physics::Gravity::read(&self.root.join("egm96.zip"))?;
        let observed = data::observations_with_ultra(&self.root, true)?;
        let samples: Vec<_> = observed
            .samples
            .into_iter()
            .filter(|s| {
                s.time <= checked
                    && s.time >= checked - 6. * data::DAY
                    && s.positions.iter().any(Option::is_some)
            })
            .collect();
        let latest = samples
            .last()
            .context("No recent observed GPS orbit samples")?
            .time;
        let frames = data::Frames::read(&self.root.join("finals2000A.all"))?;
        progress("Checking observed orbit discontinuities".into());
        let events = health::detect(&samples, &frames, checked)?;
        let mut by_prn = BTreeMap::new();
        for s in &samples {
            for (i, p) in s.positions.iter().enumerate() {
                if p.is_some() {
                    by_prn.insert((i + 1).to_string(), s.time);
                }
            }
        }
        let state = Snapshot {
            schema_version: 1,
            refresh_ok: true,
            checked_at_gps: checked,
            latest_observed_gps: latest,
            latest_observed_by_prn: by_prn,
            notices,
            orbit_events: health::merge_events(
                previous
                    .map(|s| s.orbit_events.as_slice())
                    .unwrap_or_default(),
                &events,
            ),
            refresh_error: None,
            latest_nanu: latest_notice.number,
        };
        let failures = state.failures(health::now()?);
        ensure!(failures.is_empty(), "{}", failures.join("; "));
        Ok(state)
    }
}

fn fitting_ultra(mut products: Vec<(i64, String)>, now: f64) -> Vec<(i64, String)> {
    products.retain(|(end, _)| *end as f64 <= now);
    products.sort();
    products.dedup();
    let Some(latest) = products.last().map(|p| p.0) else {
        return Vec::new();
    };
    products
        .into_iter()
        .filter(|(end, _)| *end > latest - 3 * 86400)
        .collect()
}

fn listing_keys(bytes: &[u8]) -> Result<Vec<String>> {
    let mut reader = Reader::from_reader(bytes);
    let mut keys = Vec::new();
    let mut in_key = false;
    let mut truncated = false;
    let mut root = false;
    loop {
        match reader.read_event()? {
            Event::Start(e) => {
                if !root {
                    ensure!(
                        e.local_name().as_ref() == b"ListBucketResult",
                        "Unexpected NOAA listing format"
                    );
                    root = true;
                }
                in_key = e.local_name().as_ref() == b"Key";
                if e.local_name().as_ref() == b"IsTruncated" {
                    truncated = true
                }
            }
            Event::Text(t) if in_key => {
                keys.push(quick_xml::escape::unescape(&t.decode()?)?.into_owned())
            }
            Event::Text(t) if truncated => {
                ensure!(
                    t.decode()?.trim() == "false",
                    "NOAA archive listing is truncated"
                );
                truncated = false
            }
            Event::End(_) => in_key = false,
            Event::Eof => break,
            _ => (),
        }
    }
    ensure!(root, "Empty NOAA listing");
    Ok(keys)
}
pub fn ultra_epoch(key: &str) -> Result<i64> {
    let name = Path::new(key)
        .file_name()
        .and_then(|s| s.to_str())
        .context("Invalid ultra-rapid filename")?;
    let regex = Regex::new(r"^igu(\d{4,5})([0-6])_(00|06|12|18)\.sp3\.gz$")?;
    let c = regex
        .captures(name)
        .context("Unexpected ultra-rapid filename")?;
    let week: i64 = c[1].parse()?;
    let day: i64 = c[2].parse()?;
    let hour: i64 = c[3].parse()?;
    Ok((week * 7 + day) * 86400 + hour * 3600)
}

#[cfg(test)]
mod tests {
    #[test]
    fn ultra_download_window_covers_full_fit_and_excludes_future_products() {
        let products: Vec<_> = (0..=16).map(|i| (i * 21600, format!("{i}"))).collect();
        let selected = fitting_ultra(products, 15. * 21600. + 1.);
        assert_eq!(selected.first().unwrap().0, 4 * 21600);
        assert_eq!(selected.last().unwrap().0, 15 * 21600);
        assert_eq!(selected.len(), 12);
        assert!(selected.first().unwrap().0 - 86400 <= selected.last().unwrap().0 - 3 * 86400);
    }

    use super::*;
    #[test]
    fn ultra_selection_uses_product_epoch_and_rejects_malformed_names() {
        assert!(
            ultra_epoch("rinex/2026/273/igu24383_06.sp3.gz").unwrap()
                > ultra_epoch("igu24382_18.sp3.gz").unwrap()
        );
        for bad in [
            "igu24387_06.sp3.gz",
            "igu24383_07.sp3.gz",
            "../not-an-orbit.gz",
        ] {
            assert!(ultra_epoch(bad).is_err());
        }
        let keys=listing_keys(b"<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Key>rinex/2026/273/igu24383_06.sp3.gz</Key></Contents></ListBucketResult>").unwrap();
        assert_eq!(keys, ["rinex/2026/273/igu24383_06.sp3.gz"]);
        assert!(
            listing_keys(b"<ListBucketResult><IsTruncated>true</IsTruncated></ListBucketResult>")
                .is_err()
        );
        assert!(listing_keys(b"<ListBucketResult><Contents></ListBucketResult>").is_err());
    }
}
