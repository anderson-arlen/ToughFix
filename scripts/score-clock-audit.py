#!/usr/bin/env python3
"""Score frozen production clock forecasts against subsequent observed IGS data.

Requires NumPy and Matplotlib for research only. Never accesses a camera,
downloads inputs, changes thresholds, or updates operational state.
"""
import argparse
import csv
from datetime import datetime, timedelta
import gzip
import hashlib
import json
import os
from pathlib import Path

import numpy as np

GPS = datetime(1980, 1, 6)
DAY = 86400
HORIZONS = (1, 3, 7, 14)


def calendar(seconds):
    return (GPS + timedelta(seconds=float(seconds))).isoformat()


def load_clocks(directory, frozen):
    first, end = frozen['dataset_start_gps'], frozen['dataset_end_exclusive_gps']
    times = np.arange(first, end, 900.)
    clocks = np.full((len(times), 32), np.nan)
    for item in frozen['sources']:
        path = directory / item['file']
        if hashlib.sha256(path.read_bytes()).hexdigest() != item['sha256']:
            raise ValueError(f'Changed input since forecasts were frozen: {path.name}')
        lines = gzip.decompress(path.read_bytes()).decode('ascii').splitlines()
        if not lines[0].startswith('#') or not any(s.startswith('%c') and 'GPS' in s.split() for s in lines[:22]):
            raise ValueError('SP3 must declare GPS time')
        file_start = (datetime.strptime(path.name[11:18], '%Y%j') - GPS).total_seconds()
        epochs = []
        index = None
        for line in lines:
            if line.startswith('* '):
                f = line.split()
                t = (datetime(*map(int, f[1:6])) - GPS).total_seconds() + float(f[6])
                epochs.append(t)
                index = int((t - first) / 900)
                if not 0 <= index < len(times) or times[index] != t:
                    raise ValueError('Nonintegral or out-of-range observation epoch')
            elif line.startswith('PG') and line[75:76] != 'P':
                prn, clock = int(line[2:4]), float(line[46:60])
                if index is not None and 1 <= prn <= 32 and np.isfinite(clock) and abs(clock) < 999999:
                    clocks[index, prn - 1] = clock * 1e-6
        if epochs != [file_start + i * 900 for i in range(96)]:
            raise ValueError(f'Product does not cover exactly its declared observation day: {path.name}')
    return times, clocks


def score(times, values, coefficients, reference, start, end):
    keep = (times >= start) & (times < end) & np.isfinite(values)
    t, observed = times[keep], values[keep]
    days = (end - start) / DAY
    if (len(t) < days * 80 or t[0] > start + 900 or t[-1] < end - 1800
            or np.any(np.diff(t) > 3600)):
        return None
    a, b, c = coefficients
    dt = t - reference
    errors = (a + dt * (b + dt * c) - observed) * 1e9
    return dict(samples=len(t), rms_ns=float(np.sqrt(np.mean(errors ** 2))),
                max_ns=float(np.max(np.abs(errors))))


def passed(value):
    return value['rms_ns'] <= 50 and value['max_ns'] <= 150


def encoded_coefficients(model):
    a, b, c = model['coefficients']
    if model['degree'] != 1 or c != 0:
        raise ValueError('Expected the production linear fitter')
    bias, rate = round((a - b * 10800) * 1e12), round(b * 1e20)
    if not all(-2**31 <= v < 2**31 for v in (bias, rate)):
        return None
    return bias * 1e-12, rate * 1e-20, 0.


def correlation(a, b):
    a, b = np.asarray(a), np.asarray(b)
    if len(a) < 3 or np.std(a) == 0 or np.std(b) == 0:
        return None
    return float(np.corrcoef(a, b)[0, 1])


def ranks(values):
    values = np.asarray(values)
    unique, inverse, count = np.unique(values, return_inverse=True, return_counts=True)
    del unique
    return (np.cumsum(count) - (count - 1) / 2)[inverse]


