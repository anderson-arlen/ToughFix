// SPDX-License-Identifier: MIT
//! TG-6 selector-3 state-vector assistance. Independent codec; no vendor inputs.
use super::data::{self, AU, DAY, Frames, GPS_JD, V3};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path};

pub const CAPACITY: usize = 262144;
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
    for i in 0..days.div_ceil(4) + 4 {
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

/// Export the independently fitted native states; do not silently linearize a
/// quadratic clock. Receiver-side conventions are documented in tg6-format.md.
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
    let available = metrics["available_prns_by_week"]
        .as_array()
        .context("Missing native coverage")?;
    ensure!(available.len() >= 2, "Missing native two-week coverage");
    let mut records = Vec::new();
    let mut exclusions = Vec::new();
    for s in diagnostics["satellites"]
        .as_array()
        .context("Missing native satellite diagnostics")?
    {
        let prn = s["prn"].as_u64().context("Missing native PRN")?;
        let attempt = (|| -> Result<Record> {
            ensure!(
                available[..2]
                    .iter()
                    .all(|w| w.as_array().is_some_and(|a| a.contains(&json!(prn)))),
                "Not usable throughout the native two-week forecast"
            );
            let clock = &s["clock"];
            let c: Vec<f64> = serde_json::from_value(clock["coefficients"].clone())?;
            ensure!(
                c.len() == 3 && clock["degree"] == 1 && c[2] == 0.,
                "Quadratic clock is not representable in the TG-6 linear format"
            );
            let rows = s["trajectory_2h_gcrs"]
                .as_array()
                .context("Missing two-hour native states")?;
            ensure!(rows.len() == 168, "Incomplete native TG-6 trajectory");
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
            Ok(Record {
                prn: u16::try_from(prn)?,
                clock_epoch: ticks(start)?,
                bias: integer(c[0] - c[1] * 10800., 1e12)?,
                rate: integer(c[1], 1e20)?,
                state_epoch: ticks(start)?,
                states,
            })
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
        tables: ancillary(start, 14, &Frames::read(eop)?)?,
        records,
        padded_size: CAPACITY,
    };
    let bytes = archive.encode()?;
    let mut report = metrics.clone();
    report["format"] = json!("tg6-state-vectors-v1");
    report["bytes"] = json!(bytes.len());
    report["sha256"] = json!(crate::camera::hash(&bytes));
    report["format_exclusions"] = json!(exclusions);
    let prns = archive.records.iter().map(|r| r.prn).collect::<Vec<_>>();
    report["available_prns_by_week"] = json!([prns, prns]);
    report["upload_allowed"] = json!(false);
    report["model_warning"] = json!(
        "TG-6 candidate requires receiver validation and live satellite-health authorization"
    );
    report["broadcast_approximation"] = Value::Null;
    report["archive_tail"] = json!("Two weeks of two-hour inertial state vectors");
    Ok((bytes, report))
}

/// A TG-6 record covers the full two weeks, so retain it only if the same
/// satellite passes the native health guard for BOTH weeks. This deliberately
/// shares the existing outage/discontinuity policy rather than duplicating it.
pub fn guard(bytes: &[u8], base: &[u8], base_guard: &Value) -> Result<(Vec<u8>, Value)> {
    use super::validation;
    validation::validate(base)?;
    let mut archive = Archive::decode(bytes)?;
    ensure!(
        archive.start() == validation::start(base)? && archive.end() == archive.start() + 14. * DAY,
        "TG-6/base prediction epochs differ"
    );
    let before: Vec<_> = archive.records.iter().map(|r| r.prn).collect();
    archive.records.retain(|r| {
        (0..2).all(|week| {
            let pos = week * validation::BLOCK + 6 + (r.prn as usize - 1) * validation::RECORD;
            base[pos + 1] == 0
        })
    });
    ensure!(
        !archive.records.is_empty(),
        "Satellite health removed every TG-6 record"
    );
    let retained: Vec<_> = archive.records.iter().map(|r| r.prn).collect();
    let guarded = archive.encode()?;
    let mut report = base_guard.clone();
    report["available_prns_by_week"] = json!([retained, retained]);
    report["removed_tg6_prns"] = json!(
        before
            .iter()
            .filter(|p| !retained.contains(p))
            .collect::<Vec<_>>()
    );
    let mut failures = base_guard["failures"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if retained.len() < 4 {
        failures.push(json!("Fewer than four safe, representable TG-6 satellites"));
    }
    report["upload_allowed"] =
        json!(base_guard["upload_allowed"] == true && failures.is_empty() && retained.len() >= 4);
    report["failures"] = json!(failures);
    report["output_sha256"] = json!(crate::camera::hash(&guarded));
    report["validation"] = archive.inspect();
    report["rule"] = json!(
        "Retain only satellites eligible in both native guarded weeks; never restore removed records without a new fit"
    );
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
mod tests {
    use super::*;
    const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/tg6-state-vectors.bin");

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
        let bias = integer(a - b * 10800., 1e12).unwrap();
        let rate = integer(b, 1e20).unwrap();
        for dt in [0., 86400., 14. * DAY] {
            let original = a + b * (dt - 10800.);
            let encoded = bias as f64 * 1e-12 + rate as f64 * 1e-20 * dt;
            assert!((encoded - original).abs() < 0.6e-12);
        }
        assert!(integer(f64::NAN, 1.).is_err());
        assert!(integer(2147483648., 1.).is_err());
        assert!(ticks(1475971201.).is_err());
    }
    pub(super) fn candidate(start: f64) -> Vec<u8> {
        let mut a = Archive::decode(FIXTURE).unwrap();
        let mut r = a.records[0].clone();
        r.state_epoch = ticks(start).unwrap();
        r.clock_epoch = r.state_epoch;
        r.states = vec![r.states[0]; 168];
        a.records = (1..=6).map(|prn| Record { prn, ..r.clone() }).collect();
        a.padded_size = CAPACITY;
        a.encode().unwrap()
    }
    #[test]
    fn health_guard_requires_both_weeks_and_never_restores_removed_prns() {
        use super::super::{cep, validation};
        let start = data::epoch("2026-10-14T00:00:00").unwrap();
        let base = include_bytes!("../../tests/fixtures/working-camera.cep");
        let mut weeks: Vec<Vec<Vec<u8>>> = (0..4)
            .map(|w| {
                (0..32)
                    .map(|p| {
                        let at = w * validation::BLOCK + 6 + p * validation::RECORD;
                        base[at..at + validation::RECORD].to_vec()
                    })
                    .collect()
            })
            .collect();
        weeks[1][1] = cep::unavailable();
        let base = cep::archive(start, &weeks).unwrap();
        let (guarded, report) = guard(
            &candidate(start),
            &base,
            &json!({"upload_allowed":true,"failures":[]}),
        )
        .unwrap();
        let prns: Vec<_> = Archive::decode(&guarded)
            .unwrap()
            .records
            .iter()
            .map(|r| r.prn)
            .collect();
        assert_eq!(prns, vec![1, 3, 4, 5, 6]);
        assert_eq!(report["upload_allowed"], true);
        assert_eq!(guard(&guarded, &base, &report).unwrap().0, guarded);
        assert!(guard(&candidate(start + 7200.), &base, &report).is_err());
        let (_, report) = guard(
            &candidate(start),
            &base,
            &json!({"upload_allowed":false,"failures":["stale health"]}),
        )
        .unwrap();
        assert_eq!(report["upload_allowed"], false);
    }
}
