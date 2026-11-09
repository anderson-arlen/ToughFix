# Test fixtures

These small fixtures keep the native tests self-contained. Tests do not need
the investigation archive, a camera, or network access.

- `working-camera.cep`: ToughFix-generated assistance with 31 available PRNs
  in each of two populated weeks. Used for decoding, corruption, health, and
  preflight regressions. This is dated test data, not a current upload candidate.
- `cep-encoder.json`: parameters and expected bytes from the independent
  reference encoder, used to check the Rust bit writer.
- `health-parity.json`: 80 public NAVCEN notices and expected lifecycle results,
  plus 22 health/guard cases with byte hashes from the independent reference policy.
- `navcen-opsadvisory.oa1`, `navcen-current.nnu`, `celestrak-gps-index.json`,
  and `celestrak-status.txt`: public satellite-health responses retrieved on
  November 8, 2026. They reproduce NAVCEN's `UNCLASSIFIED` prefix and a
  CelesTrak rolling directory with daily advisories but no NANU entries.
  Sources: NAVCEN's `sites/default/files/gps/opsadvisory/current_opsadvisory.oa1`
  and `sites/default/files/gps/nanu/current_nanu.nnu`, plus CelesTrak's
  `GPS/jsonDir.php` and `GPS/status/2026/status.20261108.txt`.
- `orbit-discontinuity.json`: 49 observed PRN 15 samples on September 25, 2026,
  from the named NOAA-distributed IGS rapid product. It checks discontinuity
  detection, event merging, and exclusion of future observations.
- `orbit-discontinuity-eop.txt`: five USNO Earth-orientation rows needed for
  that test. Original source: `https://maia.usno.navy.mil/ser7/finals2000A.all`.
- `egm96-degree8.zip`: public NGA EGM96 coefficient rows through degree 8,
  plus the first degree-9 row, independently compressed with Python's `zipfile`.
  A preceding text member checks selection by name. It verifies DEFLATE reading
  and coefficient values across ZIP dependency upgrades. Original source:
  `https://earth-info.nga.mil/php/download.php?file=egm-96spherical`.
- `commit-framing.json`: a 12-byte command and 12-byte acknowledgement from
  the successful TG-1 transfer, with no device identity, serial, location, or
  assistance payload. Used to check PTP framing against a hardware example.

Generated test artifacts and expectations are original ToughFix work. Public
notice, orbit, and Earth-orientation excerpts retain their source terms; see
[NOTICE.md](../../NOTICE.md). No vendor assistance file or firmware is included.
## TG-6 state-vector fixture

`tg6-state-vectors.bin` contains independently generated astronomy tables and
the first two native PRN 1 states from the 2026-10-14 18:00 GPS experiment.
It was encoded with the independent Python reference, with one satellite and
zero padding omitted. It is a 1136-byte codec fixture, not an upload candidate.
It contains no vendor assistance bytes. SHA-256:
`36a654916acd8c49fb951b9a6810e28e44fb6b4ca3572fad635d94dde730a2b6`.
