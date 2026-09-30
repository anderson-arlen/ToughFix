# README figures

These original figures are MIT licensed. Regenerate them from the checkout with
Matplotlib installed:

```sh
MPLCONFIGDIR=/tmp/toughfix-matplotlib .venv/bin/python docs/generate_figures.py
```

The script reads saved summary metrics and writes SVGs here, plus PNG previews
in `/tmp`. It does not download data, generate forecasts, or access a camera.
Python and Matplotlib are optional documentation tools, not app dependencies.

| Figure | Saved evidence | Interpretation |
| --- | --- | --- |
| [Observed comparison](observed-comparison.svg) | `observed_cep` and `observed_window` in [benchmark-data.json](benchmark-data.json) | Finished Rust predictor vs Nikon: matched 27-satellite orbit and clock errors against held-out IGS Rapid data over 9.75 hours; both files include encoding loss |
| [Forecast behavior](forecast-behavior.svg), left | `current` in [benchmark-data.json](benchmark-data.json) | Daily median and 95th-percentile separation between decoded native ToughFix CEP and decoded Nikon CEP; not accuracy against truth |
| [Forecast behavior](forecast-behavior.svg), right | `historical` in [benchmark-data.json](benchmark-data.json) | Decoded native CEP daily satellite-position errors against held-out observations, retaining all 32 PRNs and maneuver failures; logarithmic scale |
| [Source freshness](source-freshness.svg) | `freshness_comparison` in [benchmark-data.json](benchmark-data.json) | Three paired native forecasts, Rapid-only vs observed Ultra-rapid extension with per-satellite clock gates; first-day decoded medians on matched samples |

These runs were repeated with the finished Rust predictor on September 30, 2026.
Satellite errors do not directly measure camera location errors. Historical
input selection follows nominal IGS publication schedules but does not
reconstruct NOAA archive arrival times. Earth orientation is retrospective,
and these frozen accuracy experiments do not apply the live health guard.
Maneuver failures are retained in the saved metrics.
