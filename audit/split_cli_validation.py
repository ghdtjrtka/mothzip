"""Measure volume set replacement, self-exclusion, and redirected-output refusal through the CLI."""
from pathlib import Path
import argparse,hashlib,json,os,subprocess,time,uuid

p=argparse.ArgumentParser();p.add_argument('--binary',required=True);args=p.parse_args()
workspace=Path(__file__).resolve().parents[1];binary=Path(args.binary).resolve()
root=workspace/'audit/runs'/('split-cli-'+time.strftime('%Y%m%d-%H%M%S')+'-'+uuid.uuid4().hex[:6]);root.mkdir(parents=True)
records=[]
def call(*args):
    r=subprocess.run([str(binary),*map(str,args),'--json','--non-interactive'],capture_output=True,timeout=30)
    return dict(exit=r.returncode,stdout=r.stdout.decode('utf8','replace'),stderr=r.stderr.decode('utf8','replace'))
def record(name,passed,calls):records.append(dict(case=name,passed=passed,calls=calls));print(json.dumps(dict(case=name,passed=passed)),flush=True)
def inventory(path):return {str(f.relative_to(path)).replace('\\','/'):hashlib.sha256(f.read_bytes()).hexdigest() for f in path.rglob('*') if f.is_file()}
def digest(path):return hashlib.sha256(path.read_bytes()).hexdigest()

source=root/'data';source.mkdir();(source/'nested').mkdir()
(source/'payload.bin').write_bytes(os.urandom(128*1024))
(source/'nested/backup.7z.001').write_bytes(b'DISTINCT SAME LEAF')
backup=source/'backup.7z.001.bak';backup.write_bytes(b'UNRELATED')
dest=source/'backup.7z';first=Path(str(dest)+'.001')
expected={'data/'+name:value for name,value in inventory(source).items()}
r=call('pack',dest,source,'--level',0,'--volume-bytes',4096)
summary=json.loads(r['stdout']) if r['exit']==0 else {}
out=root/'first-out';x=call('unpack',first,'-o',out)
sizes=summary.get('volume_sizes',[])
record('split-and-read',r['exit']==0 and x['exit']==0 and inventory(out)==expected and sizes and all(n==4096 for n in sizes[:-1]) and 0<sizes[-1]<=4096,[r,x])

(source/'payload.bin').write_bytes(b'SMALL')
expected={name:value for name,value in expected.items() if name!='data/payload.bin'};expected['data/payload.bin']=digest(source/'payload.bin')
r=call('pack',dest,source,'--level',0,'--volume-bytes',16384);out=root/'second-out';x=call('unpack',first,'-o',out)
summary=json.loads(r['stdout']) if r['exit']==0 else {}
volumes=summary.get('volumes',[])
actual_parts=sorted(source.glob('backup.7z.[0-9][0-9][0-9]'))
record('self-exclusion-and-stale-tail',r['exit']==0 and x['exit']==0 and inventory(out)==expected and len(actual_parts)==len(volumes)==1 and backup.read_bytes()==b'UNRELATED',[r,x])

before=digest(first);r=call('pack',dest,first,'--volume-bytes',4096)
record('direct-output-input-refused',r['exit']==1 and digest(first)==before,[r])

hole=source/'backup.7z.003';hole.write_bytes(b'GAPPED SET')
r=call('pack',dest,source,'--volume-bytes',4096)
record('incomplete-set-refused',r['exit']==1 and digest(first)==before and hole.read_bytes()==b'GAPPED SET',[r])

outside=root/'outside.txt';outside.write_bytes(b'ORIGINAL')
redirect=root/'redirect.7z.001';os.symlink(outside,redirect)
r=call('pack',root/'redirect.7z',source/'payload.bin','--volume-bytes',4096)
record('symlink-output-refused',r['exit']==1 and outside.read_bytes()==b'ORIGINAL' and redirect.is_symlink(),[r])

plain=root/'plain.7z';r=call('pack',plain,source/'payload.bin');old=plain.read_bytes()
r=call('pack',plain,source/'payload.bin','--volume-bytes',1024)
x=call('test',Path(str(plain)+'.001'))
record('single-to-volume-transition',r['exit']==0 and x['exit']==0 and not plain.exists(),[r,x])

report=dict(passed=all(c['passed'] for c in records),binary_sha256=digest(binary),cases=records)
(root/'results.json').write_text(json.dumps(report,indent=2),encoding='utf8')
print(json.dumps(dict(passed=report['passed'],cases=len(records),report=str(root/'results.json'))))
raise SystemExit(0 if report['passed'] else 1)
