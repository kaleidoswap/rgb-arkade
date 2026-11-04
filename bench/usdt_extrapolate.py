# ESTIMATES from the measured linear fits (<bench dir>/fits.json, written by analyze.py). Not measured.
import json, os
B=os.environ.get('BENCH_DIR', os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'state', 'bench'))
f=json.load(open(os.path.join(B,'fits.json')))
C0=5054            # consignment base (genesis + first transition), B
ON=f['onchain_cons_raw'][1]; OFF=f['hops_cons_raw'][1]; MERGE=f['merge_cons_raw'][1]
PJ=f['hops_proof_json'][1]; PB=f['hops_proof_bin'][1]; PBGZ=f['hops_proof_bin_gz'][1]
T0=120; TON=f['onchain_cold_ms'][1]; TOFF=f['hops_cold_ms'][1]
print("ancestry transitions | of which unmined offchain | consignment | proof json | proof bin | cold validation")
for A,U in [(1,1),(10,4),(10,10),(100,10),(1000,10),(10000,10),(100000,10)]:
    cons=C0+ON*(A-U)+OFF*U
    print(f"{A} | {U} | {cons/1024:.0f} kB | {PJ*U/1024:.1f} kB | {PB*U/1024:.1f} kB | {(T0+TON*(A-U)+TOFF*U)/1000:.2f} s")
print(f"per-transition: mined {ON:.0f} B / {TON:.1f} ms, offchain {OFF:.0f} B cons + {PB:.0f} B bin proof / {TOFF:.1f} ms, merge input {MERGE:.0f} B")
