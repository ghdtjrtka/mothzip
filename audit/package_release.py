"""Package the exact validated binary and source, refusing stale or failing gate evidence."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import time
import tomllib
import zipfile

WORKSPACE=Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser()
parser.add_argument('--validation',required=True)
parser.add_argument('--dependency-audit',required=True)
args=parser.parse_args()
gate=json.loads(Path(args.validation).read_text(encoding='utf-8-sig'))
dependencies=json.loads(Path(args.dependency_audit).read_text(encoding='utf-8-sig'))
binary=Path(gate['binary'])
def sha(path): return hashlib.sha256(path.read_bytes()).hexdigest()
assert gate['passed'] and not gate['failures'],'Release gate has failures'
assert sha(binary)==gate['binary_sha256'],'Binary changed after validation'
for name,digest in gate['source_files_sha256'].items():
    assert sha(WORKSPACE/name)==digest,'Source changed after validation: '+name
assert dependencies['vulnerabilities']['count']==0,'Known vulnerable dependency'
assert not dependencies['warnings'],'Dependency audit warnings require review'
version=tomllib.loads((WORKSPACE/'Cargo.toml').read_text(encoding='utf8'))['package']['version']
assert subprocess.check_output([str(binary),'--version'],text=True).strip()=='mothzip '+version
root=WORKSPACE/'target'/'release-packages'/('mothzip-'+version+'-'+time.strftime('%Y%m%d-%H%M%S'))
root.mkdir(parents=True)
payload=root/'windows-x64';payload.mkdir()
for src,dest in [(binary,'mothzip.exe'),(WORKSPACE/'README.md','README.md'),(WORKSPACE/'README.ko.md','README.ko.md'),(WORKSPACE/'packaging/Install.cmd','Install.cmd'),(WORKSPACE/'LICENSE','LICENSE')]:
    shutil.copyfile(src,payload/dest)

# Distribute upstream license/notice text from the exact locked Cargo package sources.
metadata=json.loads(subprocess.check_output(['cargo','metadata','--locked','--format-version','1',
    '--filter-platform','x86_64-pc-windows-msvc'],cwd=WORKSPACE))
nodes={node['id']:node for node in metadata['resolve']['nodes']}
active=set(); pending=[metadata['resolve']['root']]
while pending:
    package_id=pending.pop()
    if package_id in active: continue
    active.add(package_id)
    pending.extend(nodes[package_id]['dependencies'])
packages=[package for package in metadata['packages'] if package['id'] in active]
def rust_version_key(value): return tuple(int(x) for x in (value.split('.')+['0','0'])[:3])
declared_rust=tomllib.loads((WORKSPACE/'Cargo.toml').read_text(encoding='utf8'))['package']['rust-version']
assert all(not p.get('rust_version') or rust_version_key(p['rust_version'])<=rust_version_key(declared_rust)
    for p in packages),'Declared Rust version is below an active dependency requirement'
notices=[];missing=[]
for package in sorted(packages,key=lambda p:(p['name'],p['version'])):
    if Path(package['manifest_path']).parent==WORKSPACE: continue
    directory=Path(package['manifest_path']).parent
    license_files=set()
    for pattern in ('LICENSE*','COPYING*','NOTICE*','UNLICENSE*'):
        license_files.update(p for p in directory.glob(pattern) if p.is_file())
    if package.get('license_file'):
        license_files.add(directory/package['license_file'])
    if not license_files:
        # Some crates declare an SPDX license but ship no text in the published archive.
        # Record the declared identifier instead of reconstructing a text the package did not carry.
        assert package.get('license'),'No license text and no declared license: '+package['name']
        missing.append(package['name']+' '+package['version']+' ('+package['license']+')');continue
    notices.append('\n'+'='*72+'\n'+package['name']+' '+package['version']+' | '+str(package.get('license'))+'\n')
    for path in sorted(license_files):
        notices.append('\n--- '+path.name+' ---\n'+path.read_text(encoding='utf8',errors='replace'))
if missing:
    notices.insert(0,'The following packages declare an SPDX license but ship no license text in\ntheir crate archive; the declared identifier is recorded instead:\n'
        +'\n'.join('  - '+entry for entry in missing)+'\n')
# Rust standard library license texts shipped by the installed compiler.
sysroot=Path(subprocess.check_output(['rustc','--print','sysroot'],text=True).strip())
rust_notices=list((sysroot/'share/doc/rust/licenses').glob('*'))
for path in sorted(p for p in rust_notices if p.is_file()):
    notices.append('\n--- Rust standard library: '+path.name+' ---\n'+path.read_text(encoding='utf8',errors='replace'))
(payload/'THIRD-PARTY-LICENSES.txt').write_text('\n'.join(notices),encoding='utf8')
manifest=dict(version=version,platform='windows-x64',binary_sha256=sha(binary),
    source_files_sha256=gate['source_files_sha256'],
    validation=dict(cases=gate['case_count'],calls=gate['call_count'],passed=True),
    dependency_audit=dict(vulnerabilities=0,warnings=0,database=dependencies.get('database')),
    license_text_unavailable=missing,
    files={p.name:sha(p) for p in payload.iterdir() if p.is_file()})
(payload/'manifest.json').write_text(json.dumps(manifest,ensure_ascii=False,indent=2),encoding='utf8')
binary_zip=root/f'mothzip-{version}-windows-x64.zip'
with zipfile.ZipFile(binary_zip,'w',compression=zipfile.ZIP_DEFLATED) as z:
    for p in sorted(payload.iterdir()): z.write(p,p.name)
source_zip=root/f'mothzip-{version}-source.zip'
sources=[WORKSPACE/n for n in ['Cargo.toml','Cargo.lock','build.rs','LICENSE','README.md','README.ko.md','.cargo/config.toml','.github/repository-metadata.json']]
sources+=list((WORKSPACE/'src').rglob('*.rs'))
sources+=list((WORKSPACE/'assets').iterdir())
sources+=list((WORKSPACE/'audit').glob('*.py'))
sources+=list((WORKSPACE/'docs').glob('*.md'))
sources+=list((WORKSPACE/'packaging').iterdir())
with zipfile.ZipFile(source_zip,'w',compression=zipfile.ZIP_DEFLATED) as z:
    for p in sorted(p for p in sources if p.is_file()): z.write(p,p.relative_to(WORKSPACE))

# Independent ZIP reader and the shipped executable both round-trip the binary package.
with zipfile.ZipFile(binary_zip) as z:
    assert z.testzip() is None
    assert hashlib.sha256(z.read('mothzip.exe')).hexdigest()==sha(binary)
smoke=root/'smoke';smoke.mkdir()
subprocess.run([str(binary),'unpack',str(binary_zip),'-o',str(smoke),'--json'],capture_output=True,check=True)
assert sha(smoke/'mothzip.exe')==sha(binary)
assert subprocess.check_output([str(smoke/'mothzip.exe'),'--version'],text=True).strip()=='mothzip '+version
checksums={p.name:sha(p) for p in (binary_zip,source_zip)}
(root/'SHA256SUMS.txt').write_text(''.join(digest+'  '+name+'\n' for name,digest in checksums.items()),encoding='ascii')
print(json.dumps(dict(output=str(root),binary_bytes=binary.stat().st_size,binary_sha256=sha(binary),archives=checksums,
    dependency_license_packages=len(packages)-1,rust_license_files=len(rust_notices)),indent=2))
