#!/usr/bin/env python3
"""Generate a self-hosted SVG chart from a repository's GitHub stargazers."""

from __future__ import annotations

import argparse
import html
import json
import math
import os
import subprocess
import sys
import urllib.error
import urllib.request
from datetime import datetime, timedelta, timezone
from pathlib import Path


API_VERSION = "2022-11-28"
USER_AGENT = "tensaku-star-history"


def github_token() -> str:
    for name in ("GITHUB_TOKEN", "GH_TOKEN"):
        if token := os.environ.get(name):
            return token

    try:
        result = subprocess.run(
            ["gh", "auth", "token"],
            check=True,
            capture_output=True,
            text=True,
        )
    except (FileNotFoundError, subprocess.CalledProcessError) as error:
        raise RuntimeError(
            "Set GITHUB_TOKEN (or GH_TOKEN), or authenticate the GitHub CLI."
        ) from error

    return result.stdout.strip()


def github_get(path: str, token: str) -> tuple[object, dict[str, str]]:
    request = urllib.request.Request(
        f"https://api.github.com{path}",
        headers={
            "Accept": "application/vnd.github.star+json",
            "Authorization": f"Bearer {token}",
            "User-Agent": USER_AGENT,
            "X-GitHub-Api-Version": API_VERSION,
        },
    )

    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            headers = {key.lower(): value for key, value in response.headers.items()}
            return json.load(response), headers
    except urllib.error.HTTPError as error:
        detail = error.read().decode("utf-8", errors="replace")[:500]
        raise RuntimeError(f"GitHub API request failed ({error.code}): {detail}") from error


def load_repository(repo: str, token: str) -> tuple[datetime, list[datetime]]:
    metadata, _ = github_get(f"/repos/{repo}", token)
    if not isinstance(metadata, dict) or "created_at" not in metadata:
        raise RuntimeError("GitHub returned unexpected repository metadata.")

    created_at = parse_timestamp(str(metadata["created_at"]))
    starred_at: list[datetime] = []

    for page in range(1, 101):
        payload, _ = github_get(
            f"/repos/{repo}/stargazers?per_page=100&page={page}", token
        )
        if not isinstance(payload, list):
            raise RuntimeError("GitHub returned unexpected stargazer data.")

        for item in payload:
            if not isinstance(item, dict) or "starred_at" not in item:
                raise RuntimeError(
                    "Stargazer timestamps were unavailable. The token needs "
                    "write-level repository contents permission."
                )
            starred_at.append(parse_timestamp(str(item["starred_at"])))

        if len(payload) < 100:
            break
    else:
        raise RuntimeError("Refusing to fetch more than 10,000 stargazers.")

    return created_at, sorted(starred_at)


def parse_timestamp(value: str) -> datetime:
    return datetime.fromisoformat(value.replace("Z", "+00:00")).astimezone(timezone.utc)


def nice_y_axis(maximum: int) -> tuple[int, int]:
    if maximum <= 5:
        return 5, 1

    rough_step = maximum / 5
    magnitude = 10 ** math.floor(math.log10(rough_step))
    fraction = rough_step / magnitude
    if fraction <= 1:
        nice_fraction = 1
    elif fraction <= 2:
        nice_fraction = 2
    elif fraction <= 5:
        nice_fraction = 5
    else:
        nice_fraction = 10

    step = int(nice_fraction * magnitude)
    return math.ceil(maximum / step) * step, step


def format_date(value: datetime, include_year: bool) -> str:
    pattern = "%b %Y" if include_year else "%b %-d"
    return value.strftime(pattern)


