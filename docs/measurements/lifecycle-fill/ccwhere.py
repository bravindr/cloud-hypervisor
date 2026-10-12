import os, sys, numpy as np
PAGE=4096; path=sys.argv[1]; size=os.path.getsize(path); fd=os.open(path, os.O_RDONLY)
off=0; runs=[]; cur=None
while off < size:
    try: d=os.lseek(fd,off,3)
    except OSError: break
    h=os.lseek(fd,d,4); d&=~(PAGE-1); h=(h+PAGE-1)&~(PAGE-1); pos=d
    while pos<h:
        n=min(64<<20,h-pos); pg=np.frombuffer(os.pread(fd,n,pos),np.uint8).reshape(-1,PAGE)
        cc=(pg==0xCC).all(axis=1)
        for i,v in enumerate(cc):
            a=pos+i*PAGE
            if v:
                if cur and cur[1]==a: cur[1]=a+PAGE
                else:
                    if cur: runs.append(cur)
                    cur=[a,a+PAGE]
        pos+=n
    off=h
if cur: runs.append(cur)
runs.sort(key=lambda r: r[0]-r[1])
for a,b in runs[:6]: print(f"  0xCC run gpa 0x{a:x}-0x{b:x} = {(b-a)>>20} MiB {(b-a)>>10} KiB")
print(f"  {len(runs)} runs total")
