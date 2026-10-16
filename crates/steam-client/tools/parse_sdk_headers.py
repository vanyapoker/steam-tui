# Step 1 of 2: parse Valve SDK headers (sparse checkout of ValveSoftware/Proton lsteamclient/steamworks_sdk_*).
# Edit root below, run this, then tools/gen_compat.py, then cargo fmt.
import re,glob,json,collections
root='/path/to/Proton/lsteamclient'
res=collections.defaultdict(dict)
def split_args(a):
    a=re.sub(r'STEAM_\w+\s*\((?:[^()]|\([^()]*\))*\)','',a)
    out=[];depth=0;cur=''
    for ch in a:
        if ch in '(<': depth+=1
        if ch in ')>': depth-=1
        if ch==',' and depth==0: out.append(cur);cur=''
        else: cur+=ch
    if cur.strip(): out.append(cur)
    return [x.strip() for x in out if x.strip() and x.strip()!='void']
BUILTIN=set('bool char int short long unsigned signed int8 uint8 int16 uint16 int32 uint32 int64 uint64 size_t uint8_t uint16_t uint32_t uint64_t int8_t int16_t int32_t int64_t'.split())
def cls(t):
    t=re.sub(r'\b(const|struct|class|virtual|inline)\b','',t); t=re.sub(r'STEAM_\w+\s*\([^)]*\)','',t); t=' '.join(t.split())
    if '*' in t or '&' in t: return 'P'
    if t=='void': return 'V'
    if t=='float': return 'F'
    if t=='double': return 'D'
    words=t.split()
    if all(w in BUILTIN for w in words): return 'I'
    if re.match(r'^(E[A-Z]\w*|H[A-Z]\w*|\w+_t|CSteamID|CGameID|RTime32|ScreenshotHandle|SteamLeaderboard\w*|\w+Handle\w*)$',t): return 'I'
    return 'S:'+t
def ptype(a):
    a=re.sub(r'=.*$','',a).strip()
    a=re.sub(r'\[[^\]]*\]\s*$','',a).strip()
    a=re.sub(r'STEAM_\w+\s*\([^)]*\)','',a)
    if '*' in a or '&' in a:
        return a[:max(a.rfind('*'),a.rfind('&'))+1]
    w=a.split()
    return ' '.join(w[:-1]) if len(w)>1 and not re.match(r'^(unsigned|signed|const)$',w[-1]) else a
for dd in sorted(glob.glob(root+'/steamworks_sdk_*')):
    sdk=dd.split('_')[-1]
    for h in glob.glob(dd+'/isteam*.h'):
        t=open(h,errors='ignore').read()
        t=re.sub(r'//.*','',t); t=re.sub(r'/\*.*?\*/','',t,flags=re.S)
        vers=re.findall(r'#define\s+(\w+_INTERFACE_VERSION\w*)\s+"([^"]+)"',t)
        for m in re.finditer(r'class\s+ISteam(\w+)\s*\{(.*?)\n\};',t,flags=re.S):
            name,body=m.groups()
            ms=[]
            if re.search(r'virtual\s+~ISteam\w+\s*\(\s*\)',body): ms+=[('V',[ ],'~dtor0'),('V',[],'~dtor1')]
            body=re.sub(r'virtual\s+~ISteam\w+\s*\(\s*\)\s*\{\s*\}','',body)
            for mm in re.finditer(r'virtual\s+([^;(]*?)\b(\w+)\s*\(([^;]*?)\)\s*(?:const\s*)?=\s*0\s*;',body,flags=re.S):
                ret,n,args=mm.groups()
                ms.append((cls(ret),[cls(ptype(a)) for a in split_args(args)],n))
            v=[s for k,s in vers if name.upper().replace('_','') in k.replace('_','')]
            for s in (v or [None]):
                res[f'{name}|{s}'][sdk]=ms
json.dump(res,open('/tmp/steam-ifaces.json','w'))
print(len(res))