def build_svg(repo: str, created_at: datetime, starred_at: list[datetime]) -> str:
    width, height = 1000, 560
    left, right, top, bottom = 82, 36, 92, 72
    plot_width = width - left - right
    plot_height = height - top - bottom

    end_at = starred_at[-1] if starred_at else datetime.now(timezone.utc)
    if end_at <= created_at:
        end_at = created_at + timedelta(days=1)
    span = (end_at - created_at).total_seconds()

    y_max, y_step = nice_y_axis(len(starred_at))

    def x_position(value: datetime) -> float:
        return left + ((value - created_at).total_seconds() / span) * plot_width

    def y_position(value: int) -> float:
        return top + plot_height - (value / y_max) * plot_height

    grid: list[str] = []
    for value in range(0, y_max + 1, y_step):
        y = y_position(value)
        grid.append(
            f'<line x1="{left}" y1="{y:.2f}" x2="{left + plot_width}" '
            f'y2="{y:.2f}" class="grid" />'
        )
        grid.append(
            f'<text x="{left - 15}" y="{y + 5:.2f}" class="axis-label" '
            f'text-anchor="end">{value}</text>'
        )

    include_year = (end_at - created_at).days >= 330
    x_labels: list[str] = []
    for index in range(5):
        ratio = index / 4
        value = created_at + (end_at - created_at) * ratio
        x = left + plot_width * ratio
        x_labels.append(
            f'<line x1="{x:.2f}" y1="{top}" x2="{x:.2f}" '
            f'y2="{top + plot_height}" class="grid vertical" />'
        )
        x_labels.append(
            f'<text x="{x:.2f}" y="{top + plot_height + 34}" class="axis-label" '
            f'text-anchor="middle">{html.escape(format_date(value, include_year))}</text>'
        )

    line_parts = [f"M {x_position(created_at):.2f} {y_position(0):.2f}"]
    previous_count = 0
    for count, timestamp in enumerate(starred_at, start=1):
        x = x_position(timestamp)
        line_parts.append(f"L {x:.2f} {y_position(previous_count):.2f}")
        line_parts.append(f"L {x:.2f} {y_position(count):.2f}")
        previous_count = count
    line_parts.append(f"L {x_position(end_at):.2f} {y_position(len(starred_at)):.2f}")
    line_path = " ".join(line_parts)
    area_path = (
        f"{line_path} L {x_position(end_at):.2f} {y_position(0):.2f} "
        f"L {x_position(created_at):.2f} {y_position(0):.2f} Z"
    )

    escaped_repo = html.escape(repo)
    if starred_at:
        status = f"{len(starred_at)} GitHub stars · Last star {starred_at[-1].strftime('%b %-d, %Y')}"
    else:
        status = "No GitHub stars yet"

    return f'''<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" role="img" aria-labelledby="title description">
  <title id="title">{escaped_repo} star history</title>
  <desc id="description">A chart showing {len(starred_at)} GitHub stars since {created_at.strftime('%B %-d, %Y')}.</desc>
  <style>
    .background {{ fill: #ffffff; }}
    .title {{ fill: #111827; font: 700 27px -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }}
    .subtitle {{ fill: #6b7280; font: 15px -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }}
    .axis-label {{ fill: #6b7280; font: 13px -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }}
    .axis-title {{ fill: #4b5563; font: 600 13px -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }}
    .grid {{ stroke: #e5e7eb; stroke-width: 1; }}
    .grid.vertical {{ stroke-dasharray: 3 6; }}
    .area {{ fill: url(#area-gradient); }}
    .line {{ fill: none; stroke: #16a34a; stroke-width: 3; stroke-linejoin: round; stroke-linecap: round; }}
    .endpoint {{ fill: #16a34a; stroke: #ffffff; stroke-width: 3; }}
    .border {{ fill: none; stroke: #d1d5db; stroke-width: 1; }}
  </style>
  <defs>
    <linearGradient id="area-gradient" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0%" stop-color="#22c55e" stop-opacity="0.28" />
      <stop offset="100%" stop-color="#22c55e" stop-opacity="0.03" />
    </linearGradient>
    <clipPath id="plot-clip">
      <rect x="{left}" y="{top}" width="{plot_width}" height="{plot_height}" />
    </clipPath>
  </defs>
  <rect class="background" width="{width}" height="{height}" rx="8" />
  <text x="{left}" y="42" class="title">{escaped_repo} Star History</text>
  <text x="{left}" y="68" class="subtitle">{html.escape(status)}</text>
  {''.join(grid)}
  {''.join(x_labels)}
  <g clip-path="url(#plot-clip)">
    <path d="{area_path}" class="area" />
    <path d="{line_path}" class="line" />
  </g>
  <circle cx="{x_position(end_at):.2f}" cy="{y_position(len(starred_at)):.2f}" r="6" class="endpoint" />
  <rect x="{left}" y="{top}" width="{plot_width}" height="{plot_height}" class="border" />
  <text x="24" y="{top + plot_height / 2:.2f}" class="axis-title" text-anchor="middle" transform="rotate(-90 24 {top + plot_height / 2:.2f})">GitHub stars</text>
</svg>
'''


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--repo",
        default=os.environ.get("GITHUB_REPOSITORY"),
        help="Repository in owner/name form (defaults to GITHUB_REPOSITORY)",
    )
    parser.add_argument("--output", required=True, type=Path, help="SVG output path")
    args = parser.parse_args()

    if not args.repo or "/" not in args.repo:
        parser.error("--repo must be provided in owner/name form")

    try:
        created_at, starred_at = load_repository(args.repo, github_token())
        svg = build_svg(args.repo, created_at, starred_at)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(svg, encoding="utf-8")
    except (OSError, RuntimeError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1

    print(f"Wrote {args.output} with {len(starred_at)} stars.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
