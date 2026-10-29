#!/usr/bin/env python3
from pathlib import Path
import ctypes,json,collections,subprocess,time,argparse
p=argparse.ArgumentParser(description='Group a private structural screening archive by representation; publish aggregates only.')
p.add_argument('--archive',type=Path,required=True)
p.add_argument('--private-output',type=Path,required=True)
p.add_argument('--report',type=Path,required=True)
a=p.parse_args()
root=a.archive;out=a.private_output.resolve()
if out.is_relative_to(Path(__file__).resolve().parents[1]):p.error('private output must be outside repository')
out.mkdir(mode=0o700,parents=True,exist_ok=False)
z=ctypes.CDLL('libzstd.so.1');z.ZSTD_getFrameContentSize.argtypes=[ctypes.c_void_p,ctypes.c_size_t];z.ZSTD_getFrameContentSize.restype=ctypes.c_ulonglong;z.ZSTD_decompress.argtypes=[ctypes.c_void_p,ctypes.c_size_t,ctypes.c_void_p,ctypes.c_size_t];z.ZSTD_decompress.restype=ctypes.c_size_t

def dec(data):
 n=z.ZSTD_getFrameContentSize(data,len(data));assert n<256*1024*1024;buf=ctypes.create_string_buffer(n);assert z.ZSTD_decompress(buf,n,data,len(data))==n;return buf.raw
chunks={};packs=collections.defaultdict(list)
for p in sorted((root/'manifests').iterdir(),key=lambda p:int(p.name.split('.')[0])):
 m=json.loads(subprocess.check_output(["zstd","-qdc",str(p)]))
 for idx,offset,n,flags in m['recipe']['operations']:
  c=m['chunks'][idx];key=(c['pack'],c['offset'],c['len'])
  if key not in chunks:
   kind='base64url' if flags&16 else 'base64' if flags&8 else 'text' if flags&7 else 'literal';chunks[key]=kind;packs[c['pack']].append((c['offset'],c['len'],kind))
streams={k:(out/(k+'.bin')).open('xb') for k in set(chunks.values())}
for pack,refs in packs.items():
 raw=dec((root/'packs'/(pack+'.nz')).read_bytes()[20:])
 for offset,n,kind in sorted(refs):streams[kind].write(raw[offset:offset+n])
for f in streams.values():f.close()
report=[]
for p in sorted(out.glob('*.bin')):
 for long in [False,True]:
  t=time.monotonic();dest=p.with_suffix('.long.zst' if long else '.zst');cmd=['zstd','-q','-9','--single-thread'];cmd+=['--long=27'] if long else [];subprocess.run(cmd+[str(p),'-o',str(dest)],check=True)
  row={'group':p.stem,'long':long,'raw_bytes':p.stat().st_size,'compressed_bytes':dest.stat().st_size,'seconds':time.monotonic()-t};report.append(row);print(json.dumps(row),flush=True)
a.report.parent.mkdir(parents=True,exist_ok=True)
a.report.write_text(json.dumps(report,indent=2)+'\n')
