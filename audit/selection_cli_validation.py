"""Validate selected/flat CLI outputs against literal member identity and reference bytes."""
from pathlib import Path
import argparse,gzip,hashlib,io,json,subprocess,tarfile,time,unicodedata,uuid,zipfile

p=argparse.ArgumentParser();p.add_argument('--binary',required=True);args=p.parse_args()
workspace=Path(__file__).resolve().parents[1];binary=Path(args.binary).resolve()
root=workspace/'audit/runs'/('selection-cli-'+time.strftime('%Y%m%d-%H%M%S')+'-'+uuid.uuid4().hex[:6]);root.mkdir(parents=True)
results=[]
def call(*args,input_bytes=None):
    r=subprocess.run([str(binary),*map(str,args),'--json','--non-interactive'],input=input_bytes,capture_output=True,timeout=30)
    return dict(exit=r.returncode,stdout=r.stdout.decode('utf8','replace'),stderr=r.stderr.decode('utf8','replace'))
def inventory(path):return {str(f.relative_to(path)).replace('\\','/'):f.read_bytes().hex() for f in path.rglob('*') if f.is_file()}
def record(name,passed,calls):results.append(dict(case=name,passed=passed,calls=calls));print(json.dumps(dict(case=name,passed=passed)),flush=True)
raw=unicodedata.normalize('NFD','한글')+'.txt'
for ext in ('zip','tar','tar.gz'):
    archive=root/('identity.'+ext)
    entries=[(raw,b'NFD'),('other.txt',b'OTHER'),('left/x.txt',b'LEFT'),('right/x.txt',b'RIGHT'),('leftovers/x.txt',b'KEEP')]
    if ext=='zip':
        with zipfile.ZipFile(archive,'w') as z:
            for name,value in entries:z.writestr(name,value)
    else:
        with tarfile.open(archive,'w:gz' if ext.endswith('.gz') else 'w') as z:
            for name,value in entries:
                h=tarfile.TarInfo(name);h.size=len(value);z.addfile(h,io.BytesIO(value))
    out=root/('nfd-'+ext);r=call('unpack',archive,'--member',raw,'-o',out)
    record('raw-identity-'+ext,r['exit']==0 and inventory(out)=={'한글.txt':b'NFD'.hex()},[r])
    out=root/('flat-'+ext);r=call('unpack',archive,'--member','left','--member','right','--flat','-o',out)
    record('flat-boundary-'+ext,r['exit']==0 and sorted(inventory(out).values())==sorted([b'LEFT'.hex(),b'RIGHT'.hex()]) and len(inventory(out))==2,[r])
    out=root/('missing-'+ext);out.mkdir();(out/'sentinel.txt').write_bytes(b'ORIGINAL');r=call('unpack',archive,'--member','absent','-o',out)
    record('missing-atomic-'+ext,r['exit']==1 and inventory(out)=={'sentinel.txt':b'ORIGINAL'.hex()},[r])

ref=json.loads((workspace/'audit/runs/rar-reference-20261025-201124/reference.json').read_text(encoding='utf8'))['cases']
for fragment in ('rar3-solid','rar5-solid','rar5-psw'):
    case=next(c for c in ref if fragment in c['archive']);name=sorted(case['files'])[-1];out=root/fragment
    password=case.get('password');flags=['--password-stdin'] if password else []
    r=call('unpack',case['archive'],'--member',name,'-o',out,*flags,input_bytes=(password+'\n').encode() if password else None)
    files=inventory(out);actual={name:hashlib.sha256(bytes.fromhex(content)).hexdigest() for name,content in files.items()}
    record('later-solid-'+fragment,r['exit']==0 and actual=={name:case['files'][name]},[r])

unicode_dir=root/'한글';unicode_dir.mkdir();(unicode_dir/'sample.txt').write_bytes(b'INPUT')
r=call(unicode_dir);record('bare-unicode-directory',r['exit']==0 and (root/'한글.zip').is_file(),[r])
report=dict(passed=all(c['passed'] for c in results),binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),cases=results)
(root/'results.json').write_text(json.dumps(report,ensure_ascii=False,indent=2),encoding='utf8')
print(json.dumps(dict(passed=report['passed'],cases=len(results),report=str(root/'results.json'))))
raise SystemExit(0 if report['passed'] else 1)
