"""Read-only product audit: extend the previous fixtures with commit, Unicode, and legacy-name cases."""
import ctypes
from ctypes import wintypes
import json
import os
from pathlib import Path
import runpy
import threading
import time

previous=runpy.run_path(str(Path(__file__).with_name('reaudit_20261025.py')),run_name='__main__')
base=previous['base']
ROOT,BINARY=base['ROOT'],base['BINARY']
fixture_dir,archive,invoke,record=(base[n] for n in ('fixture_dir','archive','invoke','record'))
kernel=previous['kernel']

for ext in ('zip','tar'):
    d=fixture_dir(f'round3-archive-internal-type-conflict-{ext}')
    out=d/'out'; out.mkdir(); (out/'victim.txt').write_bytes(b'ORIGINAL')
    a=d/f'attack.{ext}'
    archive(a,[('victim.txt',b'ATTACK'),('nested',b'FILE'),('nested/child.txt',b'CHILD')])
    call=invoke('unpack',a,'-o',out)
    record(d.name,[call],original_content=(out/'victim.txt').read_text(),new_file_exists=(out/'nested').is_file(),staging_dirs=[p.name for p in out.glob('.mothzip_stage_*')])

    d=fixture_dir(f'round3-peek-unicode-{ext}')
    a=d/f'valid.{ext}'; archive(a,[('\uac00'*50+'.txt',b'VALID')])
    call=invoke('peek',a,'--limit',1)
    record(d.name,[call],filename_utf8_bytes=154,panicked='panicked' in call.get('stderr',''))

    d=fixture_dir(f'round3-readonly-hardlink-locked-{ext}')
    out=d/'out'; out.mkdir(); victim=d/'outside.txt'; victim.write_bytes(b'ORIGINAL')
    linked=out/'victim.txt'; os.link(victim,linked); victim.chmod(0o444)
    a=d/f'attack.{ext}'; archive(a,[('victim.txt',b'ATTACK')])
    h=kernel.CreateFileW(str(linked),0x80000000,3,None,3,0,None)
    assert h not in (None,ctypes.c_void_p(-1).value),ctypes.get_last_error()
    before=kernel.GetFileAttributesW(str(victim))
    try:
        call=invoke('unpack',a,'-o',out)
    finally:
        kernel.CloseHandle(h)
    after=kernel.GetFileAttributesW(str(victim))
    record(d.name,[call],outside_content=victim.read_text(),outside_readonly_before=bool(before&1),outside_readonly_after=bool(after&1))

    # Compare with an independently measured Windows filesystem name equivalence.
    d=fixture_dir(f'round3-unicode-case-collision-{ext}')
    probe=d/'probe'; probe.mkdir(); (probe/'S.txt').write_bytes(b'FIRST'); (probe/'\u017f.txt').write_bytes(b'SECOND')
    reference_files=len(list(probe.iterdir()))
    a=d/f'attack.{ext}'; archive(a,[('S.txt',b'FIRST'),('\u017f.txt',b'SECOND')])
    out=d/'out'; call=invoke('unpack',a,'-o',out)
    files=[p for p in out.rglob('*') if p.is_file() and not any(c.startswith('.mothzip_stage_') for c in p.parts)]
    record(d.name,[call],filesystem_distinct_names=reference_files,input_files=2,output_files=len(files),output_contents=[p.read_text() for p in files])

# Hold the verified staged payload against rename. The old destination must survive that error.
d=fixture_dir('round3-extraction-rename-locked')
out=d/'out'; out.mkdir(); victim=out/'victim.txt'; victim.write_bytes(b'ORIGINAL')
a=d/'attack.zip'; archive(a,[('victim.txt',os.urandom(8*1024*1024))])
stop=threading.Event(); locked=threading.Event(); handles=[]

def hold_extract_stage():
    deadline=time.monotonic()+10
    while not stop.is_set() and time.monotonic()<deadline:
        for p in out.glob('.mothzip_stage_*/0.tmp'):
            h=kernel.CreateFileW(str(p),0x80000000,3,None,3,0,None)
            if h not in (None,ctypes.c_void_p(-1).value):
                handles.append(h); locked.set(); return
        stop.wait(0.001)

watcher=threading.Thread(target=hold_extract_stage); watcher.start()
try:
    call=invoke('unpack',a,'-o',out)
finally:
    stop.set(); watcher.join(timeout=11)
    for h in handles:
        kernel.CloseHandle(h)
record(d.name,[call],stage_lock_acquired=locked.is_set(),original_exists=victim.exists(),original_preserved=victim.exists() and victim.read_bytes()==b'ORIGINAL')

# A CP949 ZIP advertises no UTF-8 flag. cat must use the same decoding as peek/extract.
d=fixture_dir('round3-cp949-cat')
a=d/'legacy.zip'; encoded='\ud64d\uae38\ub3d9.txt'.encode('cp949'); placeholder=b'x'*len(encoded)
archive(a,[(placeholder.decode(),b'LEGACY CONTENT')])
raw=a.read_bytes(); assert raw.count(placeholder)==2
a.write_bytes(raw.replace(placeholder,encoded))
out=d/'out'
calls=[invoke('peek',a),invoke('unpack',a,'-o',out),invoke('cat',a,'\ud64d\uae38\ub3d9.txt')]
record(d.name,calls,requested_content='LEGACY CONTENT',cat_content=calls[-1].get('stdout'),output_names=[p.name for p in out.iterdir()] if out.exists() else [])

report=dict(binary=str(BINARY),binary_sha256=base['hashlib'].sha256(BINARY.read_bytes()).hexdigest(),
            source_commit=base['source_commit'](),
            run_dir=str(ROOT),cases=base['results'])
(ROOT/'round3-results.json').write_text(json.dumps(report,ensure_ascii=False,indent=2),encoding='utf8')
print('ROUND3_RESULTS='+str(ROOT/'round3-results.json'))
