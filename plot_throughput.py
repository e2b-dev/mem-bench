#!/usr/bin/env -S uv run
# /// script
# requires-python = ">=3.11"
# dependencies = ["numpy", "matplotlib"]
# ///
"""
Plot Criterion benchmark results as throughput and page-fault latency graphs.

Writes two PNGs into the output directory:

  throughput.png  — stacked subplots, one per selected page size (4k on top, 2m below)
    X axis : transfer size (log₂ scale)
    Y axis : throughput in GiB/s
    Lines  : one per benchmark function (method), plotted at the median
    Bands  : dark = p25–p75 (IQR), light = p5–p95

  page_fault.png  — page-fault latency per page (log Y axis)
    X axis : (backing type, page size) categories
    Y axis : latency per page fault in µs  (log scale)
    Bars   : median
    Errors : thick = p25–p75, thin capped = p5–p95

Use --page-size to restrict both plots to a single page size.

The path argument can be any level of the Criterion output tree:

  criterion                     → all groups
  criterion/4k                  → only the "4k" throughput group
  criterion/4k/mmap_to_mmap     → only that one function

Usage:
    uv run plot_throughput.py [criterion_dir] [--output DIR]

criterion_dir defaults to "criterion" in the current directory.
Cargo writes results to target/criterion; symlink or copy as needed.
"""

import argparse
import json
import sys
from collections import defaultdict
from pathlib import Path

import matplotlib.pyplot as plt
import matplotlib.ticker as ticker
import numpy as np

# ── CLI ───────────────────────────────────────────────────────────────────────


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    p.add_argument(
        "criterion_dir",
        nargs="?",
        default="criterion",
        help="Path to Criterion output directory (default: criterion)",
    )
    p.add_argument(
        "--output",
        "-o",
        default="plots",
        help="Directory to write PNG files into (default: plots)",
    )
    p.add_argument(
        "--page-size",
        choices=["4k", "2m"],
        default=None,
        help="Restrict plots to a single page size (default: show both)",
    )
    return p.parse_args()


# ── Helpers ───────────────────────────────────────────────────────────────────

GiB = 2**30


def fmt_bytes(n: int) -> str:
    """Short human-readable label for a byte count (binary prefixes)."""
    for unit, shift in (("G", 30), ("M", 20), ("K", 10)):
        v = n / 2**shift
        if v >= 1:
            return f"{v:.0f}{unit}" if v == int(v) else f"{v:.1f}{unit}"
    return str(n)


def percentile_stats(samples: np.ndarray) -> dict:
    return {
        "p5":    float(np.percentile(samples, 5)),
        "p25":   float(np.percentile(samples, 25)),
        "median": float(np.median(samples)),
        "p75":   float(np.percentile(samples, 75)),
        "p95":   float(np.percentile(samples, 95)),
    }


def per_iter_ns(new_dir: Path) -> np.ndarray | None:
    """
    Read sample.json and return per-iteration wall times in nanoseconds.
    Returns None if the files are missing.
    """
    sample_path = new_dir / "sample.json"
    if not sample_path.exists():
        return None
    sample = json.loads(sample_path.read_text())
    iters    = np.asarray(sample["iters"],  dtype=np.float64)
    times_ns = np.asarray(sample["times"],  dtype=np.float64)
    return times_ns / iters


# ── Throughput data loading (Bytes throughput) ────────────────────────────────


def collect_throughput_data(root: Path) -> dict:
    """
    Recursively find Criterion benchmarks with Bytes throughput and return:
        { group: { function: [ {size, p5, p25, median, p75, p95}, … ] } }
    Each inner list is sorted by size.

    group/function are read from benchmark.json, so root can be any depth.
    """
    data: dict = defaultdict(lambda: defaultdict(list))

    for new_dir in sorted(root.rglob("new")):
        if not new_dir.is_dir():
            continue

        benchmark_path = new_dir / "benchmark.json"
        if not benchmark_path.exists():
            continue

        meta = json.loads(benchmark_path.read_text())
        n_bytes = (meta.get("throughput") or {}).get("Bytes")
        if n_bytes is None:
            continue

        ns = per_iter_ns(new_dir)
        if ns is None:
            continue

        # bytes/ns × (1e9 ns/s) / (2^30 bytes/GiB) = GiB/s
        samples_gibs = n_bytes * (1e9 / GiB) / ns

        data[meta["group_id"]][meta["function_id"]].append(
            {"size": int(meta["value_str"]), **percentile_stats(samples_gibs)}
        )

    for group in data:
        for func in data[group]:
            data[group][func].sort(key=lambda r: r["size"])

    return data


