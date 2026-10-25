"""Executable release gate: exit nonzero for unsafe filesystem results or broken CLI contracts."""
import ctypes
import gzip
import io
import hashlib
import json
import os
from pathlib import Path
import runpy
import threading
import time

round3=runpy.run_path(str(Path(__file__).with_name('reaudit_round3.py')),run_name='__main__')
base=round3['base']; ROOT=base['ROOT']; BINARY=base['BINARY']
fixture_dir,archive,invoke,record=(base[n] for n in ('fixture_dir','archive','invoke','record'))
kernel=round3['kernel']

# The second target is refused after the first file has already been installed: require
# whole-set rollback. A directory at the destination makes the refusal deterministic; the
# earlier version raced a thread to lock a staging file and missed the window on CI.
d=fixture_dir('release-late-commit-failure')
out=d/'out'; out.mkdir()
(out/'first.txt').write_bytes(b'FIRST ORIGINAL')
(out/'second.txt').mkdir()
(out/'second.txt'/'inside.txt').write_bytes(b'SECOND ORIGINAL')
a=d/'attack.zip'; archive(a,[('first.txt',b'CHANGED'),('second.txt',b'CHANGED')])
call=invoke('unpack',a,'-o',out)
staging=list(out.glob('.mothzip_stage_*'))
record(d.name,[call],
    first_preserved=(out/'first.txt').read_bytes()==b'FIRST ORIGINAL',
    second_preserved=(out/'second.txt'/'inside.txt').read_bytes()==b'SECOND ORIGINAL',
    no_staging=not staging,
    refused=call.get('exit_code')==1)


for ext in ('zip','tar'):
    d=fixture_dir('release-unicode-display-'+ext)
    a=d/('valid.'+ext); archive(a,[('한글'*40+'.'+'😀'*30,b'VALID')])
    call=invoke('peek',a,'--limit',1)
    record(d.name,[call],valid_json=call.get('exit_code')==0 and bool(json.loads(call['stdout'])))

d=fixture_dir('release-peek-long-root')
a=d/'long.zip'; archive(a,[('r'*60000+'/file.txt',b'VALID')])
call=invoke('peek',a,'--limit',1)
record(d.name,[call],stdout_bytes=len(call.get('stdout','').encode()))

d=fixture_dir('release-gzip-limit-zero')
a=d/'plain.txt.gz';a.write_bytes(gzip.compress(b'VALID'))
call=invoke('peek',a,'--limit',0)
summary=json.loads(call['stdout'])
record(d.name,[call],tree_items=len(summary['tree_summary']),truncated_items=summary['truncated_items'])

d=fixture_dir('release-gzip-corrupt-second-member')
second=bytearray(gzip.compress(b'SECOND'));second[-8]^=1
a=d/'plain.txt.gz';a.write_bytes(gzip.compress(b'FIRST')+second)
out=d/'out';out.mkdir();(out/'plain.txt').write_bytes(b'ORIGINAL')
call=invoke('unpack',a,'-o',out)
record(d.name,[call],original_preserved=(out/'plain.txt').read_bytes()==b'ORIGINAL')

d=fixture_dir('release-pack-source-link')
src=d/'source';src.mkdir();(d/'outside.txt').write_bytes(b'OUTSIDE')
os.symlink(d/'outside.txt',src/'linked.txt')
dest=d/'backup.zip';dest.write_bytes(b'ORIGINAL BACKUP')
call=invoke('pack',dest,src)
record(d.name,[call],backup_preserved=dest.read_bytes()==b'ORIGINAL BACKUP')

d=fixture_dir('release-json-usage-error')
a=d/'valid.zip';archive(a,[('file.txt',b'VALID')])
call=invoke('peek',a,'--depth','invalid')
record(d.name,[call],json_error=bool(json.loads(call.get('stderr','{}')).get('error')))

for ext in ('zip','tar'):
    d=fixture_dir('release-archive-link-'+ext)
    out=d/'out';out.mkdir();(out/'victim.txt').write_bytes(b'ORIGINAL')
    a=d/('attack.'+ext)
    if ext=='zip':
        with base['zipfile'].ZipFile(a,'w') as z:
            z.writestr('victim.txt',b'ATTACK')
            link=base['zipfile'].ZipInfo('link');link.create_system=3;link.external_attr=0o120777<<16
            z.writestr(link,b'../outside')
    else:
        with base['tarfile'].open(a,'w') as tar:
            h=base['tarfile'].TarInfo('victim.txt');h.size=6;tar.addfile(h,io.BytesIO(b'ATTACK'))
            h=base['tarfile'].TarInfo('link');h.type=base['tarfile'].SYMTYPE;h.linkname='../outside';tar.addfile(h)
    call=invoke('unpack',a,'-o',out)
    record(d.name,[call],original_preserved=(out/'victim.txt').read_bytes()==b'ORIGINAL')

