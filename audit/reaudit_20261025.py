"""Extend the original audit using the same fixture/CLI producers, without changing product code."""
import ctypes
from ctypes import wintypes
import json
import os
from pathlib import Path
import runpy
import subprocess
import threading
import time
import zipfile
import gzip

base = runpy.run_path(str(Path(__file__).with_name('adversarial_audit.py')), run_name='__main__')
ROOT, BINARY = base['ROOT'], base['BINARY']
fixture_dir, archive, invoke, record = (base[n] for n in ('fixture_dir', 'archive', 'invoke', 'record'))

for ext in ('zip', 'tar'):
    for command, flags in [('smart-unpack', []), ('unpack', ['--to-folder'])]:
        d = fixture_dir(f'reaudit-root-junction-{ext}-{command}')
        out, outside = d/'out', d/'outside'; out.mkdir(); outside.mkdir()
        victim = outside/'victim.txt'; victim.write_bytes(b'ORIGINAL')
        junction = out/'attack'
        setup = subprocess.run(['powershell', '-NoProfile', '-Command',
            f"New-Item -ItemType Junction -Path '{junction}' -Target '{outside}' | Out-Null"], capture_output=True, timeout=12)
        assert setup.returncode == 0, setup.stderr
        a = d/f'attack.{ext}'; archive(a, [('victim.txt',b'ATTACK')])
        call = invoke(command,a,'-o',out,*flags)
        record(d.name,[call],outside_content=victim.read_text(),outside_modified=victim.read_bytes()!=b'ORIGINAL')

for ext in ('zip', 'tar'):
    # A file entry must not recursively destroy an unrelated pre-existing directory.
    d=fixture_dir(f'reaudit-directory-to-file-{ext}')
    out=d/'out'; (out/'project').mkdir(parents=True)
    original=out/'project'/'important.txt'; original.write_bytes(b'UNRELATED USER DATA')
    a=d/f'attack.{ext}'; archive(a,[('project',b'ATTACK')])
    call=invoke('unpack',a,'-o',out)
    record(d.name,[call],original_exists=original.exists(),replacement_is_file=(out/'project').is_file())

    # Existing file -> directory conversion is similarly destructive.
    d=fixture_dir(f'reaudit-file-to-directory-{ext}')
    out=d/'out'; out.mkdir(); (out/'project').write_bytes(b'ORIGINAL')
    a=d/f'attack.{ext}'
    if ext=='zip':
        archive(a,[('project/',b'')])
    else:
        with base['tarfile'].open(a,'w') as t:
            h=base['tarfile'].TarInfo('project'); h.type=base['tarfile'].DIRTYPE; t.addfile(h)
    call=invoke('unpack',a,'-o',out)
    record(d.name,[call],original_file_preserved=(out/'project').is_file(),replacement_is_directory=(out/'project').is_dir())

    # Stage succeeds, but the second commit cannot use an existing file as a parent directory.
    d=fixture_dir(f'reaudit-commit-failure-{ext}')
    out=d/'out'; out.mkdir()
    (out/'victim.txt').write_bytes(b'ORIGINAL'); (out/'blocked').write_bytes(b'BLOCKER')
    a=d/f'attack.{ext}'; archive(a,[('victim.txt',b'ATTACK'),('blocked/new.txt',b'NEW')])
    call=invoke('unpack',a,'-o',out)
    record(d.name,[call],existing_content=(out/'victim.txt').read_text(),staging_dirs=[p.name for p in out.glob('.mothzip_stage_*')])

    # PathBuf equality is case sensitive; the default Windows filesystem is not.
    d=fixture_dir(f'reaudit-case-collision-{ext}')
    a=d/f'attack.{ext}'; archive(a,[('Config.txt',b'FIRST'),('config.txt',b'SECOND')])
    out=d/'out'; call=invoke('unpack',a,'-o',out)
    files=[p for p in out.rglob('*') if p.is_file()]
    record(d.name,[call],input_files=2,output_files=len(files),output_contents=[p.read_text() for p in files])

    # Removing a link after changing its permissions still modifies the linked outside object.
    d=fixture_dir(f'reaudit-readonly-hardlink-{ext}')
    out=d/'out'; out.mkdir(); victim=d/'outside.txt'; victim.write_bytes(b'ORIGINAL')
    os.link(victim,out/'victim.txt'); victim.chmod(0o444)
    a=d/f'attack.{ext}'; archive(a,[('victim.txt',b'ATTACK')])
    before=ctypes.windll.kernel32.GetFileAttributesW(str(victim))
    call=invoke('unpack',a,'-o',out)
    after=ctypes.windll.kernel32.GetFileAttributesW(str(victim))
    record(d.name,[call],outside_content=victim.read_text(),outside_readonly_before=bool(before&1),outside_readonly_after=bool(after&1))

    # Exact lookup of the second member must win over a normalized match to the first member.
    for label,names in [('case',['Config.txt','config.txt']),('nfc',['\u1100\u1161.txt','\uac00.txt'])]:
        d=fixture_dir(f'reaudit-cat-identity-{ext}-{label}')
        a=d/f'attack.{ext}'; archive(a,[(names[0],b'FIRST'),(names[1],b'SECOND')])
        call=invoke('cat',a,names[1])
        record(d.name,[call],requested_content='SECOND',actual_content=call.get('stdout'))

    d=fixture_dir(f'reaudit-filter-skips-data-{ext}')
    a=d/f'attack.{ext}'; archive(a,[('notes.mothzip_stage_project.txt',b'IMPORTANT')])
    out=d/'out'; call=invoke('unpack',a,'-o',out)
    record(d.name,[call],input_files=1,output_files=len([p for p in out.rglob('*') if p.is_file()]))

