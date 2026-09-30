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
- `orbit-discontinuity.json`: 49 observed PRN 15 samples on September 25, 2026,
  from the named NOAA-distributed IGS rapid product. It checks discontinuity
  detection, event merging, and exclusion of future observations.
- `orbit-discontinuity-eop.txt`: five USNO Earth-orientation rows needed for
  that test. Original source: `https://maia.usno.navy.mil/ser7/finals2000A.all`.
- `commit-framing.json`: a 12-byte command and 12-byte acknowledgement from
  the successful TG-1 transfer, with no device identity, serial, location, or
  assistance payload. Used to check PTP framing against a hardware example.

Generated test artifacts and expectations are original ToughFix work. Public
notice, orbit, and Earth-orientation excerpts retain their source terms; see
[NOTICE.md](../../NOTICE.md). No vendor assistance file or firmware is included.
