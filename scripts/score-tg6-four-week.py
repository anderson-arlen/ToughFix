#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Score frozen TG-6 linear clocks against 28 days of withheld observations.

No downloads or camera access. Clock errors are not camera position errors.
"""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path

import numpy as np

spec = importlib.util.spec_from_file_location(
    'clock_scoring', Path(__file__).with_name('score-clock-audit.py'))
scoring = importlib.util.module_from_spec(spec)
spec.loader.exec_module(scoring)
HORIZONS = (1, 3, 7, 14, 28)


def encoded_coefficients(clock):
    a, b, c = clock['coefficients']
    if clock['degree'] != 1 or c != 0:
        raise ValueError('Expected the production linear clock model')
    lookback = 2 * scoring.DAY
    bias, rate = round((a - b * (10800 + lookback)) * 1e12), round(b * 1e20)
    if not all(-2**31 <= v < 2**31 for v in (bias, rate)):
        return None
    return bias * 1e-12 + rate * 1e-20 * lookback, rate * 1e-20, 0.


def evaluate(directory, frozen):
    if frozen.get('schema_version') != 2 or frozen.get('forecast_days') != 28:
        raise ValueError('Expected frozen production four-week forecasts')
    times, observations = scoring.load_clocks(directory, frozen)
    rows = []
    rejected = []
    for case in frozen['forecasts']:
        clock, cutoff, prn = case['current'], case['cutoff_gps'], case['prn']
        if clock is None:
            rejected.append(case)
            continue
        if clock['training_end_gps_seconds'] >= cutoff:
            raise ValueError('Future observations influenced a fitted clock')
        coefficients = encoded_coefficients(clock)
        if coefficients is None:
            rejected.append(dict(case, current_error='Clock exceeds signed integer range'))
            continue
        row = dict(prn=prn, cutoff_gps=cutoff, source=clock['source'])
        for days in HORIZONS:
            value = scoring.score(times, observations[:, prn - 1], coefficients,
                                  cutoff, cutoff, cutoff + days * scoring.DAY)
            row[f'{days}d'] = value
        rows.append(row)
    complete = [r for r in rows if all(r[f'{days}d'] is not None for days in HORIZONS)]
    summaries = {}
    for days in HORIZONS:
        scored = [r[f'{days}d'] for r in complete]
        if not scored:
            raise ValueError(f'No complete observed comparisons for {days} days')
        rms = np.array([r['rms_ns'] for r in scored])
        maximum = np.array([r['max_ns'] for r in scored])
        summaries[f'{days}d'] = dict(
            forecasts=len(scored), median_rms_ns=float(np.median(rms)),
            p95_rms_ns=float(np.percentile(rms, 95)),
            maximum_absolute_ns=float(np.max(maximum)),
            fraction_rms_over_100_ns=float(np.mean(rms > 100)),
            fraction_max_over_300_ns=float(np.mean(maximum > 300)))
    dates = sorted(set(c['cutoff_gps'] for c in frozen['forecasts']))
    coverage = [sum(r['cutoff_gps'] == cutoff for r in rows) for cutoff in dates]
    return dict(schema_version=1, forecast_days=28, summary=summaries,
                matched_forecasts=len(complete), available_comparisons={f'{days}d':sum(r[f'{days}d'] is not None for r in rows) for days in HORIZONS},
                encoded_clock_coverage=dict(minimum=min(coverage), median=float(np.median(coverage)),
                                            maximum=max(coverage), cutoffs=len(dates)),
                rejected_forecasts=len(rejected), fitter_sha256=frozen['fitter_sha256'],
                encoder_sha256=hashlib.sha256((Path(__file__).resolve().parents[1] / 'src/predictor/tg6.rs').read_bytes()).hexdigest(),
                sources=frozen['sources'], cases=rows, exclusions=rejected,
                limitations=[
                    'Retrospective clock-only study using observed Rapid products, not a live product-arrival replay.',
                    'Daily windows overlap; forecasts are not independent trials.',
                    'Clock errors do not measure receiver acquisition, orbital accuracy, sky visibility, or camera positioning.',
                    'No common clock offset or satellite outliers are removed.'])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--data-dir', type=Path, required=True)
    parser.add_argument('--forecasts', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    report = evaluate(args.data_dir, json.loads(args.forecasts.read_text()))
    args.output.write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps({key: report[key] for key in
                      ('summary', 'encoded_clock_coverage', 'rejected_forecasts')}, indent=2))


if __name__ == '__main__':
    main()
