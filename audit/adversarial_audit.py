r"""Bounded adversarial CLI probes. All fixtures and redirection targets stay in this workspace.

Run: python audit\adversarial_audit.py [--binary target\release\mothzip.exe]
No product files, user registry entries, or real user data are modified.
"""
import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import struct
import subprocess
import tarfile
import time
import uuid
import zipfile
import zlib

WORKSPACE = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument('--binary', default='target/debug/mothzip.exe')
args = parser.parse_args()
BINARY = (WORKSPACE / args.binary).resolve()
ROOT = WORKSPACE / 'audit' / 'runs' / (time.strftime('%Y%m%d-%H%M%S') + '-' + uuid.uuid4().hex[:6])
ROOT.mkdir(parents=True)
results = []


def source_commit():
    """A source archive has hashes but no repository commit; never borrow its parent's Git identity."""
    try:
        p = subprocess.run(['git', 'rev-parse', '--show-toplevel', 'HEAD'], cwd=WORKSPACE,
                           capture_output=True, text=True, encoding='utf8')
    except FileNotFoundError:
        return None
    lines = p.stdout.splitlines()
    return lines[1] if p.returncode == 0 and len(lines) == 2 and Path(lines[0]).resolve() == WORKSPACE else None


def invoke(*args, input_bytes=None):
    command = [str(BINARY), *map(str, args), '--json']
    start = time.monotonic()
    try:
        p = subprocess.run(command, input=input_bytes, capture_output=True, timeout=12, cwd=ROOT)
        return dict(command=command, exit_code=p.returncode, stdout=p.stdout.decode('utf8', 'replace'),
                    stderr=p.stderr.decode('utf8', 'replace'), elapsed_ms=round((time.monotonic()-start)*1000, 2))
    except subprocess.TimeoutExpired:
        return dict(command=command, timeout=True)


def record(name, calls, **observed):
    results.append(dict(case=name, calls=calls, observed=observed))
    print(json.dumps(dict(case=name, **observed), ensure_ascii=True), flush=True)


def archive(path, entries):
    if path.suffix == '.zip':
        with zipfile.ZipFile(path, 'w', compression=zipfile.ZIP_STORED) as z:
            for name, data in entries:
                z.writestr(name, data)
    else:
        with tarfile.open(path, 'w') as t:
            for name, data in entries:
                h = tarfile.TarInfo(name)
                h.size = len(data)
                t.addfile(h, io.BytesIO(data))


def fixture_dir(name):
    d = ROOT / name
    d.mkdir()
    return d


def encrypted_zip(path):
    # Test-only ZipCrypto password; validated independently by Python's ZIP reader.
    password = b'audit-only'
    data = b'ENCRYPTED TEST CONTENT'
    crc = zlib.crc32(data)
    keys = [0x12345678, 0x23456789, 0x34567890]

    def crc_byte(c, b):
        c ^= b
        for _ in range(8):
            c = (c >> 1) ^ (0xedb88320 if c & 1 else 0)
        return c

    def update(b):
        keys[0] = crc_byte(keys[0], b)
        keys[1] = ((keys[1] + (keys[0] & 255)) * 134775813 + 1) & 0xffffffff
        keys[2] = crc_byte(keys[2], keys[1] >> 24)

    for b in password:
        update(b)
    encrypted = bytearray()
    for b in bytes(range(11)) + bytes([crc >> 24]) + data:
        temp = keys[2] | 2
        encrypted.append(b ^ ((temp * (temp ^ 1)) >> 8 & 255))
        update(b)
    name = b'secret.txt'
    local = struct.pack('<IHHHHHIIIHH', 0x04034b50, 20, 1, 0, 0, 33, crc, len(encrypted), len(data), len(name), 0)
    central = struct.pack('<IHHHHHHIIIHHHHHII', 0x02014b50, 20, 20, 1, 0, 0, 33, crc,
                          len(encrypted), len(data), len(name), 0, 0, 0, 0, 0, 0) + name
    body = local + name + encrypted
    path.write_bytes(body + central + struct.pack('<IHHHHIIH', 0x06054b50, 0, 0, 1, 1, len(central), len(body), 0))
    with zipfile.ZipFile(path) as z:
        assert z.read('secret.txt', pwd=password) == data


# Existing reparse points and hardlinks: the outside directory is a synthetic sibling of output.
for ext in ('zip', 'tar'):
    for command, flags in [('smart-unpack', []), ('unpack', ['--to-folder'])]:
        d = fixture_dir(f'archive-stem-{ext}-{command}')
        out = d / 'out'; out.mkdir()
        victim = d / 'victim.txt'; victim.write_bytes(b'ORIGINAL')
        a = d / f'...{ext}'
        archive(a, [('victim.txt', b'ATTACK')])
        call = invoke(command, a, '-o', out, *flags)
        record(d.name, [call], archive_name=a.name, outside_content=victim.read_text(), outside_modified=victim.read_bytes()!=b'ORIGINAL')