d=fixture_dir('release-split-targz-members')
tar_bytes=io.BytesIO()
with base['tarfile'].open(fileobj=tar_bytes,mode='w') as tar:
    h=base['tarfile'].TarInfo('victim.txt');h.size=5;tar.addfile(h,io.BytesIO(b'VALID'))
raw=tar_bytes.getvalue();a=d/'split.tar.gz';a.write_bytes(gzip.compress(raw[:300])+gzip.compress(raw[300:]))
out=d/'out';call=invoke('smart-unpack',a,'-o',out)
record(d.name,[call],output_preserved=(out/'split/victim.txt').read_bytes()==b'VALID')

d=fixture_dir('release-encrypted-peek')
a=d/'encrypted.zip';base['encrypted_zip'](a)
call=invoke('peek',a)
record(d.name,[call],metadata_correct=call.get('exit_code')==0 and json.loads(call['stdout'])['total_uncompressed_bytes']==22)

d=fixture_dir('release-declared-size-budget')
a=d/'large.tar';h=base['tarfile'].TarInfo('victim.txt');h.size=10*1024**3+1
a.write_bytes(h.tobuf()+b'TINY')
out=d/'out';calls=[invoke('unpack',a,'-o',out),invoke('peek',a),invoke('cat',a,'victim.txt')]
record(d.name,calls,output_file_exists=(out/'victim.txt').exists(),cat_bytes=len(calls[-1].get('stdout','').encode()))

for ext in ('zip','7z','tar','tar.gz','gz'):
    d=fixture_dir('final-pack-format-'+ext)
    src=d/'payload.txt';src.write_bytes(b'PAYLOAD\n'*100)
    a=d/('output.'+ext);out=d/'out'
    calls=[invoke('pack',a,src,'--level',0),invoke('test',a),invoke('unpack',a,'-o',out)]
    files=list(out.rglob('*')) if out.exists() else []
    record(d.name,calls,content_matches=[p.read_bytes() for p in files if p.is_file()]==[src.read_bytes()])

for ext in ('zip','7z'):
    d=fixture_dir('final-pack-inventory-'+ext)
    src=d/'.config';src.mkdir()
    dest=src/('backup.'+ext);dest.write_bytes(b'OLD ARCHIVE')
    nested=src/'nested';nested.mkdir()
    (nested/dest.name).write_bytes(b'SAME NAME DISTINCT FILE')
    (src/'.mothzip_pack_notes.txt').write_bytes(b'USER FILE')
    out=d/'out'
    calls=[invoke('pack',dest,src),invoke('unpack',dest,'-o',out)]
    actual={str(p.relative_to(out)).replace('\\','/'):p.read_bytes() for p in out.rglob('*') if p.is_file()} if out.exists() else {}
    expected={'.config/nested/'+dest.name:b'SAME NAME DISTINCT FILE','.config/.mothzip_pack_notes.txt':b'USER FILE'}
    record(d.name,calls,inventory_matches=actual==expected)
    encrypted=d/('encrypted.'+ext)
    plain=d/'plain.txt';plain.write_bytes(b'PRIVATE FIXTURE')
    calls=[invoke('-p','fixture-only','pack',encrypted,plain),
           invoke('cat',encrypted,'plain.txt','--password-stdin',input_bytes=b'fixture-only\n'),
           invoke('test',encrypted,'--password-stdin',input_bytes=b'wrong\n')]
    record('final-password-contract-'+ext,calls,content_matches=calls[1].get('stdout')=='PRIVATE FIXTURE')

d=fixture_dir('final-pack-single-link')
(d/'real.txt').write_bytes(b'REAL');os.symlink(d/'real.txt',d/'link.txt')
dest=d/'out.zip';dest.write_bytes(b'ORIGINAL')
call=invoke('pack',dest,d/'link.txt')
record(d.name,[call],original_preserved=dest.read_bytes()==b'ORIGINAL')

