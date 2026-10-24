// SPDX-License-Identifier: MIT
use super::{
    Progress, RefreshStage, data,
    health::{self, Notice, Snapshot},
    physics,
};
use crate::files;
use anyhow::{Context, Result, ensure};
use chrono::{Datelike, Duration as Days, NaiveDate, Utc};
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
    sync::Mutex,
    time::{Duration, Instant},
};

const NAVCEN: &str = "https://www.navcen.uscg.gov/sites/default/files/gps/";
const CELESTRAK: &str = "https://celestrak.org/GPS/";
const NOAA: &str = "https://noaa-cors-pds.s3.amazonaws.com/";
const EOP: &str = "https://datacenter.iers.org/products/eop/rapid/standard/finals2000A.all";
const EOP_BACKUP: &str = "https://maia.usno.navy.mil/ser7/finals2000A.all";
// Astropy redistributes the original IERS table, independently of these hosts.
const EOP_MIRROR: &str = "https://raw.githubusercontent.com/astropy/astropy-iers-data/main/astropy_iers_data/data/finals2000A.all";
const EOP_FILE: &str = "finals2000A.all";
const GRAVITY: &str = "https://earth-info.nga.mil/php/download.php?file=egm-96spherical";
const GRAVITY_HASH: &str = "1f21ab8151c1b9fe25f483a4f6b78acdbf5306daf923725017b83d87a5f33472";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NoticeSource {
    Navcen,
    Celestrak,
}
impl NoticeSource {
    fn name(self) -> &'static str {
        match self {
            Self::Navcen => "US Coast Guard NAVCEN",
            Self::Celestrak => "CelesTrak mirror",
        }
    }
    fn notice_url(self, number: &str) -> String {
        match self {
            Self::Navcen => format!("{NAVCEN}nanu/{}/{number}.nnu", &number[..4]),
            Self::Celestrak => format!("{CELESTRAK}NANU/{}/nanu.{number}.txt", &number[..4]),
        }
    }
}

struct Document {
    meta: Download,
    text: String,
}
struct NoticeInputs {
    source: NoticeSource,
    latest: Document,
    notice: Notice,
    advisory: String,
    checked_at_gps: f64,
    next_check_at_gps: Option<f64>,
}
type DocumentFetcher<'a> = dyn FnMut(&str, &str, &str, bool) -> Result<Option<Document>> + 'a;

fn required_document(
    fetch: &mut DocumentFetcher<'_>,
    key: &str,
    url: &str,
    file: &str,
) -> Result<Document> {
    fetch(key, url, file, false)?.context("Missing required satellite-health source")
}

fn advisory_date(text: &str, checked: f64) -> Result<NaiveDate> {
    ensure!(
        text.trim_start().starts_with("GPS OPERATIONAL ADVISORY"),
        "Unexpected operational advisory format"
    );
    let date = Regex::new(r"SUBJ:\s*GPS STATUS\s+(\d{1,2}\s+\w{3}\s+\d{4})")?
        .captures(text)
        .context("Operational advisory lacks a date")?[1]
        .to_string();
    let date = NaiveDate::parse_from_str(&date, "%d %b %Y")?;
    ensure!(
        (-86400. ..=48. * 3600.)
            .contains(&(checked - health::utc_gps(date.and_hms_opt(0, 0, 0).unwrap().and_utc())?)),
        "Operational advisory is stale or future dated"
    );
    Ok(date)
}

fn mirror_catalog(text: &str, today: NaiveDate) -> Result<(String, String)> {
    #[derive(Deserialize)]
    struct Entry {
        #[serde(rename = "FILE_NAME")]
        file_name: String,
    }
    let entries: Vec<Entry> =
        serde_json::from_str(text).context("Invalid CelesTrak GPS directory")?;
    let nanu = Regex::new(r"^/GPS/NANU/(\d{4})/nanu\.(\d{7})\.txt$")?;
    let status = Regex::new(r"^/GPS/status/(\d{4})/status\.(\d{8})\.txt$")?;
    let mut numbers = Vec::new();
    let mut advisories = Vec::new();
    for entry in entries {
        if let Some(c) = nanu.captures(&entry.file_name) {
            let year: i32 = c[1].parse()?;
            ensure!(
                c[1] == c[2][..4] && &c[2][4..] != "000",
                "Invalid NANU directory entry"
            );
            ensure!(year <= today.year(), "Future NANU directory entry");
            if year >= today.year() - 1 {
                numbers.push(c[2].to_owned());
            }
        } else if let Some(c) = status.captures(&entry.file_name) {
            let day = NaiveDate::parse_from_str(&c[2], "%Y%m%d")?;
            ensure!(
                c[1] == c[2][..4] && day <= today,
                "Invalid advisory directory entry"
            );
            advisories.push((day, entry.file_name));
        }
    }
    let number = numbers
        .into_iter()
        .max()
        .context("CelesTrak directory lacks a recent NANU")?;
    let (_, advisory) = advisories
        .into_iter()
        .max()
        .context("CelesTrak directory lacks a GPS advisory")?;
    Ok((number, format!("https://celestrak.org{advisory}")))
}

