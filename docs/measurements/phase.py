import glob,re,statistics,sys,collections
d=sys.argv[1]
agg=collections.defaultdict(list)
for f in glob.glob(d+"/logs/snapshot-*-daemon.log"):
    m=re.search(r"snapshot-(.*)-c\d+-w(\d+)-i\d+-daemon",f)
    if not m: continue
    t=open(f).read()
    c=[float(x) for x in re.findall(r"compress_ms=([\d.]+)",t)]
    s=[float(x) for x in re.findall(r"sync_ms=([\d.]+)",t)]
    if c: agg[(m.group(1),int(m.group(2)))].append((max(c),max(s)))
for k in sorted(agg):
    v=agg[k]; print(f"{k[0]:42s} w{k[1]:<3d} n={len(v):2d} compress_ms med {statistics.median(x[0] for x in v):7.1f}  sync_ms med {statistics.median(x[1] for x in v):6.1f}")
