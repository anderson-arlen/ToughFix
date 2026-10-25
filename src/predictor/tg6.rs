// SPDX-License-Identifier: MIT
//! TG-6 selector-3 state-vector assistance. Independent codec; no vendor inputs.
use super::data::{self, AU, DAY, Frames, GPS_JD, V3};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path};

pub const CAPACITY: usize = 262144;
pub const FORECAST_DAYS: usize = super::clocks::TG6_FORECAST_DAYS;
pub const STATE_COUNT: usize = FORECAST_DAYS * 12;
const CLOCK_REFERENCE_LOOKBACK: f64 = 2. * DAY;
const MJD_GPS: f64 = 44244.;
const STEP: f64 = 7200.;

#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    pub epoch: u32,
    pub step: i16,
    pub values: Vec<i32>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub prn: u16,
    pub clock_epoch: u32,
    pub bias: i32,
    pub rate: i32,
    pub state_epoch: u32,
    pub states: Vec<[i32; 6]>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct Archive {
    pub tables: Vec<Table>,
    pub records: Vec<Record>,
    pub padded_size: usize,
}

pub fn seconds(ticks: u32) -> f64 {
    (ticks as f64 - MJD_GPS * 96.) * 900.
}
fn ticks(seconds: f64) -> Result<u32> {
    let value = MJD_GPS * 96. + seconds / 900.;
    ensure!(
        value.is_finite() && value.fract() == 0. && (0. ..=u32::MAX as f64).contains(&value),
        "TG-6 epoch must be an integral 15-minute GPS boundary"
    );
    Ok(value as u32)
}
fn integer(value: f64, scale: f64) -> Result<i32> {
    let n = (value * scale).round_ties_even();
    ensure!(
        n.is_finite() && (i32::MIN as f64..=i32::MAX as f64).contains(&n),
        "TG-6 value exceeds its signed integer field"
    );
    Ok(n as i32)
}
fn u16_at(b: &[u8], p: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(
        b.get(p..p + 2)
            .context("Truncated TG-6 field")?
            .try_into()?,
    ))
}
fn u32_at(b: &[u8], p: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        b.get(p..p + 4)
            .context("Truncated TG-6 field")?
            .try_into()?,
    ))
}
fn i32_at(b: &[u8], p: usize) -> Result<i32> {
    Ok(u32_at(b, p)? as i32)
}