fn notice_inputs_from(
    source: NoticeSource,
    fetch: &mut DocumentFetcher<'_>,
    checked: f64,
    today: NaiveDate,
) -> Result<NoticeInputs> {
    let catalog = if source == NoticeSource::Celestrak {
        Some(required_document(
            fetch,
            "celestrak-gps-index",
            &format!("{CELESTRAK}jsonDir.php"),
            "celestrak-gps-index.json",
        )?)
    } else {
        None
    };
    let mirror = catalog
        .as_ref()
        .map(|c| mirror_catalog(&c.text, today))
        .transpose()?;
    let latest = if source == NoticeSource::Navcen {
        required_document(
            fetch,
            "current_nanu",
            &format!("{NAVCEN}nanu/current_nanu.nnu"),
            "current_nanu.nnu",
        )?
    } else {
        let number = &mirror.as_ref().unwrap().0;
        let latest = required_document(
            fetch,
            "current_nanu",
            &source.notice_url(number),
            &format!("notices/{number}.nnu"),
        )?;
        ensure!(
            health::parse_notice(&latest.text, latest.meta.known_at_gps)?.number == *number,
            "Mirror's latest NANU number does not match its index"
        );
        latest
    };
    let notice = health::parse_notice(&latest.text, latest.meta.known_at_gps)?;
    let advisory = if source == NoticeSource::Navcen {
        required_document(
            fetch,
            "operational_advisory",
            &format!("{NAVCEN}opsadvisory/current_opsadvisory.oa1"),
            "current_opsadvisory.oa1",
        )?
        .text
    } else {
        let url = &mirror.as_ref().unwrap().1;
        let name = url.rsplit('/').next().unwrap();
        let file = required_document(
            fetch,
            "operational_advisory",
            url,
            &format!("advisories/{name}"),
        )?;
        ensure!(
            advisory_date(&file.text, checked)?
                == NaiveDate::parse_from_str(&name[7..15], "%Y%m%d")?,
            "Mirror advisory date does not match its filename"
        );
        file.text
    };
    advisory_date(&advisory, checked)?;
    ensure!(
        Regex::new(r"(?m)^(\d{7})\s")?
            .captures_iter(&advisory)
            .all(|c| c[1] <= *notice.number),
        "Latest NANU index is behind the operational advisory"
    );
    let checked_at_gps = catalog
        .as_ref()
        .map_or(checked, |c| c.meta.retrieved_at_gps);
    // document() checks cache age at read time. A new response can legitimately
    // postdate the start of this refresh; keep that actual retrieval timestamp.
    Ok(NoticeInputs {
        source,
        latest,
        notice,
        advisory,
        checked_at_gps,
        next_check_at_gps: catalog.map(|c| c.meta.retrieved_at_gps + 3600.),
    })
}

fn notice_inputs(
    fetch: &mut DocumentFetcher<'_>,
    checked: f64,
    today: NaiveDate,
    progress: &(impl Fn(Progress) + Sync),
) -> Result<NoticeInputs> {
    let mut errors = Vec::new();
    for source in [NoticeSource::Navcen, NoticeSource::Celestrak] {
        progress(Progress::new(
            RefreshStage::SatelliteHealth,
            format!("Checking satellite notices from {}", source.name()),
        ));
        match notice_inputs_from(source, fetch, checked, today) {
            Ok(inputs) => return Ok(inputs),
            Err(error) => {
                eprintln!(
                    "ToughFix: {} health source failed: {error:#}",
                    source.name()
                );
                errors.push(format!("{}: {error:#}", source.name()));
            }
        }
    }
    anyhow::bail!("Satellite-health downloads failed ({})", errors.join("; "))
}

fn earth_orientation(
    mut download: impl FnMut(&str) -> Result<Download>,
    cached: Option<Download>,
    progress: &(impl Fn(Progress) + Sync),
) -> Result<Download> {
    let mut errors = Vec::new();
    for (source, url) in [("IERS", EOP), ("USNO", EOP_BACKUP), ("Astropy", EOP_MIRROR)] {
        progress(Progress::new(
            RefreshStage::ModelInputs,
            format!("Downloading Earth orientation from {source}"),
        ));
        match download(url) {
            Ok(file) => return Ok(file),
            Err(e) => errors.push(format!("{source}: {e:#}")),
        }
    }
    if let Some(cached) = cached {
        // Preserve the original source, hash and timestamps. A failed request
        // does not make cached data newly observed or newly downloaded.
        eprintln!(
            "ToughFix: using verified Earth-orientation cache: {}",
            errors.join("; ")
        );
        progress(Progress::new(
            RefreshStage::ModelInputs,
            "Using verified cached Earth orientation while downloads are unavailable",
        ));
        return Ok(cached);
    }
    anyhow::bail!("Earth-orientation downloads failed ({})", errors.join("; "))
}

