#!/usr/bin/env python3
"""Zero / constant / duplicate pages per raw dump, by chunk granularity.
Reads only the data extents (holes are zero pages); sessions in parallel."""
import sys, os, re, glob, hashlib, collections
from multiprocessing import Pool
import numpy as np
PAGE = 4096; GRANS = [4 << 10, 64 << 10, 1 << 20]
SEEK_DATA, SEEK_HOLE = 3, 4

def extents(fd, size):
    off = 0
    while off < size:
        try: d = os.lseek(fd, off, SEEK_DATA)
        except OSError: return
        h = os.lseek(fd, d, SEEK_HOLE); yield d & ~(PAGE-1), (h + PAGE - 1) & ~(PAGE-1); off = h

def scan(cdir):
    files = sorted(glob.glob(os.path.join(cdir, "memory-*")))
    flags = []; seen = set(); dup = 0
    for path in files:
        size = os.path.getsize(path); fl = np.ones(size // PAGE, np.uint8)  # 1 = zero (holes)
        fd = os.open(path, os.O_RDONLY)
        for d, h in extents(fd, size):
            pos = d
            while pos < h:
                n = min(64 << 20, h - pos)
                pages = np.frombuffer(os.pread(fd, n, pos), dtype=np.uint8).reshape(-1, PAGE)
                z = ~pages.any(axis=1); c = (pages == pages[:, :1]).all(axis=1) & ~z
                f = np.zeros(len(pages), np.uint8); f[z] = 1; f[c] = 2
                fl[pos // PAGE: pos // PAGE + len(pages)] = f
                for i in np.nonzero(f == 0)[0]:
                    hsh = hashlib.blake2b(pages[i].tobytes(), digest_size=8).digest()
                    if hsh in seen: dup += 1
                    else: seen.add(hsh)
                pos += n
        os.close(fd); flags.append(fl)
    fl = np.concatenate(flags); tot = len(fl); zero = int((fl == 1).sum()); const = int((fl == 2).sum())
    z = fl == 1; iaa = {}
    for g in GRANS:
        per = g // PAGE; k = tot // per
        iaa[g] = tot * PAGE - int(z[:k*per].reshape(k, per).all(axis=1).sum()) * g
    return cdir, tot, zero, const, dup, iaa

def key(n):
    m = re.match(r"step(\d+)", n); return int(m.group(1)) if m else -1

if __name__ == "__main__":
    dirs = []
    for root in sys.argv[1:]:
        for sdir in sorted(glob.glob(os.path.join(root, "*-4k")) + glob.glob(os.path.join(root, "*-2m"))):
            dirs += sorted(glob.glob(os.path.join(sdir, "*", "*")), key=lambda d: key(os.path.basename(d)))
    with Pool(16) as p: results = p.map(scan, dirs)
    print(f"{'session':28s} {'ckpt':7s} {'zero%':>6s} {'const%':>6s} {'dup%':>6s} {'data MiB':>8s} | IAA MiB @4K   @64K   @1M   4K-vs-1M")
    summary = collections.defaultdict(list)
    for cdir, tot, zero, const, dup, iaa in results:
        session = os.path.basename(os.path.dirname(os.path.dirname(cdir)))
        print(f"{session:28s} {os.path.basename(cdir):7s} {100*zero/tot:6.1f} {100*const/tot:6.1f} {100*dup/tot:6.1f} {(tot-zero)*PAGE>>20:8d} | {iaa[4096]>>20:8d} {iaa[65536]>>20:6d} {iaa[1<<20]>>20:6d} {100*(iaa[4096]-iaa[1<<20])/iaa[1<<20]:+7.1f}%")
        summary[session].append((zero/tot, const/tot, dup/tot, iaa[4096], iaa[1 << 20], (tot-zero)*PAGE))
    print(); print(f"{'session':28s} {'ckpts':>5s} {'zero%':>6s} {'const%':>6s} {'dup%':>6s} {'data MiB':>8s} {'IAA MiB @4K':>11s} {'@1M':>6s} {'4K-vs-1M':>9s}")
    for s, rows in summary.items():
        a = np.array(rows)
        print(f"{s:28s} {len(rows):5d} {100*a[:,0].mean():6.1f} {100*a[:,1].mean():6.1f} {100*a[:,2].mean():6.1f} {int(a[:,5].mean())>>20:8d} {int(a[:,3].mean())>>20:11d} {int(a[:,4].mean())>>20:6d} {100*(a[:,3].sum()-a[:,4].sum())/a[:,4].sum():+8.1f}%")
