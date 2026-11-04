import json,sys,os
B=os.environ.get('BENCH_DIR', os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'state', 'bench'))
def load(n):
    p=os.path.join(B,n+'.jsonl')
    return [json.loads(l) for l in open(p)] if os.path.exists(p) else []
def fit(xs,ys):
    n=len(xs); mx=sum(xs)/n; my=sum(ys)/n
    sxx=sum((x-mx)**2 for x in xs)
    b=sum((x-mx)*(y-my) for x,y in zip(xs,ys))/sxx if sxx else 0
    return my-b*mx,b
out={}
H=load('hops')
if H:
    print("hop | cons raw/gz | proof json/gz | bin/gz | incr bin | psbts | cold ms | warm ms | cons-only ms | cold bin ms | accept ms")
    for r in H:
        p=r['proof'];c=r['consignment'];v=r['validation']
        print(f"{r['hop']} | {c['raw']}/{c['gz']} | {p['json']}/{p['json_gz']} | {p['bin']}/{p['bin_gz']} | {r['proof_incremental_bin']} | {p['n_psbts']} | {v['cold_ms']:.0f} | {v['warm_ms']:.0f} | {v['cons_only_ms']:.0f} | {v['cold_bin_ms']:.0f} | {r['accept_ms']:.0f}")
    xs=[r['hop'] for r in H]
    for k,f in [('cons_raw',lambda r:r['consignment']['raw']),('cons_gz',lambda r:r['consignment']['gz']),('proof_json',lambda r:r['proof']['json']),('proof_bin',lambda r:r['proof']['bin']),('proof_bin_gz',lambda r:r['proof']['bin_gz']),('cold_ms',lambda r:r['validation']['cold_ms']),('accept_ms',lambda r:r['accept_ms'])]:
        a,b=fit(xs,[f(r) for r in H]); out['hops_'+k]=(round(a,1),round(b,1)); print(f"fit hops {k}: {a:.0f} + {b:.1f}*h")
for name,key in [('refresh','refreshes'),('onchain','hop')]:
    R=load(name)
    if R:
        print(f"\n{name}: n | cons raw/gz | proof json/bin | cold ms | warm ms")
        for r in R:
            p=r.get('proof',{});c=r['consignment'];v=r['validation']
            print(f"{r[key]} | {c['raw']}/{c['gz']} | {p.get('json',0)}/{p.get('bin',0)} | {v['cold_ms']:.0f} | {v['warm_ms']:.0f}")
        xs=[r[key] for r in R]
        for k,f in [('cons_raw',lambda r:r['consignment']['raw']),('cons_gz',lambda r:r['consignment']['gz']),('proof_json',lambda r:r.get('proof',{}).get('json',0)),('proof_bin',lambda r:r.get('proof',{}).get('bin',0)),('cold_ms',lambda r:r['validation']['cold_ms'])]:
            a,b=fit(xs,[f(r) for r in R]); out[name+'_'+k]=(round(a,1),round(b,1)); print(f"fit {name} {k}: {a:.0f} + {b:.1f}*n")
M=load('merge')
if M:
    print("\nmerge: k | cons raw/gz | cold ms | warm ms")
    for r in sorted(M,key=lambda r:r['k']):
        c=r['consignment'];v=r['validation']; print(f"{r['k']} | {c['raw']}/{c['gz']} | {v['cold_ms']:.0f} | {v['warm_ms']:.0f}")
    xs=[r['k'] for r in M]
    for k,f in [('cons_raw',lambda r:r['consignment']['raw']),('cold_ms',lambda r:r['validation']['cold_ms'])]:
        a,b=fit(xs,[f(r) for r in M]); out['merge_'+k]=(round(a,1),round(b,1)); print(f"fit merge {k}: {a:.0f} + {b:.1f}*k")
json.dump(out,open(os.path.join(B,'fits.json'),'w'),indent=1)