fn validate_earth_orientation(bytes: &[u8], checked: f64) -> Result<()> {
    let text = std::str::from_utf8(bytes).context("Invalid Earth-orientation encoding")?;
    let frames = data::Frames::parse(text)?;
    // Cover the historical fitting/validation arc and the full GPS forecast.
    frames.matrix(checked - 32. * data::DAY)?;
    frames.matrix(checked + 15. * data::DAY)?;
    // Column 17 and 58 identify measured polar motion and UT1, respectively.
    // Bound prediction age independently of HTTP dates or download timestamps.
    // Astropy's IERS auto-refresh uses the same 30-day default age limit:
    // https://docs.astropy.org/en/stable/utils/iers.html#auto-refresh-behavior
    let latest = text
        .lines()
        .rev()
        .find_map(|line| {
            (line.is_ascii() && line.len() >= 68 && &line[16..17] == "I" && &line[57..58] == "I")
                .then(|| line[7..15].trim().parse::<f64>().ok())
                .flatten()
                .filter(|mjd| mjd.is_finite())
        })
        .context("Earth orientation lacks measured polar motion and UT1")?;
    let epoch = chrono::DateTime::from_timestamp(((latest - 40587.) * data::DAY).round() as i64, 0)
        .context("Invalid Earth-orientation observation epoch")?;
    ensure!(
        (0. ..=30. * data::DAY).contains(&(checked - health::utc_gps(epoch)?)),
        "Earth-orientation observations are stale or future dated"
    );
    Ok(())
}

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
#[derive(Debug)]
struct HttpFailure {
    url: String,
    status: StatusCode,
}
impl std::fmt::Display for HttpFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Source {} returned {}", self.url, self.status)
    }
}
impl std::error::Error for HttpFailure {}