for ext in ('tar','tar.gz','gz'):
    d=fixture_dir('final-reject-password-'+ext)
    src=d/'plain.txt';src.write_bytes(b'DO NOT STORE WITHOUT ENCRYPTION')
    dest=d/('out.'+ext);dest.write_bytes(b'ORIGINAL')
    call=invoke('pack',dest,src,'--password-stdin',input_bytes=b'fixture-only\n')
    record(d.name,[call],original_preserved=dest.read_bytes()==b'ORIGINAL')

# A failed --from-stdin run used to truncate an existing output before validating anything.
d=fixture_dir('final-stream-atomic-level')
dest=d/'keep.gz'
dest.write_bytes(b'ORIGINAL-BYTES')
call=invoke('pack',dest,'--from-stdin','--format','gz','--level','22',input_bytes=b'payload that must not be written\n')
record(d.name,[call],original_preserved=dest.read_bytes()==b'ORIGINAL-BYTES',
    no_staging=not list(d.glob('.mothzip_stream*')))

# A stream that cannot be read to the end (stdin closed early) must not replace the destination either.
d=fixture_dir('final-stream-atomic-container')
dest=d/'keep.gz'
dest.write_bytes(b'ORIGINAL-BYTES')
call=invoke('pack',dest,'--from-stdin','--format','zip',input_bytes=b'payload\n')
record(d.name,[call],original_preserved=dest.read_bytes()==b'ORIGINAL-BYTES',
    no_staging=not list(d.glob('.mothzip_stream*')))

# The options that cannot apply to a pipe are argument errors, not silent no-ops.
d=fixture_dir('final-stream-rejects-pipe-options')
dest=d/'out.gz'
conflicts=[]
for extra in (['--password','secret'],['--password-stdin'],['--reproducible'],['--7z-dict','4'],
              ['--volume-bytes','65536'],['a.txt']):
    kwargs={'input_bytes':b'secret\n'} if '--password-stdin' in extra else {'input_bytes':b'x'}
    call=invoke('pack',dest,'--from-stdin','--format','gz',*extra,**kwargs)
    conflicts.append(call.get('exit_code'))
record(d.name,[],conflict_exits=conflicts,no_output=not dest.exists())

# A successful stream pack must equal the same bytes compressed from a file.
d=fixture_dir('final-stream-matches-file')
payload=b'stream equivalence payload\n'*64
src=d/'payload.zst'
invoke('pack',src,'--from-stdin','--format','zst',input_bytes=payload)
produced=d/'produced.zst'
invoke('pack',produced,'--from-stdin','--format','zst',input_bytes=payload)
unpack=invoke('unpack',src,'-o',d/'out')
restored=list((d/'out').rglob('*')) if (d/'out').exists() else []
restored=[p for p in restored if p.is_file()]
record(d.name,[unpack],same_bytes=src.read_bytes()==produced.read_bytes(),
    restored=len(restored)==1 and restored[0].read_bytes()==payload)

d=fixture_dir('final-headless-no-open')
call=invoke('unpack',d/'anything.zip','--open','--non-interactive')
record(d.name,[call],json_error=bool(json.loads(call.get('stderr','{}')).get('error')))

for ext in ('zip','7z','tar','tar.gz','gz'):
    d=fixture_dir('final-configurable-budget-'+ext)
    source=d/'payload.txt';source.write_bytes(b'BUDGET TEST\n'*1024)
    a=d/('payload.'+ext);out=d/'out';out.mkdir()
    destination=out/('payload' if ext=='gz' else 'payload.txt');destination.write_bytes(b'ORIGINAL')
    calls=[invoke('pack',a,source),invoke('unpack',a,'-o',out,'--max-bytes',64)]
    original_preserved=destination.read_bytes()==b'ORIGINAL'
    calls += [invoke('--max-bytes',65536,'unpack',a,'-o',out),
              invoke('cat',a,'payload.txt','--max-bytes',64),
              invoke('cat',a,'payload.txt','--max-bytes',65536),
              invoke('test',a,'--max-bytes',64),invoke('test',a,'--max-bytes',65536)]
    record(d.name,calls,original_preserved=original_preserved,restored=destination.read_bytes()==source.read_bytes(),stream_matches=calls[4].get('stdout')==source.read_text())

failures=[]
def require(ok,case,message):
    if not ok: failures.append(dict(case=case,message=message))
