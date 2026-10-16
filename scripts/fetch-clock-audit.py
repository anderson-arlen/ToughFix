#!/usr/bin/env python3
"""Cache daily observed IGS Rapid products for the offline clock audit."""
import argparse
from concurrent.futures import ThreadPoolExecutor
from datetime import date, timedelta, datetime, timezone
import gzip
import hashlib
import json
from pathlib import Path
import shutil
import urllib.request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--start', type=date.fromisoformat, required=True)
    parser.add_argument('--end', type=date.fromisoformat, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--cache', type=Path, action='append', default=[])
    args = parser.parse_args()
    if not 0 <= (args.end - args.start).days <= 366:
        parser.error('Use an ordered date range of at most one year')
    args.output.mkdir(parents=True, exist_ok=True)

    def fetch(day):
        name = f'IGS0OPSRAP_{day.year}{day.timetuple().tm_yday:03}0000_01D_15M_ORB.SP3.gz'
        path = args.output / name
        url = f'https://noaa-cors-pds.s3.amazonaws.com/rinex/{day.year}/{day.timetuple().tm_yday:03}/{name}'
        origin = 'existing audit cache'
        if not path.exists():
            cached = next((p / name for p in args.cache if (p / name).is_file()), None)
            if cached:
                shutil.copyfile(cached, path)
                origin = 'previous local download'
            else:
                request = urllib.request.Request(url, headers={'User-Agent': 'ToughFix-clock-audit/0.1'})
                # One request per missing product. HTTP refusals stop this audit.
                with urllib.request.urlopen(request, timeout=45) as response:
                    body = response.read(4 * 1024 * 1024 + 1)
                    if response.status != 200 or not 0 < len(body) <= 4 * 1024 * 1024:
                        raise ValueError(f'Invalid source response: {url}')
                path.write_bytes(body)
                origin = 'downloaded for this audit'
        raw = gzip.decompress(path.read_bytes()).decode('ascii')
        lines = raw.splitlines()
        if not lines[0].startswith('#') or not any(line.startswith('%c') and 'GPS' in line.split() for line in lines[:22]):
            raise ValueError(f'Product does not declare GPS time: {name}')
        epochs = []
        for line in lines:
            if line.startswith('* '):
                fields = line.split()
                epochs.append(datetime(*map(int, fields[1:6])) + timedelta(seconds=float(fields[6])))
        start = datetime.combine(day, datetime.min.time())
        if epochs != [start + timedelta(minutes=15*i) for i in range(96)]:
            raise ValueError(f'Product does not contain its declared complete observation day: {name}')
        return dict(file=name, url=url, sha256=hashlib.sha256(path.read_bytes()).hexdigest(),
                    bytes=path.stat().st_size, cache_origin=origin)

    days = [args.start + timedelta(days=i) for i in range((args.end - args.start).days + 1)]
    with ThreadPoolExecutor(max_workers=3) as pool:
        entries = list(pool.map(fetch, days))
    manifest = dict(retrieved_at_utc=datetime.now(timezone.utc).isoformat(),
                    start=str(args.start), end=str(args.end), products=entries)
    (args.output / 'audit-inputs.json').write_text(json.dumps(manifest, indent=2) + '\n')
    print(json.dumps(dict(products=len(entries), downloaded=sum(e['cache_origin'] == 'downloaded for this audit' for e in entries),
                         total_bytes=sum(e['bytes'] for e in entries)), indent=2))


if __name__ == '__main__':
    main()