for ext in ('zip', 'tar'):
    for nested in (False, True):
        d = fixture_dir(f'junction-{ext}-nested-{nested}')
        out, outside = d / 'out', d / 'outside'
        out.mkdir(); outside.mkdir()
        (outside / 'sub').mkdir()
        junction = out / 'redirect'
        setup = subprocess.run(['powershell', '-NoProfile', '-Command',
                                f"New-Item -ItemType Junction -Path '{junction}' -Target '{outside}' | Out-Null"],
                               capture_output=True, timeout=12)
        if setup.returncode:
            record(d.name, [], setup_error=setup.stderr.decode('utf8', 'replace'))
            continue
        member = 'redirect/sub/victim.txt' if nested else 'redirect/victim.txt'
        victim = outside / ('sub/victim.txt' if nested else 'victim.txt')
        victim.write_bytes(b'ORIGINAL')
        a = d / f'attack.{ext}'
        archive(a, [(member, b'ATTACK')])
        call = invoke('unpack', a, '-o', out)
        record(d.name, [call], outside_content=victim.read_text(), outside_modified=victim.read_bytes()!=b'ORIGINAL')
    d = fixture_dir(f'hardlink-{ext}')
    out = d / 'out'; out.mkdir()
    victim = d / 'outside.txt'; victim.write_bytes(b'ORIGINAL')
    os.link(victim, out / 'victim.txt')
    a = d / f'attack.{ext}'; archive(a, [('victim.txt', b'ATTACK')])
    record(d.name, [invoke('unpack', a, '-o', out)], outside_content=victim.read_text())

for ext in ('zip', 'tar'):
    d = fixture_dir(f'leaf-symlink-{ext}')
    out = d / 'out'; out.mkdir()
    victim = d / 'outside.txt'; victim.write_bytes(b'ORIGINAL')
    try:
        os.symlink(victim, out / 'victim.txt')
    except OSError as e:
        record(d.name, [], setup_error=str(e))
    else:
        a = d / f'attack.{ext}'; archive(a, [('victim.txt', b'ATTACK')])
        record(d.name, [invoke('unpack', a, '-o', out)], outside_content=victim.read_text())

# Bad CRC destroys a preexisting destination; late validation leaves preceding mutations committed.
d = fixture_dir('crc-existing-file')
out = d / 'out'; out.mkdir()
victim = out / 'victim.txt'; victim.write_bytes(b'ORIGINAL IMPORTANT CONTENT')
a = d / 'attack.zip'; archive(a, [('victim.txt', b'CORRUPT-PAYLOAD')])
raw = bytearray(a.read_bytes()); offset = raw.find(b'CORRUPT-PAYLOAD'); assert offset >= 0
raw[offset] ^= 1; a.write_bytes(raw)
call = invoke('unpack', a, '-o', out)
record(d.name, [call], original_bytes=26, destination_exists=victim.exists(), destination_bytes=victim.stat().st_size if victim.exists() else None)

for ext in ('zip', 'tar'):
    d = fixture_dir(f'late-traversal-{ext}')
    out = d / 'out'; out.mkdir()
    victim = out / 'victim.txt'; victim.write_bytes(b'ORIGINAL')
    a = d / f'attack.{ext}'
    archive(a, [('victim.txt', b'ATTACK'), ('new.txt', b'PARTIAL'), ('../escape.txt', b'ESCAPE')])
    call = invoke('unpack', a, '-o', out)
    record(d.name, [call], existing_content=victim.read_text(), partial_file_exists=(out/'new.txt').exists(), traversal_escaped=(d/'escape.txt').exists())

# Filename normalization collisions make extraction and cat disagree on the same logical file.
for ext in ('zip', 'tar'):
    for label, names in [('invalid-char', ['a?.txt', 'a*.txt']), ('hangul-nfc', ['\u1100\u1161.txt', '\uac00.txt'])]:
        d = fixture_dir(f'collision-{ext}-{label}')
        a = d / f'attack.{ext}'
        archive(a, [(names[0], b'FIRST'), (names[1], b'SECOND')])
        out = d / 'out'
        call = invoke('unpack', a, '-o', out)
        files = [p for p in out.rglob('*') if p.is_file()] if out.exists() else []
        record(d.name, [call, invoke('cat', a, names[0])], input_files=2, output_files=len(files), output_contents=[p.read_text() for p in files])

# Existing backup loss on packing failure (two source files with the same ZIP name).
d = fixture_dir('pack-failure-destroys-existing')
for name in ('first', 'second'):
    (d/name).mkdir(); (d/name/'same.txt').write_bytes(name.encode())
dest = d / 'backup.zip'; dest.write_bytes(b'ORIGINAL BACKUP')
call = invoke('pack', dest, d/'first'/'same.txt', d/'second'/'same.txt')
record(d.name, [call], backup_exists=dest.exists(), backup_bytes=dest.stat().st_size if dest.exists() else None)

