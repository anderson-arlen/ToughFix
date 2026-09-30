#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Rebuild README figures from saved research metrics. Not an app dependency.

Run from the checkout with Matplotlib installed:
    MPLCONFIGDIR=/tmp/toughfix-matplotlib .venv/bin/python docs/generate_figures.py
SVGs go to docs/figures; PNG previews go to /tmp for visual inspection.
"""
import json
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib.ticker import FuncFormatter

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / "docs/figures"
OUT.mkdir(parents=True, exist_ok=True)
TEAL, PURPLE, AMBER = "#087f8c", "#7954b3", "#d86532"
INK, MUTED, GRID, PAPER = "#172d40", "#546879", "#dfe7ed", "#f8fafc"
plt.rcParams.update({
    "font.family": "DejaVu Sans", "font.size": 11,
    "text.color": INK, "axes.labelcolor": MUTED,
    "xtick.color": MUTED, "ytick.color": MUTED,
    "axes.edgecolor": GRID, "axes.spines.top": False,
    "axes.spines.right": False, "axes.titleweight": "bold",
    "svg.fonttype": "none", "svg.hashsalt": "toughfix-readme",
    "savefig.facecolor": PAPER,
})


def read(name):
    return json.loads((OUT / "benchmark-data.json").read_text())[name]


def save(fig, name):
    fig.savefig(OUT / f"{name}.svg", metadata={"Date": None})
    fig.savefig(Path("/tmp") / f"toughfix-{name}.png", dpi=150)
    plt.close(fig)


def observed():
    m = read("observed_cep")
    fig, axes = plt.subplots(1, 2, figsize=(12, 5.2), facecolor=PAPER)
    fig.subplots_adjust(left=.065, right=.965, bottom=.24, top=.73, wspace=.31)
    fig.text(.065, .92, "Encoded assistance, compared with observed GPS data",
             size=18, weight="bold")
    fig.text(.065, .853, "Same 27 satellites · 1,080 matched samples · September 28, 14:00–23:45 GPS",
             size=11, color=MUTED)
    panels = [
        ("Satellite orbit error", "3D satellite-position error (metres)",
         "decoded_vs_observed_common", "nikon_vs_observed_common", "m"),
        ("Satellite clock error", "Absolute satellite-clock error (nanoseconds)",
         "clock_vs_observed_common_ns", "nikon_clock_vs_observed_common_ns", "ns"),
    ]
    for ax, (title, label, own_key, nikon_key, unit) in zip(axes, panels):
        ax.set_facecolor(PAPER)
        ax.set_title(title, loc="left", pad=16)
        series = []
        for offset, key, color, name in [
            (-.19, own_key, TEAL, "ToughFix native CEP"),
            (.19, nikon_key, PURPLE, "Nikon specimen"),
        ]:
            values = [m[key][f"{stat}_{unit}"] for stat in ("median", "p95")]
            bars = ax.bar([offset, 1 + offset], values, width=.34,
                          color=color, label=name, zorder=3)
            ax.bar_label(bars, labels=[f"{v:.2f}" for v in values],
                         padding=5, fontsize=11, color=INK)
            series.extend(values)
        ax.set_ylim(0, max(series) * 1.3)
        ax.set_xticks([0, 1], ["Median", "95th percentile"])
        ax.set_ylabel(label, labelpad=9)
        ax.yaxis.grid(True, color=GRID, zorder=0)
        ax.spines["left"].set_visible(False)
        ax.tick_params(axis="both", length=0, pad=7)
    handles, labels = axes[0].get_legend_handles_labels()
    fig.legend(handles, labels, loc="lower left", bbox_to_anchor=(.058, .105),
               ncol=2, frameon=False, fontsize=11)
    fig.text(.065, .068, "Finished Rust predictor rerun September 30; both outputs include CEP encoding loss.",
             color=MUTED, size=10)
    fig.text(.065, .025, "Short-window satellite errors, not camera location errors or proof of two-week superiority.",
             color=MUTED, size=10)
    save(fig, "observed-comparison")


def forecasts():
    current, historical = read("current"), read("historical")
    fig, axes = plt.subplots(1, 2, figsize=(12, 5.5), facecolor=PAPER)
    fig.subplots_adjust(left=.075, right=.965, bottom=.265, top=.735, wspace=.32)
    fig.text(.075, .93, "Two weeks of prediction: agreement is not accuracy",
             size=18, weight="bold")
    fig.text(.075, .865, "Finished native predictor · decoded CEP · frozen forecasts, without live health updates",
             size=11, color=MUTED)
    panels = [
        (current, "vs_nikon", "Decoded ToughFix vs Nikon", "3D separation (metres)",
         [("median_m", "Median", TEAL), ("p95_m", "95th percentile", PURPLE)]),
        (historical, "vs_observed", "Historical forecast vs observations", "3D orbit error (log scale)",
         [("median_m", "Median", TEAL), ("p95_m", "95th percentile", PURPLE),
          ("max_m", "Maximum", AMBER)]),
    ]
    for ax, (data, key, title, label, series) in zip(axes, panels):
        ax.set_facecolor(PAPER)
        ax.set_title(title, loc="left", pad=16)
        days = [d["day"] for d in data["daily"]]
        for stat, legend, color in series:
            values = [d[key][stat] for d in data["daily"]]
            ax.plot(days, values, color=color, linewidth=2.5,
                    marker="o", markersize=3.7, label=legend, zorder=3)
        ax.set_xlim(.7, 14.3)
        ax.set_xticks([1, 3, 7, 10, 14])
        ax.set_xlabel("Day of forecast")
        ax.set_ylabel(label, labelpad=9)
        ax.grid(axis="y", color=GRID, linewidth=.8)
        ax.tick_params(length=0, pad=7)
        ax.legend(loc="upper left", frameon=False, fontsize=9)
    axes[0].set_ylim(bottom=0)
    axes[1].legend(loc="lower right", frameon=False, fontsize=9)
    axes[1].set_yscale("log")
    axes[1].set_ylim(.1, 1_000_000)
    axes[1].set_yticks([.1, 1, 10, 100, 1000, 10000, 100000, 1000000])
    axes[1].yaxis.set_major_formatter(FuncFormatter(
        lambda v, _: f"{v / 1000:g} km" if v >= 1000 else f"{v:g} m"))
    fig.text(.075, .145, "Left: September 28, 12:00 start; 27 common PRNs, five-minute samples. Differences are not truth.",
             color=MUTED, size=10)
    fig.text(.075, .093, "Right: September 14, 12:00 start; all 32 PRNs, 15-minute observed samples. Maneuver failures retained.",
             color=MUTED, size=10)
    fig.text(.075, .041, "Retrospective Earth orientation; nominal IGS publication schedules, NOAA arrival times not reconstructed.",
             color=MUTED, size=10)
    save(fig, "forecast-behavior")


def freshness():
    cases = read("freshness_comparison")
    fig, axes = plt.subplots(1, 2, figsize=(12, 5.5), facecolor=PAPER)
    fig.subplots_adjust(left=.075, right=.965, bottom=.27, top=.735, wspace=.30)
    fig.text(.075, .93, "Fresher observations improve encoded orbit predictions",
             size=18, weight="bold")
    fig.text(.075, .865, "Same 31 satellites per pair · 2,976 first-day samples · 36-hour fresher orbit inputs",
             size=11, color=MUTED)
    for ax, key, unit, title, label in [
        (axes[0], "orbit", "m", "Median satellite orbit error", "3D error (metres)"),
        (axes[1], "clock", "ns", "Median satellite clock error", "Absolute clock error (nanoseconds)"),
    ]:
        ax.set_facecolor(PAPER)
        ax.set_title(title, loc="left", pad=16)
        all_values = []
        for offset, mode, color, name in [
            (-.19, "rapid", PURPLE, "Rapid only"),
            (.19, "updated", TEAL, "Current ToughFix"),
        ]:
            values = [c[key][mode][f"median_{unit}"] for c in cases]
            bars = ax.bar([i+offset for i in range(3)], values, width=.34,
                          color=color, label=name, zorder=3)
            ax.bar_label(bars, labels=[f"{v:.2f}" for v in values], padding=5, color=INK)
            all_values.extend(values)
        ax.set_ylim(0, max(all_values)*1.25)
        ax.set_xticks(range(3), ["Sept 15", "Sept 20", "Sept 25"])
        ax.set_ylabel(label, labelpad=9)
        ax.yaxis.grid(True, color=GRID, zorder=0)
        ax.spines["left"].set_visible(False)
        ax.tick_params(length=0, pad=7)
    handles, labels = axes[0].get_legend_handles_labels()
    fig.legend(handles, labels, loc="lower left", bbox_to_anchor=(.067, .135),
               ncol=2, frameon=False)
    fig.text(.075, .099, "September 15: newer clocks rejected by past-data quality gates; Rapid clock fallback used.",
             color=MUTED, size=10)
    fig.text(.075, .061, "Medians retain maneuver failures: September 15 maximum orbit error is about 42 km in both forecasts.",
             color=MUTED, size=10)
    fig.text(.075, .023, "Three retrospective cases; nominal publication times, retrospective Earth orientation, no live health guard.",
             color=MUTED, size=10)
    save(fig, "source-freshness")


if __name__ == "__main__":
    observed()
    forecasts()
    freshness()
    print(f"Wrote README figures to {OUT}")