# ── Page-fault data loading (Elements throughput) ─────────────────────────────


def collect_page_fault_data(root: Path) -> dict:
    """
    Recursively find Criterion benchmarks with Elements throughput and return
    per-page-fault latency in microseconds:
        { function_id: { value_str: {p5, p25, median, p75, p95} } }
    e.g. {"anon": {"4k": {…}, "2m": {…}}, "file": {"4k": {…}}, …}

    In the page_fault bench, function_id is the backing type ("anon", "file",
    "memfd") and value_str is the page-size label ("4k", "2m").
    """
    data: dict = defaultdict(dict)

    for new_dir in sorted(root.rglob("new")):
        if not new_dir.is_dir():
            continue

        benchmark_path = new_dir / "benchmark.json"
        if not benchmark_path.exists():
            continue

        meta = json.loads(benchmark_path.read_text())
        n_elements = (meta.get("throughput") or {}).get("Elements")
        if n_elements is None:
            continue

        ns = per_iter_ns(new_dir)
        if ns is None:
            continue

        # µs per individual page fault
        per_fault_us = ns / n_elements / 1_000.0

        data[meta["function_id"]][meta["value_str"]] = percentile_stats(per_fault_us)

    return dict(data)


# ── Plotting ──────────────────────────────────────────────────────────────────

PALETTE = [
    "#1f77b4",
    "#ff7f0e",
    "#2ca02c",
    "#d62728",
    "#9467bd",
    "#8c564b",
    "#e377c2",
    "#17becf",
]

_NO_DATA_STYLE = dict(ha="center", va="center", fontsize=12, color="gray",
                      transform=None)  # transform set at call site


def _no_data(ax, msg: str) -> None:
    ax.text(0.5, 0.5, msg, ha="center", va="center", fontsize=12, color="gray",
            transform=ax.transAxes)
    ax.set_axis_off()


def plot_throughput_ax(ax, series_map: dict, group_label: str) -> None:
    """Draw a throughput-vs-size line chart with percentile bands onto `ax`."""
    if not series_map:
        _no_data(ax, f"No throughput data for {group_label}")
        return

    for i, (func, series) in enumerate(sorted(series_map.items())):
        color = PALETTE[i % len(PALETTE)]
        sizes   = [r["size"]   for r in series]
        medians = [r["median"] for r in series]
        p5      = [r["p5"]     for r in series]
        p25     = [r["p25"]    for r in series]
        p75     = [r["p75"]    for r in series]
        p95     = [r["p95"]    for r in series]

        ax.plot(sizes, medians, "o-", color=color, label=func,
                linewidth=2, markersize=5, zorder=3)
        ax.fill_between(sizes, p25, p75, color=color, alpha=0.30, zorder=2)
        ax.fill_between(sizes, p5,  p95, color=color, alpha=0.12, zorder=1)

    all_sizes = sorted({r["size"] for s in series_map.values() for r in s})
    ax.set_xscale("log", base=2)
    ax.set_xticks(all_sizes)
    ax.set_xticklabels([fmt_bytes(s) for s in all_sizes], rotation=30, ha="right")
    ax.xaxis.set_minor_locator(ticker.NullLocator())

    ax.set_xlabel("Transfer size", fontsize=11)
    ax.set_ylabel("Throughput (GiB/s)", fontsize=11)
    ax.set_title(
        f"Memory copy throughput — {group_label} pages\n"
        "line = median  ·  dark band = p25–p75  ·  light band = p5–p95",
        fontsize=12,
    )
    ax.legend(title="Method", loc="best", fontsize=9)
    ax.grid(True, which="major", linestyle="--", alpha=0.4)
    ax.set_ylim(bottom=0)


