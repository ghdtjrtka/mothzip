"""Verify every built icon against its ICO source and extract it through Windows' icon API."""
from pathlib import Path
import argparse,ctypes,hashlib,json,re,struct,sys,time,uuid
from ctypes import wintypes

parser=argparse.ArgumentParser();parser.add_argument('--binary',required=True);args=parser.parse_args()
workspace=Path(__file__).resolve().parents[1];binary=Path(args.binary).resolve()
catalog=(workspace/'src/core/file_types.rs').read_text(encoding='utf8')
extensions=re.findall(r'"(\.[^"]+)"',re.search(r'SUPPORTED_EXTENSIONS:[\s\S]*?=\s*&\[([\s\S]*?)\];',catalog).group(1))
base_id=int(re.search(r'GENERIC_ARCHIVE_ICON_ID: i32 = (\d+)',catalog).group(1))
kernel=ctypes.WinDLL('kernel32',use_last_error=True);user=ctypes.WinDLL('user32',use_last_error=True)
kernel.LoadLibraryExW.argtypes=[wintypes.LPCWSTR,wintypes.HANDLE,wintypes.DWORD];kernel.LoadLibraryExW.restype=wintypes.HMODULE
kernel.FindResourceW.argtypes=[wintypes.HMODULE,ctypes.c_void_p,ctypes.c_void_p];kernel.FindResourceW.restype=wintypes.HRSRC
kernel.LoadResource.argtypes=[wintypes.HMODULE,wintypes.HRSRC];kernel.LoadResource.restype=wintypes.HGLOBAL
kernel.LockResource.argtypes=[wintypes.HGLOBAL];kernel.LockResource.restype=ctypes.c_void_p
kernel.SizeofResource.argtypes=[wintypes.HMODULE,wintypes.HRSRC];kernel.SizeofResource.restype=wintypes.DWORD
kernel.FreeLibrary.argtypes=[wintypes.HMODULE]
user.PrivateExtractIconsW.argtypes=[wintypes.LPCWSTR,ctypes.c_int,ctypes.c_int,ctypes.c_int,ctypes.POINTER(wintypes.HICON),ctypes.POINTER(wintypes.UINT),wintypes.UINT,wintypes.UINT]
user.PrivateExtractIconsW.restype=wintypes.UINT
user.DestroyIcon.argtypes=[wintypes.HICON]
module=kernel.LoadLibraryExW(str(binary),None,0x2|0x20)
if not module:raise ctypes.WinError(ctypes.get_last_error())
def resource(kind,identity):
    handle=kernel.FindResourceW(module,ctypes.c_void_p(identity),ctypes.c_void_p(kind))
    if not handle:raise ctypes.WinError(ctypes.get_last_error())
    memory=kernel.LoadResource(module,handle);pointer=kernel.LockResource(memory)
    return ctypes.string_at(pointer,kernel.SizeofResource(module,handle))
results=[]
try:
    icons=[('app',1,workspace/'assets/moth.ico')]
    icons.extend((extension,base_id+index,workspace/'assets/file-icons'/f'{extension.removeprefix(".")}.ico')
                 for index,extension in enumerate(['archive',*extensions]))
    for extension,identity,path in icons:
        source=path.read_bytes()
        group=resource(14,identity)
        _,kind,count=struct.unpack_from('<HHH',group);assert kind==1
        source_count=struct.unpack_from('<H',source,4)[0];assert count==source_count
        sizes=[]
        for frame in range(count):
            width,height,colors,reserved,planes,bits,length,icon_id=struct.unpack_from('<BBBBHHIH',group,6+frame*14)
            sw,sh,sc,sr,sp,sb,sl,offset=struct.unpack_from('<BBBBHHII',source,6+frame*16)
            assert (width,height,colors,reserved,planes,bits,length)==(sw,sh,sc,sr,sp,sb,sl)
            assert resource(3,icon_id)==source[offset:offset+length]
            sizes.append(width or 256)
        assert {16,32,256}.issubset(sizes)
        for size in (16,32,256):
            icon=wintypes.HICON();returned_id=wintypes.UINT()
            extracted=user.PrivateExtractIconsW(str(binary),-identity,size,size,ctypes.byref(icon),ctypes.byref(returned_id),1,0)
            assert extracted==1 and icon.value,(extension,size,'Windows could not extract the icon')
            user.DestroyIcon(icon)
        results.append(dict(extension=extension,resource_id=identity,frames=count,sizes=sizes,passed=True))
finally:kernel.FreeLibrary(module)
root=workspace/'audit/runs'/('file-icons-'+time.strftime('%Y%m%d-%H%M%S')+'-'+uuid.uuid4().hex[:6]);root.mkdir(parents=True)
report=dict(passed=True,binary=str(binary),binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),icons=results,windows_extractions=len(results)*3)
(root/'results.json').write_text(json.dumps(report,indent=2),encoding='utf8')
print(json.dumps(dict(passed=True,icons=len(results),frames=sum(r['frames'] for r in results),windows_extractions=report['windows_extractions'],report=str(root/'results.json'))))
