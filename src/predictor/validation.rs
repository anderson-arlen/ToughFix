// SPDX-License-Identifier: MIT
//! Independent packed-record reader translated from the recovered receiver loader.
use anyhow::{Result, ensure};
use serde_json::{Value, json};

pub const BLOCK: usize = 32680;
pub const RECORD: usize = 1021;
pub const WEEK: f64 = 604800.;
const WIDTH: [usize; 15] = [16, 16, 16, 16, 24, 24, 12, 16, 12, 12, 12, 12, 12, 12, 12];
const SCALE: [i32; 15] = [
    -19, -33, -31, -31, -31, -31, -43, -43, -43, -5, -5, -29, -29, -29, -29,
];

fn signed(data: &[u8], at: usize, width: usize) -> Result<i64> {
    ensure!(
        width > 0 && width < 63 && at + width <= data.len() * 8,
        "CEP bit field is out of bounds"
    );
    let mut value = 0i64;
    for i in at..at + width {
        value = (value << 1) | ((data[i / 8] >> (7 - i % 8)) & 1) as i64;
    }
    if value & (1 << (width - 1)) != 0 {
        value -= 1 << width;
    }
    Ok(value)
}

/// Broadcast units, including semicircles, exactly as the packed loading stage.
pub fn decode(record: &[u8], interval: usize) -> Result<[f64; 15]> {
    ensure!(
        record.len() == RECORD && record[1] == 0 && interval < 28,
        "Unavailable or malformed CEP record"
    );
    let root = signed(record, 96, 34)? as f64 * 2f64.powi(-19);
    ensure!(root > 0., "Nonpositive CEP reference axis");
    let phase = 398600500000000f64.sqrt() * interval as f64 * 21600. / root.powi(3);
    let mut position = 96;
    let mut result = [0.; 15];
    for k in 0..15 {
        let mut q = [0.; 4];
        for (value, width) in q.iter_mut().zip([34, 34, 24, 20]) {
            *value = signed(record, position, width)? as f64;
            position += width;
        }
        let a = signed(record, position, 4)? as f64;
        let b = signed(record, position + 4, 4)? as f64;
        position += 8;
        let residual = signed(record, position + interval * WIDTH[k], WIDTH[k])? as f64;
        position += 28 * WIDTH[k];
        let t = interval as f64;
        result[k] = (residual
            + ((q[3] / 256. * t + q[2]) * t + q[1]) * t
            + q[0]
            + (a * phase.sin() + b * phase.cos()) / 4096.)
            * 2f64.powi(SCALE[k]);
    }
    ensure!(position == RECORD * 8, "CEP reader did not consume record");
    Ok(result)
}

pub fn radians(record: &[u8], interval: usize) -> Result<[f64; 15]> {
    let mut p = decode(record, interval)?;
    for v in &mut p[2..9] {
        *v *= std::f64::consts::PI;
    }
    Ok(p)
}

pub fn crc(bytes: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &byte in bytes {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

pub fn start(bytes: &[u8]) -> Result<f64> {
    ensure!(
        bytes.len() == 4 * BLOCK,
        "CEP archive must be exactly 130720 bytes"
    );
    Ok(u32::from_be_bytes(bytes[..4].try_into().unwrap()) as f64)
}

pub fn validate(bytes: &[u8]) -> Result<Value> {
    let start = start(bytes)?;
    let mut counts = [0usize; 4];
    for (week, block) in bytes.as_chunks::<BLOCK>().0.iter().enumerate() {
        ensure!(
            u32::from_be_bytes(block[..4].try_into().unwrap()) as f64 == start + week as f64 * WEEK
                && block[4] as usize == week
                && block[5] == 4,
            "CEP week {} has inconsistent headers",
            week + 1
        );
        ensure!(
            crc(&block[..BLOCK - 2]) == u16::from_be_bytes(block[BLOCK - 2..].try_into().unwrap()),
            "CEP week {} failed CRC",
            week + 1
        );
        for prn in 1..=32 {
            let at = 6 + (prn - 1) * RECORD;
            let record = &block[at..at + RECORD];
            if record[1] != 0 {
                ensure!(
                    record[0] == 0 || record[0] as usize == prn,
                    "Invalid unavailable PRN slot"
                );
                continue;
            }
            ensure!(
                record[0] as usize == prn && week < 2,
                "Invalid available PRN slot or populated archive tail"
            );
            counts[week] += 1;
            for interval in 0..28 {
                let p = decode(record, interval)?;
                ensure!(
                    p.iter().all(|v| v.is_finite())
                        && p[0] > 5100.
                        && p[0] < 5200.
                        && (0. ..0.1).contains(&p[1])
                        && p[2] > 0.25
                        && p[2] < 0.4
                        && p[6..9].iter().all(|v| v.abs() < 1e-6)
                        && p[9..11].iter().all(|v| v.abs() < 3000.)
                        && p[11..].iter().all(|v| v.abs() < 1e-3),
                    "Implausible CEP ephemeris week {}, G{prn:02}, interval {interval}",
                    week + 1
                );
            }
        }
    }
    Ok(
        json!({"bytes":bytes.len(),"sha256":crate::camera::hash(bytes),"available_records_by_week":counts,"checked_ephemerides":counts.iter().sum::<usize>()*28,"weekly_crcs":"passed","headers_and_record_slots":"passed","physical_ranges":"passed"}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn independent_reader_accepts_real_working_archive_and_rejects_corruption() {
        let bytes = include_bytes!("../../tests/fixtures/working-camera.cep");
        let report = validate(bytes).unwrap();
        assert_eq!(report["available_records_by_week"], json!([31, 31, 0, 0]));
        let mut bad = bytes.to_vec();
        bad[80] ^= 1;
        assert!(validate(&bad).is_err());
        bad = bytes.to_vec();
        bad[4] = 1;
        assert!(validate(&bad).is_err());
        assert!(validate(&bytes[..bytes.len() - 1]).is_err());
        bad = bytes.to_vec();
        bad[6] = 2;
        let crc = crc(&bad[..BLOCK - 2]);
        bad[BLOCK - 2..BLOCK].copy_from_slice(&crc.to_be_bytes());
        assert!(validate(&bad).is_err());
    }
}
