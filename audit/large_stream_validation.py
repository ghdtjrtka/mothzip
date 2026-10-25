"""Measure a real gzip stream beyond the default 10 GiB policy, without large extracted files."""
from pathlib import Path
import argparse,gzip,hashlib,json,subprocess,time,uuid

parser=argparse.ArgumentParser();parser.add_argument('--binary',required=True);args=parser.parse_args()
workspace=Path(__file__).resolve().parents[1];binary=Path(args.binary).resolve()
root=workspace/'audit/runs'/('large-stream-'+time.strftime('%Y%m%d-%H%M%S')+'-'+uuid.uuid4().hex[:6]);root.mkdir(parents=True)
size=10*1024**3+1
archive=root/'large.gz';chunk=b'\0'*(1024**2);remaining=size
start=time.monotonic()
with gzip.open(archive,'wb',compresslevel=1) as output:
    while remaining:
        length=min(remaining,len(chunk));output.write(chunk[:length]);remaining-=length
print(json.dumps(dict(stage='created',input_bytes=size,compressed_bytes=archive.stat().st_size,seconds=round(time.monotonic()-start,2))),flush=True)
command=[str(binary),'peek',str(archive),'--json','--non-interactive','--limit','0','--max-bytes',str(size)]
start=time.monotonic();result=subprocess.run(command,capture_output=True,timeout=180)
summary=json.loads(result.stdout) if result.returncode==0 else None
passed=result.returncode==0 and summary['total_uncompressed_bytes']==size
report=dict(passed=passed,binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),input_bytes=size,compressed_bytes=archive.stat().st_size,command=command,exit=result.returncode,summary=summary,stderr=result.stderr.decode('utf8','replace'),read_seconds=round(time.monotonic()-start,2))
(root/'results.json').write_text(json.dumps(report,indent=2),encoding='utf8')
print(json.dumps(dict(passed=passed,input_bytes=size,read_seconds=report['read_seconds'],report=str(root/'results.json'))),flush=True)
raise SystemExit(0 if passed else 1)