def summarize(rows, horizon, include_missing_history=False):
    eligible = [r for r in rows if (include_missing_history or r.get('past_rms_ns') is not None)
                and r.get(f'future_{horizon}d_rms_ns') is not None]
    result = dict(pairs=len(eligible))
    for rejected in (False, True):
        group = [r for r in eligible if r['past_rejected'] == rejected]
        bad = sum(r[f'future_{horizon}d_failed'] for r in group)
        errors = [r[f'future_{horizon}d_rms_ns'] for r in group]
        result['previously_failed' if rejected else 'previously_passed'] = dict(
            pairs=len(group), future_failed=bad, future_passed=len(group) - bad,
            failure_rate=bad / len(group) if group else None,
            median_rms_ns=float(np.median(errors)) if errors else None)
    numeric = [r for r in eligible if r.get('past_rms_ns') is not None]
    result['spearman_rms'] = correlation(ranks([r['past_rms_ns'] for r in numeric]),
                                         ranks([r[f'future_{horizon}d_rms_ns'] for r in numeric]))
    result['recent_past_day_rms_spearman'] = correlation(ranks([r['recent_past_day_rms_ns'] for r in numeric]),
                                                        ranks([r[f'future_{horizon}d_rms_ns'] for r in numeric]))
    x = np.log10(np.maximum(1e-9, [r['past_rms_ns'] for r in numeric]))
    y = np.log10(np.maximum(1e-9, [r[f'future_{horizon}d_rms_ns'] for r in numeric]))
    prns = np.array([r['prn'] for r in numeric])
    for prn in np.unique(prns):
        x[prns == prn] -= x[prns == prn].mean()
        y[prns == prn] -= y[prns == prn].mean()
    result['within_satellite_log_rms_correlation'] = correlation(x, y)
    result['by_prn'] = []
    for prn in sorted(set(prns)):
        group = [r for r in numeric if r['prn'] == prn]
        a = [r['past_rms_ns'] for r in group]
        b = [r[f'future_{horizon}d_rms_ns'] for r in group]
        result['by_prn'].append(dict(prn=int(prn), pairs=len(group),
            rank_correlation=correlation(ranks(a), ranks(b)),
            past_failure_rate=float(np.mean([r['past_rejected'] for r in group])),
            future_failure_rate=float(np.mean([r[f'future_{horizon}d_failed'] for r in group]))))
    return result