impl Archive {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(
            (12..=CAPACITY).contains(&bytes.len()) && bytes.len().is_multiple_of(4),
            "Invalid TG-6 buffer size"
        );
        let offset = u16_at(bytes, 2)? as usize;
        let logical = u32_at(bytes, 4)? as usize;
        ensure!(
            u16_at(bytes, 0)? == 0x122a
                && offset > 8
                && offset.is_multiple_of(4)
                && logical.is_multiple_of(4)
                && offset <= logical.saturating_sub(4)
                && (12..=bytes.len()).contains(&logical),
            "Invalid TG-6 container header"
        );
        ensure!(
            bytes[logical..].iter().all(|&x| x == 0),
            "Nonzero TG-6 padding"
        );
        let checksum = bytes[..logical - 4]
            .iter()
            .fold(0u32, |a, &b| a.wrapping_add(b as u32));
        ensure!(
            checksum == u32_at(bytes, logical - 4)?,
            "TG-6 checksum mismatch"
        );
        let b = &bytes[..logical - 4];
        let mut pos = 8;
        let mut tables = Vec::new();
        for (i, width) in [3, 3, 1, 2, 2].into_iter().enumerate() {
            let epoch = u32_at(b, pos)?;
            let step = u16_at(b, pos + 4)? as i16;
            let count = u16_at(b, pos + 6)? as usize;
            ensure!(
                count > 0 && count.is_multiple_of(width) && pos + 8 + 4 * count <= offset,
                "Invalid TG-6 ancillary table length"
            );
            ensure!(
                step == [4, -1, 1, 1, -1][i],
                "Unexpected TG-6 ancillary spacing"
            );
            let values = (0..count)
                .map(|j| i32_at(b, pos + 8 + 4 * j))
                .collect::<Result<Vec<_>>>()?;
            tables.push(Table {
                epoch,
                step,
                values,
            });
            pos += 8 + 4 * count;
        }
        ensure!(pos == offset, "TG-6 ancillary region length mismatch");
        let mut records = Vec::new();
        let mut seen = BTreeSet::new();
        while pos < b.len() {
            let marker = u16_at(b, pos)?;
            let prn = ((marker >> 8) & 31) + 1;
            let size = u16_at(b, pos + 2)? as usize;
            let count = u16_at(b, pos + 22)? as usize;
            ensure!(
                marker == 0x802a + ((prn - 1) << 8) && seen.insert(prn),
                "Invalid or duplicate TG-6 satellite"
            );
            ensure!(
                (1..=336).contains(&count)
                    && size == 24 + 24 * count
                    && pos + size <= b.len()
                    && u16_at(b, pos + 20)? == STEP as u16,
                "Invalid TG-6 state record"
            );
            let mut states = Vec::with_capacity(count);
            for j in 0..count {
                let mut state = [0; 6];
                for (k, v) in state.iter_mut().enumerate() {
                    *v = i32_at(b, pos + 24 + j * 24 + k * 4)?;
                }
                let r = V3::new(state[0] as f64, state[1] as f64, state[2] as f64) / 10.;
                let v = V3::new(state[3] as f64, state[4] as f64, state[5] as f64) / 1000.;
                ensure!(
                    (2e7..3e7).contains(&r.norm()) && (2000. ..5000.).contains(&v.norm()),
                    "Nonphysical TG-6 state vector"
                );
                states.push(state);
            }
            let record = Record {
                prn,
                clock_epoch: u32_at(b, pos + 4)?,
                bias: i32_at(b, pos + 8)?,
                rate: i32_at(b, pos + 12)?,
                state_epoch: u32_at(b, pos + 16)?,
                states,
            };
            ensure!(
                record.clock_epoch <= record.state_epoch,
                "TG-6 clock reference follows its states"
            );
            if let Some(first) = records.first() {
                let first: &Record = first;
                ensure!(
                    record.state_epoch == first.state_epoch
                        && record.states.len() == first.states.len(),
                    "TG-6 satellite coverage differs"
                );
            }
            records.push(record);
            pos += size;
        }
        ensure!(
            pos == b.len() && !records.is_empty(),
            "Empty or truncated TG-6 satellite region"
        );
        Ok(Self {
            tables,
            records,
            padded_size: bytes.len(),
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ensure!(
            self.tables.len() == 5 && !self.records.is_empty() && self.records.len() <= 32,
            "Invalid TG-6 archive contents"
        );
        let mut b = vec![0x2a, 0x12, 0, 0, 0, 0, 0, 0];
        for t in &self.tables {
            b.extend(t.epoch.to_le_bytes());
            b.extend(t.step.to_le_bytes());
            b.extend(u16::try_from(t.values.len())?.to_le_bytes());
            for v in &t.values {
                b.extend(v.to_le_bytes());
            }
        }
        let offset = u16::try_from(b.len())?;
        for r in &self.records {
            ensure!((1..=32).contains(&r.prn), "Invalid TG-6 PRN");
            b.extend((0x802a + ((r.prn - 1) << 8)).to_le_bytes());
            b.extend(u16::try_from(24 + 24 * r.states.len())?.to_le_bytes());
            b.extend(r.clock_epoch.to_le_bytes());
            b.extend(r.bias.to_le_bytes());
            b.extend(r.rate.to_le_bytes());
            b.extend(r.state_epoch.to_le_bytes());
            b.extend((STEP as u16).to_le_bytes());
            b.extend(u16::try_from(r.states.len())?.to_le_bytes());
            for s in &r.states {
                for v in s {
                    b.extend(v.to_le_bytes());
                }
            }
        }
        b[2..4].copy_from_slice(&offset.to_le_bytes());
        let logical = u32::try_from(b.len() + 4)?;
        b[4..8].copy_from_slice(&logical.to_le_bytes());
        let sum = b.iter().fold(0u32, |a, &x| a.wrapping_add(x as u32));
        b.extend(sum.to_le_bytes());
        ensure!(
            b.len() <= self.padded_size && self.padded_size <= CAPACITY,
            "TG-6 archive exceeds capacity"
        );
        b.resize(self.padded_size, 0);
        Self::decode(&b)?;
        Ok(b)
    }
    pub fn start(&self) -> f64 {
        seconds(self.records[0].state_epoch)
    }
    pub fn end(&self) -> f64 {
        self.start() + STEP * self.records[0].states.len() as f64
    }
    pub fn inspect(&self) -> Value {
        json!({"format":"tg6-state-vectors-v1", "start_gps":data::calendar(self.start()),
               "validity_end_exclusive_gps":data::calendar(self.end()),
               "prns":self.records.iter().map(|r| r.prn).collect::<Vec<_>>(),
               "samples_per_satellite":self.records[0].states.len(), "bytes":self.padded_size})
    }
}

fn table(epoch_seconds: f64, step: i16, values: Vec<i32>) -> Result<Table> {
    Ok(Table {
        epoch: ticks(epoch_seconds)?,
        step,
        values,
    })
}
fn ancillary(start: f64, days: usize, frames: &Frames) -> Result<Vec<Table>> {
    let day = (start / DAY).floor() * DAY;
    let mut sun = Vec::new();
    let sun_start = day - 5. * DAY;
    for i in 0..days / 4 + 5 {
        let t = sun_start - DAY + i as f64 * 4. * DAY;
        let ((earth, _), warning) = erfars::ephemerides::Epv00(GPS_JD, t / DAY)
            .map_err(|e| anyhow::anyhow!("ERFA TG-6 Earth ephemeris: {e:?}"))?;
        ensure!(
            warning == 0,
            "TG-6 Earth ephemeris outside accuracy interval"
        );
        for v in &earth[..3] {
            sun.push(integer(v * AU / 1000., 1.)?);
        }
    }
    let moon_start = day - DAY / 2.;
    let mut moon = Vec::new();
    let mut nutation = Vec::new();
    let arcsec = 180. / std::f64::consts::PI * 3600.;
    for i in 0..days * 2 + 7 {
        let t = moon_start - DAY + i as f64 * DAY / 2.;
        if i < days * 2 + 6 {
            for v in &erfars::ephemerides::Moon98(GPS_JD, t / DAY)[..3] {
                moon.push(integer(v * AU, 1.)?);
            }
        }
        let (dp, de) = erfars::precnutpolar::Nut00a(GPS_JD, t / DAY);
        nutation.push(integer(dp * arcsec, 1e4)?);
        nutation.push(integer(de * arcsec, 1e4)?);
    }
    let eop_start = day - DAY;
    let mut tai_ut1 = Vec::new();
    let mut polar = Vec::new();
    for i in 0..days + 5 {
        let mjd = MJD_GPS + (eop_start + i as f64 * DAY) / DAY;
        let [xp, yp, dut] = frames.orientation(mjd)?;
        let date = chrono::DateTime::from_timestamp(((mjd - MJD_GPS) * DAY + 315964800.) as i64, 0)
            .context("Invalid TG-6 ancillary date")?;
        use chrono::Datelike;
        let (leaps, warning) =
            erfars::timescales::Dat(date.year(), date.month() as i32, date.day() as i32, 0.)
                .map_err(|e| anyhow::anyhow!("ERFA TG-6 leap seconds: {e:?}"))?;
        ensure!(
            warning == 0,
            "TG-6 ancillary dates need an updated leap-second table"
        );
        tai_ut1.push(integer(leaps - dut, 1e6)?);
        polar.push(integer(xp, 1e5)?);
        polar.push(integer(yp, 1e5)?);
    }
    Ok(vec![
        table(sun_start, 4, sun)?,
        table(moon_start, -1, moon)?,
        table(eop_start, 1, tai_ut1)?,
        table(eop_start, 1, polar)?,
        table(moon_start, -1, nutation)?,
    ])
}

/// Export native orbit states with a separate, observed-data linear clock fit
/// checked against recent observations. TG-1's selected clock is independent.
pub fn generate(diagnostics: &Value, metrics: &Value, eop: &Path) -> Result<(Vec<u8>, Value)> {
    let start = data::epoch(
        diagnostics["forecast_start_gps"]
            .as_str()
            .context("Missing TG-6 epoch")?,
    )?;
    ensure!(
        metrics["forecast_start_gps"] == diagnostics["forecast_start_gps"]
            && metrics["forecast_days"] == 14
            && metrics["earth_orientation_sha256"].as_str()
                == Some(crate::camera::hash(&std::fs::read(eop)?).as_str()),
        "TG-6 forecast/Earth-orientation provenance mismatch"
    );
    let mut records = Vec::new();
    let mut exclusions = Vec::new();
    let mut clock_reports = Vec::new();
    let mut clock_sources = std::collections::BTreeMap::<String, usize>::new();
    let mut health_start = data::epoch(
        metrics["training_start_gps"]
            .as_str()
            .context("Missing native training epoch")?,
    )?;
    for s in diagnostics["satellites"]
        .as_array()
        .context("Missing native satellite diagnostics")?
    {
        let prn = s["prn"].as_u64().context("Missing native PRN")?;
        let attempt = (|| -> Result<Record> {
            ensure!(
                s["tg6_clock"].is_object(),
                "{}",
                s["tg6_clock_error"]
                    .as_str()
                    .unwrap_or("Regenerate predictions with a validated TG-6 linear clock fit")
            );
            let clock: super::clocks::Tg6Clock = serde_json::from_value(s["tg6_clock"].clone())?;
            clock.validate(start)?;
            let c = clock.model.coefficients;
            let rows = s["trajectory_2h_gcrs"]
                .as_array()
                .context("Missing two-hour native states")?;
            ensure!(
                rows.len() == STATE_COUNT,
                "Incomplete native TG-6 trajectory"
            );
            let mut states = Vec::new();
            for row in rows {
                let v: Vec<f64> = serde_json::from_value(row.clone())?;
                ensure!(v.len() == 6, "Invalid native TG-6 state");
                let mut state = [0; 6];
                for k in 0..6 {
                    state[k] = integer(v[k], if k < 3 { 10. } else { 1000. })?;
                }
                states.push(state);
            }
            let record = Record {
                prn: u16::try_from(prn)?,
                clock_epoch: ticks(start - CLOCK_REFERENCE_LOOKBACK)?,
                bias: integer(c[0] - c[1] * (10800. + CLOCK_REFERENCE_LOOKBACK), 1e12)?,
                rate: integer(c[1], 1e20)?,
                state_epoch: ticks(start)?,
                states,
            };
            health_start = health_start.min(clock.model.training_start_gps_seconds);
            *clock_sources.entry(clock.model.source.clone()).or_default() += 1;
            clock_reports.push(json!({"prn":prn,"source":clock.model.source,
                "training_rms_ns":clock.model.training_rms_ns,"past_day_validation_ns":clock.model.past_day_validation_ns[0],
                "forecast_validation":clock.validation}));
            Ok(record)
        })();
        match attempt {
            Ok(r) => records.push(r),
            Err(e) => exclusions.push(json!({"prn":prn,"reason":format!("{e:#}")})),
        }
    }
    ensure!(
        records.len() >= 4,
        "Fewer than four representable TG-6 satellites"
    );
    let archive = Archive {
        tables: ancillary(start, FORECAST_DAYS, &Frames::read(eop)?)?,
        records,
        padded_size: CAPACITY,
    };
    let bytes = archive.encode()?;
    let mut report = metrics.clone();
    report["format"] = json!("tg6-state-vectors-v1");
    report["forecast_days"] = json!(FORECAST_DAYS);
    report["validity_end_exclusive_gps"] = json!(data::calendar(archive.end()));
    report["bytes"] = json!(bytes.len());
    report["sha256"] = json!(crate::camera::hash(&bytes));
    report["format_exclusions"] = json!(exclusions);
    report["clock_policy"] = json!(
        "Independent recent observed linear fit: training and held-out past-day RMS <= 50 ns, residual steps <= 50 ns. Prefer a recent Rapid fallback when newer data exceed relative gates; without a fallback use these absolute limits. Historical fortnight errors are diagnostic, not constellation exclusions. No camera position-accuracy guarantee."
    );
    report["clock_validation"] = json!(clock_reports);
    report["clock_sources"] = json!(clock_sources);
    report["health_training_start_gps"] = json!(data::calendar(health_start));
    let prns = archive.records.iter().map(|r| r.prn).collect::<Vec<_>>();
    report["available_prns_by_week"] = json!([prns, prns, prns, prns]);
    report["upload_allowed"] = json!(false);
    report["model_warning"] = json!(
        "TG-6 candidate requires receiver validation and live satellite-health authorization"
    );
    report["broadcast_approximation"] = Value::Null;
    report["archive_tail"] = json!("Four weeks of two-hour inertial state vectors");
    Ok((bytes, report))
}

/// Apply the shared health policy over this archive's complete four weeks.
/// TG-1 polynomial/clock exclusions do not determine TG-6 availability.
pub fn guard(
    bytes: &[u8],
    training: f64,
    snapshot: &super::health::Snapshot,
    now: f64,
) -> Result<(Vec<u8>, Value)> {
    let mut archive = Archive::decode(bytes)?;
    ensure!(
        archive.records[0].states.len() == STATE_COUNT,
        "TG-6 predictions need a fresh four-week forecast"
    );
    let before: Vec<_> = archive.records.iter().map(|r| r.prn).collect();
    let (eligible, mut report) = super::health::guard_prns(
        &before,
        training,
        archive.start(),
        archive.end(),
        snapshot,
        now,
    )?;
    archive.records.retain(|r| eligible.contains(&r.prn));
    // Preserve decodability when health removes everything; the report blocks
    // upload and the original immutable candidate remains available to inspect.
    if archive.records.is_empty() {
        report["available_prns_by_week"] = json!([[], [], [], []]);
        report["removed_tg6_prns"] = json!(before);
        report["output_sha256"] = json!(crate::camera::hash(bytes));
        return Ok((bytes.to_vec(), report));
    }
    let retained: Vec<_> = archive.records.iter().map(|r| r.prn).collect();
    let guarded = archive.encode()?;
    report["available_prns_by_week"] = json!([retained, retained, retained, retained]);
    report["removed_tg6_prns"] = json!(
        before
            .iter()
            .filter(|p| !retained.contains(p))
            .collect::<Vec<_>>()
    );
    report["output_sha256"] = json!(crate::camera::hash(&guarded));
    report["validation"] = archive.inspect();
    Ok((guarded, report))
}

pub fn cli(mut args: impl Iterator<Item = String>) -> Result<()> {
    match args.next().as_deref() {
        Some("inspect") => {
            let path = args.next().context("tg6 inspect needs a file")?;
            ensure!(args.next().is_none(), "Unexpected TG-6 inspect argument");
            let b = std::fs::read(path)?;
            let a = Archive::decode(&b)?;
            ensure!(a.encode()? == b, "TG-6 round-trip mismatch");
            let mut report = a.inspect();
            report["sha256"] = json!(crate::camera::hash(&b));
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Some("encode") => {
            let mut d = None;
            let mut m = None;
            let mut e = None;
            let mut o = None;
            while let Some(arg) = args.next() {
                if matches!(arg.as_str(), "--help" | "-h") {
                    println!(
                        "toughfix tg6 encode --diagnostics FILE --metrics FILE --eop FILE --output FILE\nOffline encoder; never accesses a camera."
                    );
                    return Ok(());
                }
                let value = args.next().context("TG-6 encode option needs a path")?;
                match arg.as_str() {
                    "--diagnostics" => d = Some(value),
                    "--metrics" => m = Some(value),
                    "--eop" => e = Some(value),
                    "--output" => o = Some(value),
                    _ => bail!("Unknown TG-6 encode option {arg}"),
                }
            }
            let diagnostics =
                serde_json::from_slice(&std::fs::read(d.context("--diagnostics is required")?)?)?;
            let metrics =
                serde_json::from_slice(&std::fs::read(m.context("--metrics is required")?)?)?;
            let (bytes, report) = generate(
                &diagnostics,
                &metrics,
                Path::new(&e.context("--eop is required")?),
            )?;
            let output = std::path::PathBuf::from(o.context("--output is required")?);
            crate::files::atomic(&output, &bytes)?;
            crate::files::json(&output.with_extension("json"), &report)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&Archive::decode(&bytes)?.inspect())?
            );
        }
        _ => println!(
            "toughfix tg6 inspect FILE\ntoughfix tg6 encode --diagnostics FILE --metrics FILE --eop FILE --output FILE\nOffline independent TG-6 codec. Never accesses a camera."
        ),
    }
    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/tg6-state-vectors.bin");