# Hold the staging file without FILE_SHARE_DELETE, simulating a concurrent scanner or backup reader.
# Existing backup should survive even when final rename cannot complete.
d=fixture_dir('reaudit-pack-rename-failure')
dest=d/'backup.zip'; dest.write_bytes(b'ORIGINAL BACKUP')
source=d/'source.bin'; source.write_bytes(os.urandom(8*1024*1024))
kernel=ctypes.WinDLL('kernel32',use_last_error=True)
kernel.CreateFileW.argtypes=[wintypes.LPCWSTR,wintypes.DWORD,wintypes.DWORD,wintypes.LPVOID,wintypes.DWORD,wintypes.DWORD,wintypes.HANDLE]
kernel.CreateFileW.restype=wintypes.HANDLE
kernel.CloseHandle.argtypes=[wintypes.HANDLE]; kernel.CloseHandle.restype=wintypes.BOOL
stop=threading.Event(); locked=threading.Event(); handle=[]

def hold_stage():
    deadline=time.monotonic()+10
    while not stop.is_set() and time.monotonic()<deadline:
        for p in d.glob('.mothzip_pack_*'):
            h=kernel.CreateFileW(str(p),0x80000000,3,None,3,0,None)
            if h not in (None,ctypes.c_void_p(-1).value):
                handle.append(h); locked.set(); return
        stop.wait(0.001)

watcher=threading.Thread(target=hold_stage); watcher.start()
try:
    call=invoke('pack',dest,source)
finally:
    stop.set(); watcher.join(timeout=11)
    for h in handle:
        kernel.CloseHandle(h)
record(d.name,[call],stage_lock_acquired=locked.is_set(),backup_exists=dest.exists(),backup_content=dest.read_text() if dest.exists() else None)

# Multi-member GZIP is valid; Python's gzip decoder provides an independent expected result.
d=fixture_dir('reaudit-gzip-multiple-members')
a=d/'report.txt.gz'; a.write_bytes(gzip.compress(b'FIRST')+gzip.compress(b'SECOND'))
out=d/'out'; call=invoke('unpack',a,'-o',out)
record(d.name,[call,invoke('cat',a,'report.txt')],expected_content=gzip.decompress(a.read_bytes()).decode(),actual_content=(out/'report.txt').read_text())

# The count limit does not constrain individual path and extension key lengths.
d=fixture_dir('reaudit-peek-long-name')
a=d/'long.zip'; archive(a,[('f.'+'x'*60000,b'X')])
call=invoke('peek',a,'--limit',1)
summary=json.loads(call['stdout'])
# Store the actual output in evidence while displaying only its measured length here.
record(d.name,[call],limit=1,tree_items=len(summary['tree_summary']),stdout_bytes=len(call['stdout'].encode()),extension_keys=len(summary['extension_distribution']))

# An unrelated encrypted member prevents reading a following public member.
d=fixture_dir('reaudit-cat-mixed-encryption')
a=d/'mixed.zip'; base['encrypted_zip'](a)
with zipfile.ZipFile(a,'a') as z:
    z.writestr('public.txt',b'PUBLIC')
call=invoke('cat',a,'public.txt')
record(d.name,[call],reference_content=zipfile.ZipFile(a).read('public.txt').decode())

report=dict(binary=str(BINARY),binary_sha256=base['hashlib'].sha256(BINARY.read_bytes()).hexdigest(),
            source_commit=base['source_commit'](),
            run_dir=str(ROOT),cases=base['results'])
(ROOT/'reaudit-results.json').write_text(json.dumps(report,ensure_ascii=False,indent=2),encoding='utf8')
print('REAUDIT_RESULTS='+str(ROOT/'reaudit-results.json'))
