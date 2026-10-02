#!/usr/bin/env python3
"""Realistic agent-sandbox tool steps for the full rootfs.
usage: agent_wl2.py <step> <seed>
steps: pyimport | pandas | web | git | pytest | rg | sqlite | all
Each step is a fresh process (like a tool call spawning python/git/rg)."""
import sys, os, time, subprocess, random, json
step, seed = sys.argv[1], int(sys.argv[2]) if len(sys.argv) > 2 else 1
T = "/tmp/work"; os.makedirs(T, exist_ok=True)
rng = random.Random(seed)

def sh(cmd, **kw):
    return subprocess.run(cmd, shell=True, capture_output=True, text=True, **kw)

def pyimport():
    r = sh("python3 -c 'import numpy, pandas, pyarrow, requests, httpx, jinja2, pydantic, sqlalchemy, yaml, bs4, lxml, markdown, rich, anthropic, openai, tiktoken; print(\"imported\")'")
    return r.stdout.strip()

def pandas_step():
    import numpy as np, pandas as pd
    n = 1_000_000
    df = pd.DataFrame({"k": np.random.default_rng(seed).integers(0, 5000, n),
                       "v": np.random.default_rng(seed + 1).random(n),
                       "s": np.random.default_rng(seed + 2).choice(["alpha", "beta", "gamma", "delta"], n)})
    g = df.groupby(["k", "s"]).agg(v_mean=("v", "mean"), v_max=("v", "max"), n=("v", "size")).reset_index()
    m = df.merge(g, on=["k", "s"])
    p = os.path.join(T, f"frame_{seed % 3}.parquet"); m.to_parquet(p)
    back = pd.read_parquet(p)
    csv = os.path.join(T, "sample.csv"); back.sample(100_000, random_state=seed).to_csv(csv, index=False)
    return f"rows={len(back)} groups={len(g)} csv={os.path.getsize(csv)}"

def web_step():
    from jinja2 import Template
    from bs4 import BeautifulSoup
    import markdown, pydantic, yaml
    class Doc(pydantic.BaseModel):
        title: str; tags: list[str]; body: str; score: float
    tpl = Template("<html><body><h1>{{d.title}}</h1>{% for t in d.tags %}<span class=t>{{t}}</span>{% endfor %}<div>{{body}}</div></body></html>")
    total = 0; docs = []
    for i in range(2000):
        d = Doc(title=f"doc {i} seed {seed}", tags=[f"t{rng.randint(0, 50)}" for _ in range(5)],
                body="\n\n".join("## sec %d\n\n%s" % (j, " ".join(rng.choices(["lorem", "ipsum", "agent", "tool", "call", "result"], k=60))) for j in range(8)),
                score=rng.random())
        html = tpl.render(d=d, body=markdown.markdown(d.body))
        soup = BeautifulSoup(html, "lxml")
        total += len(soup.find_all("span")) + len(soup.get_text())
        docs.append(d.model_dump())
    with open(os.path.join(T, "docs.yaml"), "w") as f:
        yaml.safe_dump(docs[:300], f)
    with open(os.path.join(T, "docs.json"), "w") as f:
        json.dump(docs, f)
    return f"total={total}"

def git_step():
    repo = os.path.join(T, "repo")
    if not os.path.isdir(repo):
        os.makedirs(repo); sh("git init -q", cwd=repo)
        for i in range(400):
            os.makedirs(os.path.join(repo, f"pkg{i % 20}"), exist_ok=True)
            with open(os.path.join(repo, f"pkg{i % 20}", f"mod{i}.py"), "w") as f:
                f.write("\n".join(f"def f{i}_{j}(x):\n    return x * {j} + {i}\n" for j in range(40)))
        sh("git add -A && git commit -qm init", cwd=repo)
    for i in rng.sample(range(400), 40):
        p = os.path.join(repo, f"pkg{i % 20}", f"mod{i}.py")
        with open(p, "a") as f:
            f.write(f"\n# edit seed {seed}\ndef g{seed}(y):\n    return y + {rng.randint(0, 999)}\n")
    a = sh("git status --short | wc -l && git diff --stat | tail -1 && git add -A && git commit -qm step%d && git log --oneline | wc -l" % seed, cwd=repo)
    return a.stdout.replace("\n", " ")

def pytest_step():
    td = os.path.join(T, "tests"); os.makedirs(td, exist_ok=True)
    for i in range(30):
        with open(os.path.join(td, f"test_m{i}.py"), "w") as f:
            f.write("import json, math\n" + "\n".join(
                f"def test_{i}_{j}():\n    assert json.loads(json.dumps({{'a': {j}}}))['a'] == {j}\n    assert math.isclose(math.sqrt({j}*{j}), {j})\n" for j in range(25)))
    r = sh(f"python3 -m pytest -q -p no:cacheprovider {td} 2>&1 | tail -1")
    return r.stdout.strip()

def rg_step():
    r = sh("rg -c 'def ' /usr/lib/python3/dist-packages /usr/local/lib/python3.12/dist-packages 2>/dev/null | wc -l; "
           "rg -l 'import numpy' /usr/local/lib/python3.12/dist-packages 2>/dev/null | wc -l; "
           "find /usr -name '*.py' | wc -l; jq -c '.[0:3]' /tmp/work/docs.json 2>/dev/null | wc -c")
    return r.stdout.replace("\n", " ")