def plot_page_fault_ax(ax, data: dict) -> None:
    """
    Draw a grouped bar chart of page-fault latency onto `ax`.

    Bars are grouped by page size; within each group one bar per backing type.
    Error bars show p25–p75 (thick) and p5–p95 (thin with caps).
    """
    if not data:
        _no_data(ax, "No page-fault data")
        return

    backings   = sorted(data.keys())
    page_sizes = sorted({ps for b in data.values() for ps in b})

    n_backings = len(backings)
    n_sizes    = len(page_sizes)
    bar_width  = 0.8 / n_backings
    group_pos  = np.arange(n_sizes, dtype=float)

    for i, backing in enumerate(backings):
        color  = PALETTE[i % len(PALETTE)]
        offset = (i - n_backings / 2 + 0.5) * bar_width

        xs, medians = [], []
        iqr_lo, iqr_hi = [], []   # p25–p75 (thick inner error)
        ext_lo, ext_hi = [], []   # p5–p95  (thin outer error)

        for j, ps in enumerate(page_sizes):
            if ps not in data[backing]:
                continue
            s = data[backing][ps]
            xs.append(group_pos[j] + offset)
            medians.append(s["median"])
            iqr_lo.append(s["median"] - s["p25"])
            iqr_hi.append(s["p75"]    - s["median"])
            ext_lo.append(s["median"] - s["p5"])
            ext_hi.append(s["p95"]    - s["median"])

        ax.bar(xs, medians, width=bar_width, color=color, alpha=0.8,
               label=backing, zorder=3)
        # IQR error bars (thick)
        ax.errorbar(xs, medians, yerr=[iqr_lo, iqr_hi],
                    fmt="none", color=color, capsize=6, linewidth=2.5, zorder=4)
        # p5–p95 error bars (thin, dashed caps)
        ax.errorbar(xs, medians, yerr=[ext_lo, ext_hi],
                    fmt="none", color=color, capsize=4, linewidth=1.0,
                    linestyle=":", zorder=4)

    ax.set_yscale("log")
    ax.set_xticks(group_pos)
    ax.set_xticklabels([f"{ps} pages" for ps in page_sizes], fontsize=11)
    ax.set_ylabel("Latency per page fault (µs, log scale)", fontsize=11)
    ax.set_title(
        "Page-fault latency\n"
        "bar = median  ·  thick error = p25–p75  ·  thin error = p5–p95",
        fontsize=12,
    )
    ax.legend(title="Backing", loc="best", fontsize=9)
    ax.yaxis.set_major_formatter(ticker.ScalarFormatter())
    ax.grid(True, axis="y", which="both", linestyle="--", alpha=0.4)


# ── Entry point ───────────────────────────────────────────────────────────────


def main() -> None:
    args = parse_args()
    root       = Path(args.criterion_dir)
    output_dir = Path(args.output)

    if not root.exists():
        sys.exit(f"error: criterion directory not found: {root}")

    output_dir.mkdir(parents=True, exist_ok=True)

    print(f"Loading from {root} …")
    tp_data = collect_throughput_data(root)
    pf_data = collect_page_fault_data(root)

    if not tp_data and not pf_data:
        sys.exit("error: no benchmark data found")

    # Apply page-size filter
    page_size = args.page_size
    if page_size is not None:
        tp_data = {k: v for k, v in tp_data.items() if k == page_size}
        pf_data = {b: {ps: s for ps, s in sizes.items() if ps == page_size}
                   for b, sizes in pf_data.items()}
        pf_data = {b: sizes for b, sizes in pf_data.items() if sizes}

    n_tp = sum(len(v) for v in tp_data.values())
    print(
        f"  throughput : {n_tp} series across "
        f"{len(tp_data)} group(s): {', '.join(sorted(tp_data)) or '—'}"
    )
    print(
        f"  page fault : {len(pf_data)} backing(s): "
        f"{', '.join(sorted(pf_data)) or '—'}"
    )

    # ── throughput.png: one subplot per page size (4k on top, 2m below) ──────
    groups = [g for g in ("4k", "2m") if g in tp_data]
    n_rows = len(groups) or 1
    fig, axes = plt.subplots(n_rows, 1, figsize=(12, 6 * n_rows))
    fig.subplots_adjust(hspace=0.55)
    if n_rows == 1:
        axes = [axes]
    for ax, group in zip(axes, groups):
        plot_throughput_ax(ax, tp_data[group], group)
    out = output_dir / "throughput.png"
    fig.savefig(out, dpi=150)
    plt.close(fig)
    print(f"  Wrote {out}")

    # ── page_fault.png ────────────────────────────────────────────────────────
    fig, ax = plt.subplots(figsize=(10, 6))
    plot_page_fault_ax(ax, pf_data)
    fig.tight_layout()
    out = output_dir / "page_fault.png"
    fig.savefig(out, dpi=150)
    plt.close(fig)
    print(f"  Wrote {out}")

    print("Done.")


if __name__ == "__main__":
    main()