    #[test]
    fn encoder_uses_validated_tg6_clocks_and_rejects_failed_error_checks() {
        let temp = crate::test_support::Temp::new();
        let eop = temp.path().join("eop.txt");
        let mut lines = String::new();
        for day in 61315..61360 {
            let mut line = " ".repeat(80);
            for (a, b, value) in [
                (7, 15, format!("{day:8.2}")),
                (18, 27, format!("{:9.6}", 0.1)),
                (37, 46, format!("{:9.6}", 0.2)),
                (58, 68, format!("{:10.7}", -0.035)),
            ] {
                line.replace_range(a..b, &value);
            }
            lines.push_str(&line);
            lines.push('\n');
        }
        std::fs::write(&eop, lines).unwrap();
        let cutoff = data::epoch("2026-10-14T00:00:00").unwrap();
        let state = Archive::decode(FIXTURE).unwrap().records[0].states[0];
        let clock = json!({"model":{
            "degree":1,"coefficients":[1e-4 + 1e-12*10800.,1e-12,0.],
            "samples":288,"training_rms_ns":1.,"largest_step_ns":1.,"past_day_validation_ns":[2.,1.],
            "source":"rapid","training_start_gps_seconds":cutoff-3.*DAY,
            "training_end_gps_seconds":cutoff-900.,"selection_reason":"Independent TG-6 linear fit"
        },"validation":{"forecast_days":14,"forecast_start_gps_seconds":cutoff-14.*DAY,
            "training_end_gps_seconds":cutoff-14.*DAY-900.,"observed_end_gps_seconds":cutoff-900.,
            "samples":14*96,"rms_ns":10.,"max_absolute_ns":20.}});
        let mut diagnostics = json!({"forecast_start_gps":data::calendar(cutoff),
            "satellites":(1..=6).map(|prn| json!({"prn":prn,
                "clock":{"degree":2,"coefficients":[1e-4,1e-12,1e-20]},
                "tg6_clock":clock,"trajectory_2h_gcrs":vec![state.map(|v|v as f64);STATE_COUNT].iter().map(|s|
                    s.iter().enumerate().map(|(k,v)|v/if k<3 {10.} else {1000.}).collect::<Vec<_>>()).collect::<Vec<_>>()
            })).collect::<Vec<_>>()});
        let metrics = json!({"forecast_start_gps":data::calendar(cutoff),"forecast_days":14,
            "training_start_gps":data::calendar(cutoff-3.*DAY),
            "available_prns_by_week":[[1,2,3,4,5,6],[1,2,3,4,5,6]],
            "earth_orientation_sha256":crate::camera::hash(&std::fs::read(&eop).unwrap())});
        let (bytes, report) = generate(&diagnostics, &metrics, &eop).unwrap();
        let decoded = Archive::decode(&bytes).unwrap();
        assert_eq!(decoded.records.len(), 6);
        assert_eq!(decoded.records[0].states.len(), 336);
        assert_eq!(decoded.end() - decoded.start(), 28. * DAY);
        assert_eq!(u16_at(&bytes, 2).unwrap(), 1836);
        assert_eq!(
            decoded
                .tables
                .iter()
                .map(|t| t.values.len())
                .collect::<Vec<_>>(),
            vec![36, 186, 33, 66, 126]
        );
        assert_eq!(report["forecast_days"], 28);
        assert_eq!(decoded.records[0].bias, 99_827_200);
        assert_eq!(seconds(decoded.records[0].clock_epoch), cutoff - 2. * DAY);
        assert_eq!(decoded.records[0].rate, 100_000_000);
        assert_eq!(report["clock_validation"].as_array().unwrap().len(), 6);
        let mut full = decoded.clone();
        full.records = (1..=32)
            .map(|prn| Record {
                prn,
                ..decoded.records[0].clone()
            })
            .collect();
        let full_bytes = full.encode().unwrap();
        assert_eq!(u32_at(&full_bytes, 4).unwrap(), 260656);
        assert_eq!(Archive::decode(&full_bytes).unwrap().records.len(), 32);
        diagnostics["satellites"][0]["tg6_clock"]["validation"]["max_absolute_ns"] = json!(1e6);
        assert_eq!(
            Archive::decode(&generate(&diagnostics, &metrics, &eop).unwrap().0)
                .unwrap()
                .records
                .len(),
            6
        );
        diagnostics["satellites"][0]["tg6_clock"]["model"]["past_day_validation_ns"][0] =
            json!(51.);
        diagnostics["satellites"][1]["tg6_clock"]["model"]["degree"] = json!(2);
        let (bytes, report) = generate(&diagnostics, &metrics, &eop).unwrap();
        assert_eq!(
            Archive::decode(&bytes)
                .unwrap()
                .records
                .iter()
                .map(|r| r.prn)
                .collect::<Vec<_>>(),
            vec![3, 4, 5, 6]
        );
        assert_eq!(report["format_exclusions"].as_array().unwrap().len(), 2);
        diagnostics["satellites"][2]["tg6_clock"] = Value::Null;
        assert!(generate(&diagnostics, &metrics, &eop).is_err());
    }

