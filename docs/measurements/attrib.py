import glob,re,statistics as st,collections,sys,csv
d=sys.argv[1]
def p95(v):
    v=sorted(v); return v[min(len(v)-1,int(round(0.95*(len(v)-1))))]
wall=collections.defaultdict(list)
for r in csv.DictReader(open(d+"/results.csv")):
    wall[(r["phase"],r["codec"],int(r["workers"]))].append(float(r["elapsed_ms"]))
cpu=collections.defaultdict(list)
for f in glob.glob(d+"/logs/*-cpu.txt"):
    m=re.search(r"/(snapshot|restore)-(.*)-c\d+-w(\d+)-i\d+-cpu\.txt",f)
    if not m: continue
    t=open(f).read().split()
    if len(t)>=3: cpu[(m.group(1),m.group(2),int(m.group(3)))].append(float(t[1])+float(t[2]))
print(f"{'phase':8s} {'codec':38s} {'w':>3s} {'n':>3s} {'med ms':>8s} {'p95 ms':>8s} {'cpu s med':>9s} {'cpu s p95':>9s}")
for k in sorted(wall):
    c=cpu.get(k,[])
    print(f"{k[0]:8s} {k[1]:38s} {k[2]:3d} {len(wall[k]):3d} {st.median(wall[k]):8.1f} {p95(wall[k]):8.1f} {st.median(c) if c else float('nan'):9.3f} {p95(c) if c else float('nan'):9.3f}")
