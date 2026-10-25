"""Exercise actual self-extracting executables, never execute archived members."""
from pathlib import Path
import argparse, hashlib, json, subprocess, time, uuid, zipfile

parser=argparse.ArgumentParser();parser.add_argument('--binary',required=True);args=parser.parse_args()
workspace=Path(__file__).resolve().parents[1];binary=Path(args.binary).resolve()
root=workspace/'audit/runs'/('sfx-'+time.strftime('%Y%m%d-%H%M%S')+'-'+uuid.uuid4().hex[:6]);root.mkdir(parents=True)
records=[]
def invoke(exe,*arguments,input_bytes=None):
    result=subprocess.run([str(exe),*map(str,arguments),'--json'],input=input_bytes,capture_output=True,timeout=30)
    return dict(exit=result.returncode,stdout=result.stdout.decode('utf8','replace'),stderr=result.stderr.decode('utf8','replace'))
def record(name,passed,calls):
    records.append(dict(case=name,passed=passed,calls=calls));print(json.dumps(dict(case=name,passed=passed)),flush=True)
def inventory(path):
    return {str(p.relative_to(path)).replace('\\','/'):hashlib.sha256(p.read_bytes()).hexdigest() for p in path.rglob('*') if p.is_file()}

entries={'hello.txt':b'SFX TEST','folder/payload.bin':bytes(range(256)),'folder/empty.txt':b''}
expected={name:hashlib.sha256(value).hexdigest() for name,value in entries.items()}
archive=root/'input.zip'
with zipfile.ZipFile(archive,'w',compression=zipfile.ZIP_DEFLATED) as output:
    for name,value in entries.items():output.writestr(name,value)
sfx=root/'bundle.exe';call=invoke(binary,'sfx',sfx,archive)
record('create',call['exit']==0 and sfx.is_file(),[call])
assert call['exit']==0,call
summary=json.loads(call['stdout'])
call=invoke(sfx,'--non-interactive','--out',root/'standalone','--to-folder')
record('standalone',call['exit']==0 and inventory(root/'standalone/bundle')==expected,[call])
call=invoke(binary,sfx,'--non-interactive','--out',root/'bare','--to-folder')
record('bare-path',call['exit']==0 and inventory(root/'bare/bundle')==expected,[call])
calls=[invoke(binary,'test',sfx),invoke(binary,'peek',sfx),invoke(binary,'unpack',sfx,'-o',root/'explicit')]
record('ordinary-reader',all(c['exit']==0 for c in calls) and inventory(root/'explicit')==expected,calls)
second=root/'second.exe';call=invoke(sfx,'sfx',second,archive)
second_info=json.loads(call['stdout']) if call['exit']==0 else {}
record('no-nested-stub',call['exit']==0 and second_info.get('stub_bytes')==summary['stub_bytes'] and second_info.get('embedded_archive_bytes')==archive.stat().st_size,[call])
existing=root/'original.exe';existing.write_bytes(b'ORIGINAL')
call=invoke(binary,'sfx',existing,archive,'--max-bytes',1)
record('budget-preserves-output',call['exit']==1 and existing.read_bytes()==b'ORIGINAL',[call])
badzip=root/'broken.zip';badzip.write_bytes(b'BROKEN')
call=invoke(binary,'sfx',existing,badzip)
record('corruption-preserves-output',call['exit']==1 and existing.read_bytes()==b'ORIGINAL',[call])
broken=root/'broken.exe';data=bytearray(sfx.read_bytes());data[-28:-20]=b'\0'*8;broken.write_bytes(data)
call=invoke(broken,'--non-interactive','--out',root/'broken-output')
record('invalid-footer',call['exit']==1 and not (root/'broken-output').exists(),[call])
plain=root/'private.txt';plain.write_bytes(b'PRIVATE SFX PAYLOAD')
encrypted=root/'encrypted.zip';encrypted_sfx=root/'encrypted.exe'
calls=[invoke(binary,'pack',encrypted,plain,'--password-stdin',input_bytes=b'fixture-only\n'),
       invoke(binary,'sfx',encrypted_sfx,encrypted,'--password-stdin',input_bytes=b'fixture-only\n'),
       invoke(encrypted_sfx,'--non-interactive','--out',root/'encrypted-output','--password-stdin',input_bytes=b'fixture-only\n')]
record('encrypted',all(c['exit']==0 for c in calls) and (root/'encrypted-output/encrypted/private.txt').read_bytes()==plain.read_bytes(),calls)
report=dict(passed=all(c['passed'] for c in records),binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),cases=records)
(root/'results.json').write_text(json.dumps(report,ensure_ascii=False,indent=2),encoding='utf8')
print(json.dumps(dict(passed=report['passed'],cases=len(records),report=str(root/'results.json'))))
raise SystemExit(0 if report['passed'] else 1)