    #[test]
    fn independent_python_fixture_round_trips_with_correct_units() {
        assert_eq!(
            crate::camera::hash(FIXTURE),
            "36a654916acd8c49fb951b9a6810e28e44fb6b4ca3572fad635d94dde730a2b6"
        );
        let a = Archive::decode(FIXTURE).unwrap();
        assert_eq!(a.encode().unwrap(), FIXTURE);
        assert_eq!(a.records.len(), 1);
        let r = &a.records[0];
        assert_eq!(r.prn, 1);
        assert_eq!(r.bias, 149741781);
        assert_eq!(r.rate, -857151685);
        assert_eq!(
            r.states[0],
            [
                -230056474, 133759868, -4821097, -1074485, -1959642, -3154549
            ]
        );
        assert_eq!(
            data::calendar(seconds(r.clock_epoch)),
            "2026-10-14T18:00:00"
        );
        assert_eq!(data::calendar(a.start()), "2026-10-14T18:00:00");
    }

    fn repair(b: &mut [u8]) {
        let end = u32_at(b, 4).unwrap() as usize - 4;
        if end + 4 > b.len() {
            return;
        }
        let sum = b[..end].iter().map(|&v| v as u32).sum::<u32>();
        b[end..end + 4].copy_from_slice(&sum.to_le_bytes());
    }
    #[test]
    fn malformed_lengths_units_and_padding_fail_even_with_valid_checksum() {
        let record = u16_at(FIXTURE, 2).unwrap() as usize;
        for offset in [0, 2, 4, 14, record, record + 2, record + 20, record + 22] {
            let mut b = FIXTURE.to_vec();
            b[offset] ^= 1;
            repair(&mut b);
            assert!(Archive::decode(&b).is_err(), "offset {offset}");
        }
        let mut b = FIXTURE.to_vec();
        b[record + 24..record + 36].fill(0);
        repair(&mut b);
        assert!(Archive::decode(&b).is_err());
        let mut b = FIXTURE.to_vec();
        b.extend([0, 0, 0, 1]);
        assert!(Archive::decode(&b).is_err());
        for end in [0, 8, record - 1, record + 1, record + 24, FIXTURE.len() - 1] {
            assert!(Archive::decode(&FIXTURE[..end]).is_err());
        }
    }
    #[test]
    fn duplicate_prns_and_different_satellite_coverage_are_rejected() {
        let mut a = Archive::decode(FIXTURE).unwrap();
        a.padded_size = CAPACITY;
        a.records.push(a.records[0].clone());
        assert!(a.encode().is_err());
        a.records[1].prn = 2;
        a.records[1].state_epoch += 1;
        assert!(a.encode().is_err());
    }
    #[test]
    fn clock_reference_and_integer_limits_are_explicit() {
        let a = 1e-4;
        let b = 1e-12;
        let bias = integer(a - b * (10800. + CLOCK_REFERENCE_LOOKBACK), 1e12).unwrap();
        let rate = integer(b, 1e20).unwrap();
        for dt in [0., 86400., 14. * DAY, 28. * DAY] {
            let original = a + b * (dt - 10800.);
            let encoded =
                bias as f64 * 1e-12 + rate as f64 * 1e-20 * (dt + CLOCK_REFERENCE_LOOKBACK);
            assert!((encoded - original).abs() < 0.6e-12);
        }
        assert!(integer(f64::NAN, 1.).is_err());
        assert!(integer(2147483648., 1.).is_err());
        assert!(ticks(1475971201.).is_err());
    }
    pub(crate) fn candidate(start: f64) -> Vec<u8> {
        let mut a = Archive::decode(FIXTURE).unwrap();
        let mut r = a.records[0].clone();
        r.state_epoch = ticks(start).unwrap();
        r.clock_epoch = r.state_epoch;
        r.states = vec![r.states[0]; STATE_COUNT];
        a.records = (1..=6).map(|prn| Record { prn, ..r.clone() }).collect();
        a.padded_size = CAPACITY;
        a.encode().unwrap()
    }
    #[test]
    fn health_guard_covers_all_four_weeks_and_never_restores_removed_prns() {
        use super::super::health::{Event, Notice, Snapshot};
        let start = data::epoch("2026-10-14T00:00:00").unwrap();
        let now = start + 20. * DAY;
        let mut snapshot = Snapshot {
            schema_version: 1,
            refresh_ok: true,
            checked_at_gps: now,
            latest_observed_gps: now,
            latest_observed_by_prn: (1..=6).map(|p| (p.to_string(), now)).collect(),
            notices: vec![],
            orbit_events: vec![],
            refresh_error: None,
            latest_nanu: String::new(),
        };
        snapshot.orbit_events.push(Event {
            prn: 2,
            first_gps: start - 2. * DAY,
            last_gps: start - 2. * DAY,
            peak_innovation_m: 500.,
            samples: 1,
            reason: "Needs a new clean fit".into(),
        });
        snapshot.notices.push(Notice {
            number: "2026001".into(),
            kind: "FCSTDV".into(),
            reference: None,
            issued_gps: now,
            issued_from_dtg: true,
            prn: Some(3),
            svn: None,
            start_gps: Some(start + 25. * DAY),
            stop_gps: Some(start + 26. * DAY),
            review_required: false,
            mentioned_prns: vec![3],
        });
        let (guarded, report) = guard(&candidate(start), start - 3. * DAY, &snapshot, now).unwrap();
        let prns: Vec<_> = Archive::decode(&guarded)
            .unwrap()
            .records
            .iter()
            .map(|r| r.prn)
            .collect();
        assert_eq!(prns, vec![1, 4, 5, 6]);
        assert_eq!(report["upload_allowed"], true);
        assert_eq!(
            guard(&guarded, start - 3. * DAY, &snapshot, now).unwrap().0,
            guarded
        );
        assert_eq!(
            report["available_prns_by_week"].as_array().unwrap().len(),
            4
        );
        assert_eq!(
            guard(
                &candidate(start),
                start - 3. * DAY,
                &snapshot,
                start + 28. * DAY
            )
            .unwrap()
            .1["upload_allowed"],
            false
        );
        snapshot.refresh_ok = false;
        let (_, report) = guard(&candidate(start), start - 3. * DAY, &snapshot, now).unwrap();
        assert_eq!(report["upload_allowed"], false);
    }
}