def sqlite_step():
    import sqlalchemy as sa
    eng = sa.create_engine(f"sqlite:///{T}/db.sqlite")
    md = sa.MetaData(); t = sa.Table("ev", md, sa.Column("id", sa.Integer, primary_key=True),
                                     sa.Column("k", sa.Integer), sa.Column("v", sa.String))
    md.create_all(eng)
    with eng.begin() as c:
        c.execute(t.insert(), [{"k": rng.randint(0, 1000), "v": "x" * rng.randint(10, 200)} for _ in range(50000)])
        n = c.execute(sa.select(sa.func.count()).select_from(t)).scalar()
        top = c.execute(sa.select(t.c.k, sa.func.count()).group_by(t.c.k).order_by(sa.func.count().desc()).limit(5)).all()
    return f"rows={n} top={top[0]}"

def diag():
    import traceback, io
    b = io.StringIO()
    try:
        import numpy
    except Exception:
        traceback.print_exc(file=b)
    d = "/usr/local/lib/python3.12/dist-packages"
    return {"path": sys.path, "exists": os.path.isdir(d), "ls": sorted(os.listdir(d))[:6] if os.path.isdir(d) else None,
            "mounts": open("/proc/mounts").read().split("\n")[:8], "exe": sys.executable, "tb": b.getvalue()[-600:]}

def _vm():
    d = {}
    for f in ("/proc/vmstat", "/proc/meminfo"):
        for line in open(f):
            k, v = line.split()[:2]; d[k.rstrip(":")] = int(v)
    return d

def rgdiag():
    keys = ["pgpgin", "pgfault", "pgmajfault", "pgsteal_kswapd", "pgsteal_direct", "pgscan_kswapd", "pgscan_direct",
            "nr_file_pages", "nr_anon_pages", "nr_free_pages", "workingset_refault_file", "drop_pagecache",
            "MemFree", "Cached", "AnonPages", "Buffers"]
    a = _vm(); r = rg_step(); b = _vm()
    return {"rg": r.strip(), "delta": {k: b.get(k, 0) - a.get(k, 0) for k in keys}, "after": {k: b.get(k, 0) for k in ("MemFree", "Cached", "nr_file_pages")}}

def rgflags():
    """run rg, then emit an RLE class map of every guest PFN from /proc/kpageflags:
    F=buddy free, A=anon, C=file cache (LRU & !anon), S=slab, N=nopage, O=other"""
    import array
    r = rg_step()
    a = array.array("Q"); fd = os.open("/proc/kpageflags", os.O_RDONLY); CH = 4096
    for pfn in range(0, 5 << 18, CH):          # up to 5GiB of GPA
        try:
            b = os.pread(fd, 8 * CH, 8 * pfn)
        except OSError:
            b = b""
        if len(b) < 8 * CH:
            b += (1 << 20).to_bytes(8, "little") * (CH - len(b) // 8)   # pad as NOPAGE
        a.frombytes(b)
    os.close(fd)
    LRU, BUDDY, ANON, SLAB, NOPAGE = 1 << 5, 1 << 10, 1 << 12, 1 << 19, 1 << 20
    out = []; cur = None; n = 0
    for v in a:
        c = "N" if v & NOPAGE else "F" if v & BUDDY else "A" if v & ANON else "S" if v & SLAB else "C" if v & LRU else "O"
        if c == cur: n += 1
        else:
            if cur: out.append(f"{cur}{n}")
            cur, n = c, 1
    out.append(f"{cur}{n}")
    sys.stdout.write("AGENT_KPF " + ",".join(out) + "\n"); sys.stdout.flush()
    return r.strip()

D2 = "/usr/local/lib/python3.12/dist-packages"
def rgnommap(): return sh(f"rg --no-mmap -c 'def ' {D2} | wc -l").stdout.strip()
def rgmmap():   return sh(f"rg --mmap -c 'def ' {D2} | wc -l").stdout.strip()
def catread():  return sh(f"find {D2} -type f -print0 | xargs -0 cat | wc -c").stdout.strip()
def pyread():
    n = 0
    for root, _, files in os.walk(D2):
        for f in files:
            try:
                with open(os.path.join(root, f), "rb") as fh: n += len(fh.read())
            except OSError: pass
    return str(n)

STEPS = {"diag": diag, "rgnommap": rgnommap, "rgmmap": rgmmap, "catread": catread, "pyread": pyread, "rgdiag": rgdiag, "rgflags": rgflags, "pyimport": pyimport, "pandas": pandas_step, "web": web_step, "git": git_step,
         "pytest": pytest_step, "rg": rg_step, "sqlite": sqlite_step}
t0 = time.time()
if step == "all":
    out = {s: STEPS[s]() for s in STEPS if s not in ("diag", "rgdiag", "rgflags", "rgnommap", "rgmmap", "catread", "pyread")}
else:
    out = STEPS[step]()
print(f"AGENT_WL2 {step} seed={seed} secs={time.time()-t0:.2f} {out}")