# Standard gzip, truncated TAR, and corrupt gzip trailer integrity checks.
d = fixture_dir('plain-gzip')
a = d / 'report.txt.gz'; a.write_bytes(gzip.compress(b'VALID PLAIN GZIP'))
record(d.name, [invoke('unpack', a, '-o', d/'out'), invoke('peek', a)], reference_bytes=len(gzip.decompress(a.read_bytes())))
d = fixture_dir('truncated-tar')
h = tarfile.TarInfo('victim.txt'); h.size = 100
a = d/'attack.tar'; a.write_bytes(h.tobuf() + b'TINY')
out = d/'out'
call = invoke('unpack', a, '-o', out)
record(d.name, [call], declared_bytes=100, written_bytes=(out/'victim.txt').stat().st_size if (out/'victim.txt').exists() else None)
d = fixture_dir('gzip-bad-trailer')
tar_bytes = io.BytesIO()
with tarfile.open(fileobj=tar_bytes, mode='w') as t:
    h = tarfile.TarInfo('victim.txt'); h.size=5; t.addfile(h, io.BytesIO(b'VALID'))
raw=bytearray(gzip.compress(tar_bytes.getvalue())); raw[-8] ^= 0xff
a = d/'attack.tar.gz'; a.write_bytes(raw)
try:
    gzip.decompress(raw); reference = 'accepted'
except Exception as e:
    reference = type(e).__name__ + ': ' + str(e)
record(d.name, [invoke('unpack', a, '-o', d/'out'), invoke('peek', a)], reference_gzip_validation=reference)

d = fixture_dir('tar-extreme-mtime')
a = d/'attack.tar'
with tarfile.open(a, 'w', format=tarfile.GNU_FORMAT) as t:
    h=tarfile.TarInfo('victim.txt'); h.size=1; h.mtime=2**64-1
    t.addfile(h, io.BytesIO(b'X'))
record(d.name, [invoke('unpack', a, '-o', d/'out')], header_mtime=str(2**64-1))

d = fixture_dir('zip-deceptive-size-control')
a=d/'attack.zip'; archive(a,[('victim.txt',b'X'*8192)])
raw=bytearray(a.read_bytes())
struct.pack_into('<I',raw,22,1)
central=raw.index(b'PK\x01\x02'); struct.pack_into('<I',raw,central+24,1)
a.write_bytes(raw)
record(d.name,[invoke('unpack',a,'-o',d/'out')], declared_bytes=1, actual_bytes=8192, destination_exists=(d/'out'/'victim.txt').exists())

# Supplied password must work in every receiving operation, not just direct unpack.
d = fixture_dir('encrypted-zip')
a=d/'encrypted.zip'; encrypted_zip(a)
record(d.name, [invoke('unpack', a, '-o', d/'direct', '-p', 'audit-only'),
                invoke('smart-unpack', a, '-o', d/'smart', '-p', 'audit-only'),
                invoke('cat', a, 'secret.txt', '-p', 'audit-only')], reference_python_decrypt=True)

# Entry count limit does not bound extension map size or total JSON output.
d=fixture_dir('peek-output-budget')
a=d/'many.zip'; archive(a, [(f'f{i}.ext{i}', b'x') for i in range(1000)])
call=invoke('peek', a, '--limit', 1)
summary=json.loads(call['stdout'])
record(d.name, [call], limit=1, tree_items=len(summary['tree_summary']), extension_keys=len(summary['extension_distribution']), stdout_bytes=len(call['stdout'].encode()))

# Positive controls for ordinary archives and blocked traversal / ADS / reserved device names.
for name in ('../escape.txt', 'sub/../../escape.txt', 'file.txt:stream', 'CON.txt'):
    d=fixture_dir('guard-'+uuid.uuid4().hex[:6]); a=d/'attack.zip'; archive(a, [(name,b'ATTACK')])
    call=invoke('unpack', a, '-o', d/'out')
    record('guard-control:'+name, [call], rejected=call.get('exit_code')==1, outside_created=(d/'escape.txt').exists())
for ext in ('zip','tar'):
    d=fixture_dir('roundtrip-'+ext); a=d/f'valid.{ext}'; archive(a,[('root/test.txt',b'VALID')])
    call=invoke('smart-unpack',a,'-o',d/'out')
    record(d.name,[call], output_matches=(d/'out'/'root'/'test.txt').read_bytes()==b'VALID')

report=dict(binary=str(BINARY), binary_sha256=hashlib.sha256(BINARY.read_bytes()).hexdigest(),
            source_commit=source_commit(),
            run_dir=str(ROOT), cases=results)
(ROOT/'results.json').write_text(json.dumps(report,ensure_ascii=False,indent=2),encoding='utf8')
print('RESULTS='+str(ROOT/'results.json'))