for test in base['results']:
    name=test['case']; o=test['observed']; calls=test['calls']
    for call in calls:
        require(not call.get('timeout'),name,'timeout')
        require(call.get('exit_code')!=101 and 'panicked' not in call.get('stderr',''),name,'panic')
    code=calls[0].get('exit_code') if calls else None
    if 'setup_error' in o: require(False,name,'fixture setup failed');continue
    if name.startswith(('archive-stem-','junction-','hardlink-','leaf-symlink-','reaudit-root-junction-')):
        require(o['outside_content']=='ORIGINAL',name,'outside file modified')
    elif name=='crc-existing-file':
        require(code==1 and (ROOT/name/'out/victim.txt').read_bytes()==b'ORIGINAL IMPORTANT CONTENT',name,'CRC failure destroyed original')
    elif name.startswith('late-traversal-'):
        require(code==1 and o['existing_content']=='ORIGINAL' and not o['partial_file_exists'] and not o['traversal_escaped'],name,'partial commit or traversal')
    elif name.startswith('collision-') or name.startswith('reaudit-case-collision-'):
        require(code==0 and o['output_files']==2 and sorted(o['output_contents'])==['FIRST','SECOND'],name,'lost collision member')
    elif name=='pack-failure-destroys-existing':
        require(code==1 and (ROOT/name/'backup.zip').read_bytes()==b'ORIGINAL BACKUP',name,'backup lost')
    elif name=='plain-gzip':
        require(all(c['exit_code']==0 for c in calls) and (ROOT/name/'out/report.txt').read_bytes()==b'VALID PLAIN GZIP',name,'plain gzip failure')
    elif name=='truncated-tar': require(code==1 and o['written_bytes'] is None,name,'truncated file left')
    elif name=='gzip-bad-trailer': require(code==1,name,'bad gzip accepted')
    elif name=='tar-extreme-mtime': require(code==0,name,'extreme mtime crash')
    elif name=='zip-deceptive-size-control': require(code==1 and not o['destination_exists'],name,'size deception accepted')
    elif name=='encrypted-zip':
        require(all(c['exit_code']==0 for c in calls) and calls[-1]['stdout']=='ENCRYPTED TEST CONTENT',name,'encrypted operation failed')
    elif name=='peek-output-budget': require(code==0 and o['stdout_bytes']<1000,name,'unbounded extension output')
    elif name.startswith('guard-control:'): require(code==1 and o['rejected'] and not o['outside_created'],name,'path guard failed')
    elif name.startswith('roundtrip-'): require(code==0 and o['output_matches'],name,'normal extraction failed')
    elif name.startswith('reaudit-directory-to-file-'): require(code==1 and o['original_exists'] and not o['replacement_is_file'],name,'directory destroyed')
    elif name.startswith('reaudit-file-to-directory-'): require(code==1 and o['original_file_preserved'],name,'file destroyed')
    elif name.startswith('reaudit-commit-failure-'):
        require(code==1 and o['existing_content']=='ORIGINAL' and not o['staging_dirs'],name,'partial commit')
    elif name.startswith(('reaudit-readonly-hardlink-','round3-readonly-hardlink-locked-')):
        require(o['outside_content']=='ORIGINAL' and o['outside_readonly_before'] and o['outside_readonly_after'],name,'outside metadata modified')
    elif name.startswith('reaudit-cat-identity-'): require(code==0 and o['actual_content']==o['requested_content'],name,'wrong member returned')
    elif name.startswith('reaudit-filter-skips-data-'): require(code==0 and o['output_files']==1,name,'normal member skipped')
    elif name=='reaudit-pack-rename-failure': require(code==1 and o['stage_lock_acquired'] and o['backup_content']=='ORIGINAL BACKUP',name,'locked rename lost backup')
    elif name=='reaudit-gzip-multiple-members': require(all(c['exit_code']==0 for c in calls) and o['actual_content']==o['expected_content'] and calls[-1]['stdout']==o['expected_content'],name,'gzip member lost')
    elif name=='reaudit-peek-long-name': require(code==0 and o['stdout_bytes']<1000,name,'long display not bounded')
    elif name=='reaudit-cat-mixed-encryption': require(code==0 and calls[0]['stdout']=='PUBLIC',name,'unrelated encrypted member blocks cat')
    elif name.startswith('round3-archive-internal-type-conflict-'): require(code==1 and o['original_content']=='ORIGINAL' and not o['new_file_exists'] and not o['staging_dirs'],name,'internal tree conflict modified originals')
    elif name.startswith('round3-peek-unicode-'): require(code==0 and not o['panicked'],name,'unicode display failed')
    elif name.startswith('round3-unicode-case-collision-'): require(code==0 and o['output_files']==2 and sorted(o['output_contents'])==['FIRST','SECOND'],name,'Unicode member lost')
    elif name=='round3-extraction-rename-locked': require(code==1 and o['stage_lock_acquired'] and o['original_preserved'],name,'rename lock lost original')
    elif name=='round3-cp949-cat': require(all(c['exit_code']==0 for c in calls) and o['cat_content']==o['requested_content'],name,'legacy name lookup failed')
    elif name=='release-late-commit-failure': require(code==1 and o['refused'] and o['first_preserved'] and o['second_preserved'] and o['no_staging'],name,'whole-set rollback failed')
    elif name.startswith('release-unicode-display-'): require(code==0 and o['valid_json'],name,'Unicode slicing or JSON failure')
    elif name=='release-peek-long-root': require(code==0 and o['stdout_bytes']<1000,name,'root name unbounded')
    elif name=='release-gzip-limit-zero': require(code==0 and o['tree_items']==0 and o['truncated_items']==1,name,'count limit ignored')
    elif name=='release-gzip-corrupt-second-member': require(code==1 and o['original_preserved'],name,'second gzip CRC failure committed')
    elif name=='release-pack-source-link': require(code==1 and o['backup_preserved'],name,'unsupported input silently omitted')
    elif name=='release-json-usage-error': require(code==2 and o['json_error'],name,'usage error violates JSON contract')
    elif name.startswith('release-archive-link-'): require(code==1 and o['original_preserved'],name,'unsupported link committed or omitted')
    elif name=='release-split-targz-members': require(code==0 and o['output_preserved'],name,'gzip boundary split rejected')
    elif name=='release-encrypted-peek': require(code==0 and o['metadata_correct'],name,'encrypted metadata requires password')
    elif name=='release-declared-size-budget': require(all(c['exit_code']==1 for c in calls) and not o['output_file_exists'] and o['cat_bytes']==0,name,'large declaration consumed output budget')
    elif name.startswith('final-pack-format-'): require(all(c.get('exit_code')==0 for c in calls) and o['content_matches'],name,'format creation/test/extraction mismatch')
    elif name.startswith('final-pack-inventory-'): require(all(c.get('exit_code')==0 for c in calls) and o['inventory_matches'],name,'source inventory lost or included a file')
    elif name.startswith('final-password-contract-'): require([c.get('exit_code') for c in calls]==[0,0,1] and o['content_matches'],name,'password or global argument contract failed')
    elif name=='final-pack-single-link' or name.startswith('final-reject-password-'): require(code==1 and o['original_preserved'],name,'unsupported source/option modified destination')
    elif name.startswith('final-stream-atomic-'): require(code==1 and o['original_preserved'] and o['no_staging'],name,'failed stdin pack modified the destination')
    elif name=='final-stream-rejects-pipe-options': require(o['conflict_exits']==[2,2,2,2,2,2] and o['no_output'],name,'pipe option silently ignored')
    elif name=='final-stream-matches-file': require(o['same_bytes'] and o['restored'],name,'stdin stream differs from the file form')
    elif name=='final-headless-no-open': require(code==2 and o['json_error'],name,'headless command accepted desktop window')
    elif name.startswith('final-configurable-budget-'): require([c.get('exit_code') for c in calls]==[0,1,0,1,0,1,0] and o['original_preserved'] and o['restored'] and o['stream_matches'],name,'byte budget ignored or could not be raised')
    else: require(False,name,'missing release assertion')

workspace=base['WORKSPACE']
sources=list((workspace/'src').rglob('*.rs'))+[workspace/n for n in ('Cargo.toml','Cargo.lock','build.rs','.cargo/config.toml','assets/moth.ico')]
report=dict(binary=str(BINARY),binary_sha256=hashlib.sha256(BINARY.read_bytes()).hexdigest(),
    source_files_sha256={str(p.relative_to(workspace)):hashlib.sha256(p.read_bytes()).hexdigest() for p in sources},
    cases=base['results'],case_count=len(base['results']),call_count=sum(len(t['calls']) for t in base['results']),
    failures=failures,passed=not failures)
(ROOT/'release-validation.json').write_text(json.dumps(report,ensure_ascii=False,indent=2),encoding='utf8')
print(json.dumps(dict(passed=not failures,cases=report['case_count'],calls=report['call_count'],failures=failures,report=str(ROOT/'release-validation.json'))))
raise SystemExit(1 if failures else 0)