def plot(rows, output):
    os.environ.setdefault('MPLCONFIGDIR', '/tmp/tg-agps-matplotlib')
    import matplotlib
    matplotlib.use('Agg')
    import matplotlib.pyplot as plt
    fig, axes = plt.subplots(1, 2, figsize=(11, 4.6), constrained_layout=True)
    valid = [r for r in rows if r.get('past_rms_ns') is not None and r.get('future_14d_rms_ns') is not None]
    for rejected, label, color in [(False, 'Passed earlier backtest', '#287b9b'),
                                    (True, 'Failed earlier backtest', '#be573c')]:
        group = [r for r in valid if r['past_rejected'] == rejected]
        axes[0].scatter([r['past_rms_ns'] for r in group], [r['future_14d_rms_ns'] for r in group],
                        s=9, alpha=.32, color=color, label=label, rasterized=True)
        rates = [summarize(rows, h)['previously_failed' if rejected else 'previously_passed']['failure_rate'] for h in HORIZONS]
        axes[1].plot(HORIZONS, np.array(rates) * 100, marker='o', color=color, label=label)
    axes[0].axvline(50, color='#666666', ls='--', lw=.8)
    axes[0].axhline(50, color='#666666', ls='--', lw=.8)
    axes[0].set(xscale='log', yscale='log', xlabel='Earlier 14-day forecast error (RMS ns)',
                ylabel='Following 14-day forecast error (RMS ns)', title='Does error persist after refitting?')
    axes[1].set(xlabel='Following forecast horizon (days)', ylabel='Forecasts exceeding error limits (%)',
                title='How much does the earlier failure predict?', xticks=HORIZONS, ylim=(0, 100))
    for ax in axes:
        ax.grid(alpha=.18)
    axes[1].legend(frameon=False, loc='best')
    fig.suptitle('TG-6 clock forecasts: historical failure versus subsequent accuracy', fontsize=13)
    fig.supxlabel(f'Failure: RMS > 50 ns or maximum > 150 ns. Same {len(valid):,} complete trials across horizons; daily windows overlap.', fontsize=8)
    fig.savefig(output.with_suffix('.png'), dpi=170)
    fig.savefig(output.with_suffix('.pdf'))
    plt.close(fig)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--data-dir', type=Path, required=True)
    parser.add_argument('--forecasts', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    frozen = json.loads(args.forecasts.read_text())
    if frozen['thresholds'] != dict(rms_ns=50, maximum_ns=150):
        raise ValueError('Scorer thresholds differ from the frozen production policy')
    times, clocks = load_clocks(args.data_dir, frozen)
    rows = []
    counts = dict(current_fit_failed=0, current_unencodable=0, historical_fit_failed=0, historical_coverage_missing=0)
    for item in frozen['forecasts']:
        if item['current'] is None:
            counts['current_fit_failed'] += 1
            continue
        cutoff, past_cutoff, prn = item['cutoff_gps'], item['historical_cutoff_gps'], item['prn']
        assert past_cutoff + 14 * DAY == cutoff
        assert item['current']['training_end_gps_seconds'] < cutoff
        coefficients = encoded_coefficients(item['current'])
        if coefficients is None:
            counts['current_unencodable'] += 1
            continue
        values = clocks[:, prn - 1]
        row = dict(cutoff=calendar(cutoff), cutoff_gps=cutoff, prn=prn,
                   recent_past_day_rms_ns=item['current']['past_day_validation_ns'][0],
                   recent_training_rms_ns=item['current']['training_rms_ns'])
        history = item['historical']
        past = None
        if history is None:
            counts['historical_fit_failed'] += 1
        else:
            assert history['training_end_gps_seconds'] < past_cutoff
            past = score(times, values, history['coefficients'], past_cutoff + 10800, past_cutoff, cutoff)
            if past is None:
                counts['historical_coverage_missing'] += 1
        row['past_rms_ns'] = None if past is None else past['rms_ns']
        row['past_max_ns'] = None if past is None else past['max_ns']
        row['past_rejected'] = past is None or not passed(past)
        if row['past_rejected'] == item['production_gate_passed']:
            raise ValueError('Independent scorer disagrees with the production acceptance gate')
        for horizon in HORIZONS:
            future = score(times, values, coefficients, cutoff, cutoff, cutoff + horizon * DAY)
            for field in ('rms_ns', 'max_ns'):
                row[f'future_{horizon}d_{field}'] = None if future is None else future[field]
            row[f'future_{horizon}d_failed'] = None if future is None else not passed(future)
        rows.append(row)
    summary = {f'{h}d': summarize(rows, h) for h in HORIZONS}
    common = [r for r in rows if r.get('past_rms_ns') is not None
              and all(r[f'future_{h}d_rms_ns'] is not None for h in HORIZONS)]
    dates = sorted(set(r['cutoff_gps'] for r in rows))
    disjoint_dates = set(dates[::31])
    disjoint = [r for r in rows if r['cutoff_gps'] in disjoint_dates]
    months = sorted(set(r['cutoff'][:7] for r in rows))
    result = dict(schema_version=1, forecast_method=frozen['engine'],
        dataset_start=calendar(frozen['dataset_start_gps']),
        dataset_end_exclusive=calendar(frozen['dataset_end_exclusive_gps']),
        first_forecast=calendar(dates[0]), last_forecast=calendar(dates[-1]), daily_cutoffs=len(dates),
        attempted_forecasts=len(frozen['forecasts']), current_eligible_forecasts=len(rows), exclusions=counts,
        thresholds=dict(rms_ns=50, maximum_ns=150), by_horizon=summary,
        common_cohort_by_horizon={f'{h}d': summarize(common, h) for h in HORIZONS},
        policy_including_unavailable_history={f'{h}d': summarize(rows, h, True) for h in HORIZONS},
        future_missing_given_numeric_history={f'{h}d':sum(r.get('past_rms_ns') is not None and r[f'future_{h}d_rms_ns'] is None for r in rows) for h in HORIZONS},
        disjoint_date_check=dict(dates=[calendar(t) for t in sorted(disjoint_dates)],
            by_horizon={f'{h}d': summarize(disjoint, h) for h in HORIZONS}),
        chronological_checks={month:{f'{h}d':summarize([r for r in rows if r['cutoff'].startswith(month)],h) for h in HORIZONS} for month in months},
        frozen_forecasts_sha256=hashlib.sha256(args.forecasts.read_bytes()).hexdigest(),
        fitter_sha256=frozen['fitter_sha256'], observation_reader_sha256=frozen['observation_reader_sha256'],
        production_gate_decisions_match=True,
        sources=frozen['sources'],
        limitations=[
            'Retrospective forecasts use only earlier observation epochs; original product publication delays are not reconstructed.',
            'Daily trials overlap and satellites recur; trial counts do not represent independent experiments.',
            'The 31-day-spaced sensitivity check uses disjoint observation windows but still repeats satellites.',
            'Primary paired comparisons require a valid earlier model and sufficient earlier observations; unavailable histories are counted separately.',
            'Current recent-fit and integer-range checks precede this analysis, matching the population affected by the historical gate.',
            'Thresholds were fixed from existing code before this study; they have not been validated against camera acquisition performance.',
            'No common clock offsets, satellite outliers, or individual prediction errors are removed.',
            'Clock-only study; orbit errors, satellite-health exclusions, PRN reassignment and receiver acquisition are not separately modelled.'])
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.with_suffix('.json').write_text(json.dumps(result, indent=2)+'\n')
    with args.output.with_suffix('.csv').open('w') as stream:
        writer = csv.DictWriter(stream, fieldnames=list(rows[0]))
        writer.writeheader()
        writer.writerows(rows)
    plot(common, args.output)
    print(json.dumps({k:result[k] for k in ('daily_cutoffs','attempted_forecasts','current_eligible_forecasts','exclusions')},indent=2))
    for horizon in HORIZONS:
        s = summary[f'{horizon}d']
        print(f'{horizon} days: '+json.dumps({k:v for k,v in s.items() if k not in ('by_prn',)}))


if __name__ == '__main__':
    main()