#[derive(Clone, Serialize, Deserialize)]
struct DownloadBackoff {
    until_gps: f64,
    message: String,
}
pub struct Sources {
    root: PathBuf,
    client: Client,
    manifest: BTreeMap<String, Download>,
    celestrak_backoff: Mutex<Option<DownloadBackoff>>,
    next_check_at_gps: Option<f64>,
}
impl Sources {
    pub fn new(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)?;
        let client = Client::builder()
            .https_only(true)
            // Use the configured DNS servers asynchronously. System getaddrinfo
            // can stall past the connection timeout on some Linux DNS setups.
            .hickory_dns(true)
            .user_agent("ToughFix/0.1 (open-source GPS assistance)")
            // Model inputs are multi-megabyte public-service downloads. Allow
            // time for DNS retries and slow connection setup as well as transfer.
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(90))
            .redirect(Policy::custom(|attempt| {
                if attempt
                    .previous()
                    .iter()
                    .any(|u| u.host_str() == Some("celestrak.org"))
                {
                    attempt.stop()
                } else {
                    Policy::limited(5).redirect(attempt)
                }
            }))
            .build()?;
        let manifest = fs::read(root.join("manifest.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let backoff = fs::read(root.join("celestrak-backoff.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .or_else(|| {
                // Honour a refusal recorded by older versions, before this
                // persistent gate existed. Restarting must not prolong a block.
                let state: Snapshot =
                    serde_json::from_slice(&fs::read(root.join("health-state.json")).ok()?).ok()?;
                let message = state.refresh_error?;
                let refusal = message
                    .rsplit("; CelesTrak mirror: ")
                    .next()
                    .unwrap_or(&message);
                (refusal.contains("celestrak.org") && refusal.contains("403 Forbidden")).then_some(
                    DownloadBackoff {
                        until_gps: state.checked_at_gps + 7200.,
                        message,
                    },
                )
            });
        if let Some(backoff) = &backoff {
            files::json(&root.join("celestrak-backoff.json"), backoff)?;
        }
        Ok(Self {
            root: root.into(),
            client,
            manifest,
            celestrak_backoff: Mutex::new(backoff),
            next_check_at_gps: None,
        })
    }
    pub fn next_refresh_at(&self, successful: bool) -> Option<f64> {
        if successful {
            self.next_check_at_gps
        } else {
            self.celestrak_backoff
                .lock()
                .unwrap()
                .as_ref()
                .map(|b| b.until_gps)
        }
    }
    fn check_celestrak_backoff(&self, now: f64) -> Result<()> {
        if let Some(b) = self.celestrak_backoff.lock().unwrap().as_ref() {
            ensure!(
                now >= b.until_gps,
                "CelesTrak downloads paused for {} more seconds after a server refusal: {}",
                (b.until_gps - now).ceil() as u64,
                b.message
            );
        }
        Ok(())
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
        for attempt in 1..=2 {
            let started = Instant::now();
            match self.fetch_once(url, file, missing_ok) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    eprintln!(
                        "ToughFix: source download attempt {attempt}/2 failed after {:.1}s: {e:#}",
                        started.elapsed().as_secs_f64()
                    );
                    // An HTTP refusal is not a transient connection failure.
                    // Repeating it can turn a temporary refusal into an IP ban.
                    if e.downcast_ref::<HttpFailure>().is_some() {
                        return Err(e);
                    }
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap())
    }
    fn fetch_once(&self, url: &str, file: &str, missing_ok: bool) -> Result<Option<Download>> {
        let celestrak = url.starts_with(CELESTRAK);
        if celestrak {
            self.check_celestrak_backoff(health::now()?)?;
        }
        let mut response = self
            .client
            .get(url)
            .send()
            .with_context(|| format!("Downloading {url}"))?;
        if response.status() != StatusCode::OK && celestrak {
            let error = HttpFailure {
                url: url.into(),
                status: response.status(),
            };
            let backoff = DownloadBackoff {
                until_gps: health::now()? + 7200.,
                message: error.to_string(),
            };
            files::json(&self.root.join("celestrak-backoff.json"), &backoff)?;
            *self.celestrak_backoff.lock().unwrap() = Some(backoff);
            return Err(error.into());
        }
        if missing_ok && response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if response.status() != StatusCode::OK {
            return Err(HttpFailure {
                url: url.into(),
                status: response.status(),
            }
            .into());
        }
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
        if file == EOP_FILE {
            validate_earth_orientation(&bytes, checked)?;
        }
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
    fn cached_earth_orientation(&self, checked: f64) -> Option<Download> {
        let meta = self.cached("earth_orientation", f64::INFINITY, checked)?;
        if meta.file != EOP_FILE {
            return None;
        }
        let bytes = fs::read(self.root.join(EOP_FILE)).ok()?;
        validate_earth_orientation(&bytes, checked).ok()?;
        Some(meta)
    }
    fn cached_notice(&self, number: &str, checked: f64) -> Option<Download> {
        // Numbered NANUs are published once. Freshness comes from the latest
        // directory/advisory, not from downloading unchanged history each day.
        // Preserve the original provenance and verify the bytes on every read.
        self.cached(number, f64::INFINITY, checked)
    }
    fn document(
        &mut self,
        key: &str,
        url: &str,
        file: &str,
        missing_ok: bool,
    ) -> Result<Option<Document>> {
        let now = health::now()?;
        let cached = if url.starts_with(CELESTRAK) {
            self.check_celestrak_backoff(now)?;
            if key == "celestrak-gps-index" {
                self.cached(key, 3600., now).filter(|m| m.url == url)
            } else if key == "current_nanu" {
                let number = url
                    .rsplit('/')
                    .next()
                    .unwrap()
                    .strip_prefix("nanu.")
                    .and_then(|s| s.strip_suffix(".txt"))
                    .context("Invalid mirror NANU URL")?;
                self.cached_notice(number, now).or_else(|| {
                    self.cached(key, f64::INFINITY, now)
                        .filter(|m| m.url == url)
                })
            } else {
                self.cached(file, f64::INFINITY, now)
                    .filter(|m| m.url == url)
                    .or_else(|| {
                        self.cached(key, f64::INFINITY, now)
                            .filter(|m| m.url == url)
                    })
            }
        } else {
            None
        };
        let meta = match cached {
            Some(meta) => Some(meta),
            None => self.fetch(url, file, missing_ok)?,
        };
        let Some(mut meta) = meta else {
            return Ok(None);
        };
        let text = fs::read_to_string(self.root.join(&meta.file))?;
        if url.starts_with(CELESTRAK) && meta.file != file {
            // Migrate mutable legacy aliases to the dated/numbered cache path.
            files::atomic(&self.root.join(file), text.as_bytes())?;
            meta.file = file.into();
        }
        self.manifest.insert(key.into(), meta.clone());
        if url.starts_with(CELESTRAK) && key == "current_nanu" {
            let notice = health::parse_notice(&text, meta.known_at_gps)?;
            ensure!(
                url == NoticeSource::Celestrak.notice_url(&notice.number),
                "Mirror notice number mismatch"
            );
            self.manifest.insert(notice.number, meta.clone());
        }
        if url.starts_with(CELESTRAK) && key == "operational_advisory" {
            self.manifest.insert(file.into(), meta.clone());
        }
        Ok(Some(Document { meta, text }))
    }
    pub fn refresh(&mut self, progress: &(impl Fn(Progress) + Sync)) -> Result<Snapshot> {
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
        progress: &(impl Fn(Progress) + Sync),
    ) -> Result<Snapshot> {
        let checked = health::now()?;
        let inputs = notice_inputs(
            &mut |key, url, file, missing_ok| self.document(key, url, file, missing_ok),
            checked,
            Utc::now().date_naive(),
            progress,
        )?;
        self.next_check_at_gps = inputs.next_check_at_gps;
        let notice_checked = inputs.checked_at_gps;
        let source = inputs.source;
        let latest = inputs.latest.meta;
        let latest_notice = inputs.notice;
        let advisory = inputs.advisory;
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
        loop {
            let todo: Vec<_> = numbers.difference(&seen).cloned().collect();
            if todo.is_empty() {
                break;
            }
            ensure!(
                numbers.len() <= 2000,
                "NANU reference chain is unexpectedly large"
            );
            // Sequential requests let the first HTTP refusal stop the archive
            // immediately. Persist each success even if a later notice fails.
            for n in todo {
                let file = format!("notices/{n}.nnu");
                let url = source.notice_url(&n);
                let m = match self.cached_notice(&n, checked) {
                    Some(m) => m,
                    None => self.fetch(&url, &file, false)?.context("Missing NANU")?,
                };
                let notice = health::parse_notice(
                    &fs::read_to_string(self.root.join(&m.file))?,
                    m.known_at_gps,
                )?;
                ensure!(notice.number == n, "Archive notice number mismatch");
                self.manifest.insert(n.clone(), m);
                seen.insert(n);
                if let Some(r) = &notice.reference {
                    numbers.insert(r.clone());
                }
                notices.push(notice);
            }
            progress(Progress::new(
                RefreshStage::SatelliteHealth,
                format!("Checked {} satellite notices", seen.len()),
            ));
        }
        health::resolve(&notices, checked)?;
        progress(Progress::new(
            RefreshStage::Observations,
            "Downloading observed GPS orbits from NOAA / IGS",
        ));
        let today = Utc::now().date_naive();
        // Three days to fit, fourteen held-out days to validate TG-6 clocks,
        // and two days for normal orbit-product latency. Older daily products
        // are immutable cached evidence and are not downloaded each refresh.
        let jobs: Vec<_> = (0..=19)
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
        let pool = rayon::ThreadPoolBuilder::new().num_threads(4).build()?;
        let results: Vec<_> = pool.install(|| {
            jobs.par_iter()
                .enumerate()
                .map(|(offset, (name, url))| {
                    if offset >= 7
                        && let Some(cached) = self.cached(name, f64::INFINITY, checked)
                    {
                        Ok(Some(cached))
                    } else {
                        self.fetch(url, name, true)
                    }
                })
                .collect()
        });
        for r in results {
            if let Some(m) = r? {
                self.manifest.insert(m.file.clone(), m);
            }
        }
        progress(Progress::new(
            RefreshStage::Observations,
            "Looking for newer observed Ultra-rapid data",
        ));
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
        progress(Progress::new(
            RefreshStage::ModelInputs,
            "Refreshing Earth orientation and gravity inputs",
        ));
        let cached_eop = self.cached_earth_orientation(checked);
        if cached_eop
            .as_ref()
            .is_none_or(|m| checked - m.retrieved_at_gps >= data::DAY)
        {
            earth_orientation(
                |url| self.required("earth_orientation", url, EOP_FILE),
                cached_eop,
                progress,
            )?;
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
        progress(Progress::new(
            RefreshStage::SatelliteHealth,
            "Checking observations for satellite maneuvers",
        ));
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
            checked_at_gps: notice_checked,
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
    fn mirror_cache_reuses_numbered_and_dated_files_without_renewing_health_time() {
        let temp = crate::test_support::Temp::new();
        let mut source = Sources::new(temp.path()).unwrap();
        // A cache regression fails locally, never by querying the public site.
        source.client = Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:9").unwrap())
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let now = health::now().unwrap();
        let day = Utc::now().date_naive();
        let number = format!("{}002", day.year());
        let index_url = format!("{CELESTRAK}jsonDir.php");
        let notice_url = NoticeSource::Celestrak.notice_url(&number);
        let status_url = format!(
            "{CELESTRAK}status/{}/status.{}.txt",
            day.year(),
            day.format("%Y%m%d")
        );
        for (key, url, file, text, retrieved) in [
            (
                "celestrak-gps-index",
                index_url.as_str(),
                "celestrak-gps-index.json",
                catalog(day, &number),
                now - 1800.,
            ),
            (
                number.as_str(),
                notice_url.as_str(),
                "notices/old.nnu",
                notice(&number),
                now - 2. * 86400.,
            ),
            (
                "operational_advisory",
                status_url.as_str(),
                "current_opsadvisory.oa1",
                advisory(day, &number),
                now - 1800.,
            ),
        ] {
            files::atomic(&temp.path().join(file), text.as_bytes()).unwrap();
            source
                .manifest
                .insert(key.into(), document(url, file, text, retrieved).meta);
        }
        for _ in 0..2 {
            let inputs = notice_inputs_from(
                NoticeSource::Celestrak,
                &mut |key, url, file, missing| source.document(key, url, file, missing),
                now,
                day,
            )
            .unwrap();
            assert_eq!(inputs.checked_at_gps, now - 1800.);
            assert_eq!(inputs.next_check_at_gps, Some(now + 1800.));
            assert_eq!(inputs.latest.meta.retrieved_at_gps, now - 2. * 86400.);
            assert_eq!(inputs.latest.meta.file, format!("notices/{number}.nnu"));
        }
        assert!(
            source
                .cached("celestrak-gps-index", 3600., now + 1800.)
                .is_none()
        );
        files::atomic(
            &temp.path().join(format!("notices/{number}.nnu")),
            b"altered",
        )
        .unwrap();
        assert!(source.cached_notice(&number, now).is_none());
    }

    #[test]
    fn celestrak_refusal_cooldown_survives_restarts_without_extending_the_block() {
        let temp = crate::test_support::Temp::new();
        let checked = health::now().unwrap() - 120.;
        let state = serde_json::json!({
            "schema_version":1, "refresh_ok":false, "checked_at_gps":checked,
            "latest_observed_gps":checked, "latest_observed_by_prn":{},
            "notices":[], "orbit_events":[], "latest_nanu":"",
            "refresh_error":"Source https://celestrak.org/GPS/NANU/2026/ returned 403 Forbidden"
        });
        files::json(&temp.path().join("health-state.json"), &state).unwrap();
        let first = Sources::new(temp.path()).unwrap();
        assert_eq!(first.next_refresh_at(false), Some(checked + 7200.));
        assert!(first.check_celestrak_backoff(checked + 7199.).is_err());
        let mut later_state = state;
        later_state["checked_at_gps"] = (checked + 60.).into();
        files::json(&temp.path().join("health-state.json"), &later_state).unwrap();
        let mut restarted = Sources::new(temp.path()).unwrap();
        assert_eq!(restarted.next_refresh_at(false), Some(checked + 7200.));
        assert!(restarted.check_celestrak_backoff(checked + 7200.).is_ok());
        let error = restarted
            .document(
                "celestrak-gps-index",
                &format!("{CELESTRAK}jsonDir.php"),
                "index.json",
                false,
            )
            .err()
            .unwrap();
        assert!(error.to_string().contains("downloads paused"));
    }

    #[test]
    fn http_refusals_are_not_immediately_retried() {
        use std::{io::Write, net::TcpListener};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/notice", listener.local_addr().unwrap());
        let temp = crate::test_support::Temp::new();
        let mut source = Sources::new(temp.path()).unwrap();
        source.client = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let server = std::thread::spawn(move || {
            let (mut connection, _) = listener.accept().unwrap();
            connection
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = [0; 4096];
            assert!(connection.read(&mut request).unwrap() > 0);
            connection
                .write_all(
                    b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            drop(connection);
            listener
        });
        let error = source.fetch(&url, "notice.nnu", false).unwrap_err();
        assert_eq!(
            error.downcast_ref::<HttpFailure>().unwrap().status,
            StatusCode::FORBIDDEN
        );
        let listener = server.join().unwrap();
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn mirror_directory_rejects_missing_unsafe_and_inconsistent_entries() {
        let day = NaiveDate::from_ymd_opt(2026, 10, 16).unwrap();
        for text in [
            "[]".to_owned(),
            "<html>Denied</html>".into(),
            catalog(day, "2026000"),
            catalog(day, "2027001"),
            catalog(day, "2026002").replace("/NANU/2026/", "/NANU/2025/"),
            catalog(day, "2026002").replace("/GPS/status/2026/", "https://other.example/"),
            catalog(day + Days::days(1), "2026002"),
        ] {
            assert!(mirror_catalog(&text, day).is_err(), "{text}");
        }
    }

    #[test]
    fn historical_notice_cache_preserves_integrity_without_renewing_current_checks() {
        let temp = crate::test_support::Temp::new();
        let mut source = Sources::new(temp.path()).unwrap();
        let checked = 1_400_000_000.;
        for number in ["2025001", "2026001"] {
            let file = format!("notices/{number}.nnu");
            let text = notice(number);
            files::atomic(&temp.path().join(&file), text.as_bytes()).unwrap();
            let meta = document(
                &NoticeSource::Navcen.notice_url(number),
                &file,
                text,
                checked - 2. * 86400.,
            )
            .meta;
            source.manifest.insert(number.into(), meta);
        }
        let historical = source.cached_notice("2025001", checked).unwrap();
        assert_eq!(historical.retrieved_at_gps, checked - 2. * 86400.);
        assert_eq!(
            source
                .cached_notice("2026001", checked)
                .unwrap()
                .retrieved_at_gps,
            checked - 2. * 86400.
        );
        files::atomic(&temp.path().join(&historical.file), b"altered").unwrap();
        assert!(source.cached_notice("2025001", checked).is_none());
    }

    fn document(url: &str, file: &str, text: String, checked: f64) -> Document {
        Document {
            meta: Download {
                file: file.into(),
                url: url.into(),
                bytes: text.len(),
                sha256: crate::camera::hash(text.as_bytes()),
                retrieved_at_gps: checked,
                known_at_gps: checked,
                http_last_modified: None,
            },
            text,
        }
    }
    fn notice(number: &str) -> String {
        format!("NOTICE ADVISORY TO NAVSTAR USERS (NANU) {number} NANU TYPE: GENERAL\nPRN 39\n")
    }
    fn catalog(day: NaiveDate, number: &str) -> String {
        serde_json::json!([
            {"FILE_NAME": format!("/GPS/NANU/{}/nanu.{number}.txt", &number[..4])},
            {"FILE_NAME": format!("/GPS/status/{}/status.{}.txt", day.year(), day.format("%Y%m%d"))}
        ])
        .to_string()
    }
    fn advisory(day: NaiveDate, number: &str) -> String {
        format!(
            "GPS OPERATIONAL ADVISORY\nSUBJ: GPS STATUS {}\n{number} active notice\n",
            day.format("%d %b %Y").to_string().to_uppercase()
        )
    }

    #[test]
    fn navcen_failure_uses_validated_mirror_and_records_the_actual_source() {
        let day = NaiveDate::from_ymd_opt(2026, 10, 16).unwrap();
        let checked = health::utc_gps(day.and_hms_opt(12, 0, 0).unwrap().and_utc()).unwrap();
        for primary_fails in [false, true] {
            let mut urls = Vec::new();
            let inputs = notice_inputs(
                &mut |key, url, file, _| {
                    urls.push(url.to_owned());
                    if primary_fails && url.starts_with(NAVCEN) {
                        anyhow::bail!("403 Forbidden");
                    }
                    let text = match key {
                        "current_nanu" => notice("2026002"),
                        "operational_advisory" => advisory(day, "2026002"),
                        "celestrak-gps-index" => catalog(day, "2026002"),
                        _ => anyhow::bail!("Unexpected request {url}"),
                    };
                    Ok(Some(document(url, file, text, checked)))
                },
                checked,
                day,
                &|_| {},
            )
            .unwrap();
            let expected = if primary_fails {
                NoticeSource::Celestrak
            } else {
                NoticeSource::Navcen
            };
            assert_eq!(inputs.source, expected);
            assert_eq!(inputs.notice.number, "2026002");
            assert!(inputs.latest.meta.url.starts_with(if primary_fails {
                CELESTRAK
            } else {
                NAVCEN
            }));
            assert_eq!(urls.len(), if primary_fails { 4 } else { 2 });
            assert_eq!(
                inputs.source.notice_url("2025001"),
                if primary_fails {
                    format!("{CELESTRAK}NANU/2025/nanu.2025001.txt")
                } else {
                    format!("{NAVCEN}nanu/2025/2025001.nnu")
                }
            );
        }
    }

    #[test]
    fn mirror_cannot_clear_missing_malformed_stale_or_inconsistent_health_data() {
        let day = NaiveDate::from_ymd_opt(2026, 10, 16).unwrap();
        let checked = health::utc_gps(day.and_hms_opt(12, 0, 0).unwrap().and_utc()).unwrap();
        for failure in [
            "unavailable",
            "bad-index",
            "wrong-notice",
            "stale",
            "future",
            "html",
            "behind",
        ] {
            let error = notice_inputs(
                &mut |key, url, file, _| {
                    if url.starts_with(NAVCEN) || failure == "unavailable" {
                        anyhow::bail!("Source {url} returned 403 Forbidden");
                    }
                    let text = match key {
                        "celestrak-gps-index" if failure == "bad-index" => {
                            "<html>Access denied</html>".into()
                        }
                        "celestrak-gps-index" => catalog(day, "2026002"),
                        "current_nanu" => notice(if failure == "wrong-notice" {
                            "2026001"
                        } else {
                            "2026002"
                        }),
                        "operational_advisory" => match failure {
                            "stale" => advisory(day - Days::days(4), "2026002"),
                            "future" => advisory(day + Days::days(4), "2026002"),
                            "html" => "<html>Access denied</html>".into(),
                            "behind" => advisory(day, "2026003"),
                            _ => advisory(day, "2026002"),
                        },
                        _ => anyhow::bail!("Unexpected request {url}"),
                    };
                    Ok(Some(document(url, file, text, checked)))
                },
                checked,
                day,
                &|_| {},
            )
            .err()
            .expect("Invalid health inputs must fail closed");
            let message = format!("{error:#}");
            assert!(
                message.contains("US Coast Guard NAVCEN"),
                "{failure}: {message}"
            );
            assert!(message.contains("CelesTrak mirror"), "{failure}: {message}");
        }
    }

    #[test]
    fn mirror_uses_published_directory_entries_at_year_rollover_without_404_probes() {
        let day = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        let checked = health::utc_gps(day.and_hms_opt(12, 0, 0).unwrap().and_utc()).unwrap();
        let mut urls = Vec::new();
        let inputs = notice_inputs(
            &mut |key, url, file, missing_ok| {
                urls.push(url.to_owned());
                if url.starts_with(NAVCEN) {
                    anyhow::bail!("403 Forbidden")
                }
                assert!(!missing_ok);
                let text = match key {
                    "celestrak-gps-index" => catalog(day - Days::days(1), "2025002"),
                    "current_nanu" => notice("2025002"),
                    "operational_advisory" => advisory(day - Days::days(1), "2025002"),
                    _ => anyhow::bail!("Unexpected request {url}"),
                };
                Ok(Some(document(url, file, text, checked)))
            },
            checked,
            day,
            &|_| {},
        )
        .unwrap();
        assert_eq!(inputs.notice.number, "2025002");
        assert!(urls.contains(&format!("{CELESTRAK}status/2025/status.20251231.txt")));
        assert_eq!(urls.len(), 4);
    }

    #[test]
    fn official_earth_orientation_mirror_records_its_source_and_only_falls_back_on_failure() {
        let urls = [EOP, EOP_BACKUP, EOP_MIRROR];
        for failures in 0..urls.len() {
            let mut calls = Vec::new();
            let file = earth_orientation(
                |url| {
                    calls.push(url.to_owned());
                    if calls.len() <= failures {
                        anyhow::bail!("source unavailable");
                    }
                    Ok(Download {
                        file: "finals2000A.all".into(),
                        url: url.into(),
                        bytes: 100,
                        sha256: "verified-download-hash".into(),
                        retrieved_at_gps: 1.,
                        known_at_gps: 1.,
                        http_last_modified: None,
                    })
                },
                None,
                &|_| {},
            )
            .unwrap();
            assert_eq!(file.url, urls[failures]);
            assert_eq!(calls, urls[..=failures]);
        }
        let error = earth_orientation(
            |url| anyhow::bail!("Downloading {url}: connection failed"),
            None,
            &|_| {},
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("IERS:") && message.contains("USNO:") && message.contains("Astropy:")
        );
        assert!(urls.iter().all(|url| message.contains(url)));
    }

    fn eop_table(last_measured_day: i64, last_day: i64) -> Vec<u8> {
        let today = Utc::now().date_naive();
        let mjd = today
            .signed_duration_since(NaiveDate::from_ymd_opt(1858, 11, 17).unwrap())
            .num_days();
        let mut bytes = Vec::new();
        for day in -40..=last_day {
            let mut row = vec![b' '; 185];
            row[7..15].copy_from_slice(format!("{:8.2}", (mjd + day) as f64).as_bytes());
            row[16] = if day <= last_measured_day { b'I' } else { b'P' };
            row[18..27].copy_from_slice(b" 0.123456");
            row[37..46].copy_from_slice(b" 0.234567");
            row[57] = row[16];
            row[58..68].copy_from_slice(b"-0.0123456");
            bytes.extend(row);
            bytes.push(b'\n');
        }
        bytes
    }

    #[test]
    fn earth_orientation_requires_recent_measurements_and_complete_forecast_coverage() {
        let checked = health::now().unwrap();
        assert!(validate_earth_orientation(&eop_table(-9, 20), checked).is_ok());
        assert!(validate_earth_orientation(&eop_table(-29, 20), checked).is_ok());
        for invalid in [
            b"<html>Service unavailable</html>".to_vec(),
            vec![255; 185],
            eop_table(-31, 20),
            eop_table(1, 20),
            eop_table(-9, 14),
        ] {
            assert!(validate_earth_orientation(&invalid, checked).is_err());
        }
    }

    #[test]
    fn earth_orientation_outage_reuses_only_verified_cache_without_renewing_its_age() {
        let temp = crate::test_support::Temp::new();
        let mut source = Sources::new(temp.path()).unwrap();
        let checked = health::now().unwrap();
        let bytes = eop_table(-9, 20);
        let meta = Download {
            file: EOP_FILE.into(),
            url: EOP.into(),
            bytes: bytes.len(),
            sha256: crate::camera::hash(&bytes),
            retrieved_at_gps: checked - 3. * data::DAY,
            known_at_gps: checked - 9. * data::DAY,
            http_last_modified: None,
        };
        files::atomic(&temp.path().join(EOP_FILE), &bytes).unwrap();
        source
            .manifest
            .insert("earth_orientation".into(), meta.clone());
        assert!(
            source
                .cached("earth_orientation", data::DAY, checked)
                .is_none()
        );
        let cached = source.cached_earth_orientation(checked).unwrap();
        let mut calls = 0;
        let used = earth_orientation(
            |_| {
                calls += 1;
                anyhow::bail!("source unavailable")
            },
            Some(cached),
            &|_| {},
        )
        .unwrap();
        assert_eq!(calls, 3);
        assert_eq!(
            serde_json::to_value(&used).unwrap(),
            serde_json::to_value(&meta).unwrap()
        );
        files::atomic(&temp.path().join(EOP_FILE), b"corrupted").unwrap();
        assert!(source.cached_earth_orientation(checked).is_none());
        let stale = eop_table(-31, 20);
        files::atomic(&temp.path().join(EOP_FILE), &stale).unwrap();
        let entry = source.manifest.get_mut("earth_orientation").unwrap();
        entry.bytes = stale.len();
        entry.sha256 = crate::camera::hash(&stale);
        entry.retrieved_at_gps = checked;
        assert!(source.cached_earth_orientation(checked).is_none());
    }

    #[test]
    fn invalid_earth_orientation_response_cannot_overwrite_the_last_good_table() {
        use std::{io::Write, net::TcpListener};
        let temp = crate::test_support::Temp::new();
        let bytes = eop_table(-9, 20);
        files::atomic(&temp.path().join(EOP_FILE), &bytes).unwrap();
        let mut source = Sources::new(temp.path()).unwrap();
        source.client = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/eop", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut connection, _) = listener.accept().unwrap();
            connection
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = [0; 4096];
            assert!(connection.read(&mut request).unwrap() > 0);
            connection
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\n<html>",
                )
                .unwrap();
        });
        assert!(source.fetch_once(&url, EOP_FILE, false).is_err());
        server.join().unwrap();
        assert_eq!(fs::read(temp.path().join(EOP_FILE)).unwrap(), bytes);
        assert!(!source.manifest.contains_key("earth_orientation"));
    }

    #[test]
    #[ignore = "requires live Earth-orientation downloads"]
    fn live_earth_orientation_download_validates_with_the_app_http_client() {
        let temp = crate::test_support::Temp::new();
        let mut source = Sources::new(temp.path()).unwrap();
        let meta = earth_orientation(
            |url| source.required("earth_orientation", url, EOP_FILE),
            None,
            &|progress| eprintln!("{progress}"),
        )
        .unwrap();
        let bytes = fs::read(temp.path().join(EOP_FILE)).unwrap();
        validate_earth_orientation(&bytes, health::now().unwrap()).unwrap();
        assert_eq!(crate::camera::hash(&bytes), meta.sha256);
        eprintln!("Validated {} bytes from {}", bytes.len(), meta.url);
    }

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
