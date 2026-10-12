#!/usr/bin/env python3
"""Per dump: bytes in data extents (what the daemon must classify) and how
many of those are zero pages (allocated zeros, not holes) vs holes."""
import sys, os, glob, re, collections
import numpy as np
PAGE = 4096
SEEK_DATA, SEEK_HOLE = 3, 4
def extents(fd, size):
    off = 0
    while off < size:
        try: d = os.lseek(fd, off, SEEK_DATA)
        except OSError: return
        h = os.lseek(fd, d, SEEK_HOLE)
        yield d, h; off = h
def key(n):
    m = re.match(r"step(\d+)", n); return int(m.group(1)) if m else -1
print(f"{'session':28s} {'ckpt':7s} {'guest MiB':>9s} {'hole MiB':>8s} {'data MiB':>8s} {'alloc-zero MiB':>14s} {'alloc-zero %data':>16s}")
agg = collections.defaultdict(lambda: np.zeros(3))
for root in sys.argv[1:]:
    for sdir in sorted(glob.glob(os.path.join(root, "*-4k"))):
        session = os.path.basename(sdir)
        for cdir in sorted(glob.glob(os.path.join(sdir, "*", "*")), key=lambda d: key(os.path.basename(d))):
            tot = data = az = 0
            for path in sorted(glob.glob(os.path.join(cdir, "memory-*"))):
                size = os.path.getsize(path); tot += size
                fd = os.open(path, os.O_RDONLY)
                for d, h in extents(fd, size):
                    d &= ~(PAGE - 1); h = (h + PAGE - 1) & ~(PAGE - 1)
                    data += h - d; pos = d
                    while pos < h:
                        n = min(64 << 20, h - pos)
                        buf = np.frombuffer(os.pread(fd, n, pos), dtype=np.uint8).reshape(-1, PAGE)
                        az += int((~buf.any(axis=1)).sum()) * PAGE; pos += n
                os.close(fd)
            print(f"{session:28s} {os.path.basename(cdir):7s} {tot>>20:9d} {(tot-data)>>20:8d} {data>>20:8d} {az>>20:14d} {100*az/max(data,1):15.1f}%", flush=True)
            agg[session] += (tot, data, az)
print()
for s, (tot, data, az) in agg.items():
    print(f"{s:28s} mean per ckpt: data {int(data/ (tot/ (4096<<20)))>>20 if False else ''}hole {100*(tot-data)/tot:5.1f}%  data {100*data/tot:5.1f}%  alloc-zero {100*az/tot:5.1f}% of guest = {100*az/max(data,1):5.1f}% of data")
