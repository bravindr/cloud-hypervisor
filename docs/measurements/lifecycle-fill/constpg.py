import sys, os, glob, collections, numpy as np
PAGE=4096; SEEK_DATA, SEEK_HOLE = 3, 4
for cdir in sys.argv[1:]:
    vals = collections.Counter(); chunk_const = collections.Counter(); const_in_full = 0; const_total = 0
    for path in sorted(glob.glob(os.path.join(cdir, "memory-*"))):
        size = os.path.getsize(path); fd = os.open(path, os.O_RDONLY)
        kind = np.full(size // PAGE, -1, np.int16)  # -1 zero/hole, 0..255 constant value, 256 data
        off = 0
        while off < size:
            try: d = os.lseek(fd, off, SEEK_DATA)
            except OSError: break
            h = os.lseek(fd, d, SEEK_HOLE); d &= ~(PAGE-1); h = (h+PAGE-1) & ~(PAGE-1); pos = d
            while pos < h:
                n = min(64<<20, h-pos); pg = np.frombuffer(os.pread(fd, n, pos), np.uint8).reshape(-1, PAGE)
                z = ~pg.any(axis=1); c = (pg == pg[:, :1]).all(axis=1) & ~z
                k = np.where(z, -1, np.where(c, pg[:, 0].astype(np.int16), 256))
                kind[pos//PAGE: pos//PAGE+len(pg)] = k; pos += n
                for v in pg[c, 0]: vals[int(v)] += 1
            off = h
        os.close(fd)
        per = 256; m = len(kind)//per; ch = kind[:m*per].reshape(m, per)
        isconst = (ch >= 0) & (ch < 256)
        const_total += int(isconst.sum())
        full = isconst.all(axis=1) & (ch == ch[:, :1]).all(axis=1)
        const_in_full += int(isconst[full].sum())
        # constant pages per non-full chunk distribution
        mixed = isconst[~full].sum(axis=1); mixed = mixed[mixed > 0]
        chunk_const.update((mixed // 32 * 32).tolist())
    print(os.path.basename(os.path.dirname(os.path.dirname(cdir))), os.path.basename(cdir))
    print("  constant page byte values:", dict(vals.most_common(6)))
    print(f"  constant pages {const_total} ({const_total*PAGE>>20} MiB); in fully-constant 1 MiB chunks: {const_in_full} ({100*const_in_full/max(const_total,1):.0f}%)")
    print("  constant pages per mixed chunk (binned by 32):", dict(sorted(chunk_const.items())))
