# Step 2 of 2: writes src/compat.rs from the parsed headers.
import json,re,collections,sys
d=json.load(open('/tmp/steam-ifaces.json'))
src=open('/home/vp/Projects/steam-tui/crates/steam-client/src/client.rs').read()
served=set(re.findall(r'c"([A-Za-z_0-9]+)"',src[src.index('static REGISTRY'):src.index('fn entry(')]))
def last(key):
    sd=sorted(d[key]); return max((d[key][x] for x in sd),key=len)
def ok(base,tgt):
    br,bp,_=base; tr,tp,_=tgt
    if br!=tr and not (br in 'VI' and tr in 'VI'): return False
    if len(bp)>len(tp): return False
    return all(bp[i]==tp[i] for i in range(len(bp)))
def remap(base_key,tgt_key):
    base=last(base_key); tgt=last(tgt_key)
    occ=collections.defaultdict(list)
    for i,m in enumerate(base): occ[m[2]].append(i)
    seen=collections.Counter(); out=[]; bad=[]
    for m in tgt:
        k=seen[m[2]]; seen[m[2]]+=1
        if k<len(occ[m[2]]):
            bi=occ[m[2]][k]
            if ok(base[bi],m): out.append(bi); continue
            bad.append(m[2])
        out.append(-1)
    return out,bad
fams=[
('User','user::TABLE',32,'User|SteamUser021','User',True,lambda v:v.startswith('SteamUser')),
('Utils','utils::TABLE',39,'Utils|SteamUtils010','Utils',False,lambda v:v.startswith('SteamUtils')),
('Apps','apps::TABLE',33,'Apps|STEAMAPPS_INTERFACE_VERSION008','Apps',True,lambda v:v.startswith('STEAMAPPS_INTERFACE_VERSION') or v=='SteamApps001'),
('Friends','friends::TABLE',80,'Friends|SteamFriends017','Friends',True,lambda v:v.startswith('SteamFriends')),
('Matchmaking','matchmaking::TABLE',39,'Matchmaking|SteamMatchMaking009','Matchmaking',True,lambda v:v.startswith('SteamMatchMaking0') and 'Servers' not in v),
('MatchmakingServers','matchmaking::SERVERS_TABLE',17,'MatchmakingServers|SteamMatchMakingServers002','Servers',True,lambda v:'MatchMakingServers' in v),
('UserStats','userstats::TABLE',45,'UserStats|STEAMUSERSTATS_INTERFACE_VERSION012','UserStats',True,lambda v:v.startswith('STEAMUSERSTATS')),
('Networking','networking::NET_TABLE',22,'Networking|SteamNetworking006','Networking',True,lambda v:v.startswith('SteamNetworking0') and v[-3:] in ('001','002','003','004','005','006')and 'Sockets' not in v and 'Utils' not in v and 'Messages' not in v),
('RemoteStorage','remotestorage::TABLE',59,'RemoteStorage|STEAMREMOTESTORAGE_INTERFACE_VERSION016','RemoteStorage',True,lambda v:v.startswith('STEAMREMOTESTORAGE')),
('Screenshots','screenshots::TABLE',9,'Screenshots|STEAMSCREENSHOTS_INTERFACE_VERSION003','Screenshots',True,lambda v:v.startswith('STEAMSCREENSHOTS')),
('HTTP','http::TABLE',25,'HTTP|STEAMHTTP_INTERFACE_VERSION003','Http',True,lambda v:v.startswith('STEAMHTTP')),
('HTMLSurface','htmlsurface::TABLE',39,'HTMLSurface|STEAMHTMLSURFACE_INTERFACE_VERSION_005','Html',True,lambda v:v.startswith('STEAMHTMLSURFACE')),
('Controller','controller::TABLE',34,'Controller|SteamController008','Controller',True,lambda v:v.startswith('SteamController0')),
('Inventory','inventory::TABLE',38,'Inventory|STEAMINVENTORY_INTERFACE_V003','Inventory',True,lambda v:v.startswith('STEAMINVENTORY')),
('UGC','ugc::TABLE',89,'UGC|STEAMUGC_INTERFACE_VERSION017','Ugc',True,lambda v:v.startswith('STEAMUGC')),
('Input','input::TABLE',48,'Input|SteamInput006','Input',True,lambda v:v.startswith('SteamInput0')),
]
tables=[];entries=[];report=[]
def tag(v): return re.sub(r'[^A-Z0-9]','_',v.upper())
maxn=0
for cls,tab,n,bkey,fam,sess,flt in fams:
    assert len(last(bkey))==n,(bkey,len(last(bkey)),n)
    for key in sorted(d):
        c,v=key.split('|')
        if c!=cls or v=='None' or not flt(v) or v in served: continue
        m,bad=remap(bkey,key); maxn=max(maxn,len(m))
        t=tag(v)
        tables.append(f"static T_{t}: VTable<{len(m)}> = remap(&{tab}.0, [{', '.join(map(str,m))}]);\nstatic O_{t}: Object = Object {{ table: T_{t}.0.as_ptr() }};\n")
        entries.append(f'    (c"{v}", Family::{fam}, {str(sess).lower()}, &O_{t}),\n')
        report.append((v,len(m),sum(1 for x in m if x<0),bad))
# client
cb='Client|SteamClient021'
assert len(last(cb))==40
clients=[]
for key in sorted(d):
    c,v=key.split('|')
    if c!='Client' or v=='None' or v in ('SteamClient017','SteamClient020','SteamClient021','SteamClient023'): continue
    m,bad=remap(cb,key); maxn=max(maxn,len(m)); t=tag(v)
    tables.append(f"static T_{t}: VTable<{len(m)}> = remap(&client::TABLE021.0, [{', '.join(map(str,m))}]);\nstatic O_{t}: Object = Object {{ table: T_{t}.0.as_ptr() }};\n")
    clients.append(f'    (c"{v}", &O_{t}),\n'); report.append((v,len(m),sum(1 for x in m if x<0),bad))
head='''//! Older Steamworks interface versions (SDK 0.99 to 1.49 layouts), generated from
//! Valve's public SDK headers (Proton's lsteamclient/steamworks_sdk_*). Each table
//! reuses the current implementation for a slot only when the method name and ABI
//! (return class, argument classes, no missing trailing arguments) match; every
//! other slot logs and returns zero. Regenerate rather than edit by hand.
use super::client::Family;
use super::*;

'''
stubs=''.join(f'unsafe extern "C" fn u{i}() -> usize {{\n    trace("interface slot {i} is unavailable");\n    0\n}}\n' for i in range(maxn))
unav=f"static UNAVAILABLE: VTable<{maxn}> = VTable([\n"+''.join(f"    u{i} as *const (),\n" for i in range(maxn))+"]);\n"
rm='''const fn remap<const N: usize, const M: usize>(base: &[*const (); N], map: [i16; M]) -> VTable<M> {
    let mut slots = [ptr::null::<()>(); M];
    let mut i = 0;
    while i < M {
        slots[i] = if map[i] < 0 { UNAVAILABLE.0[i] } else { base[map[i] as usize] };
        i += 1;
    }
    VTable(slots)
}
'''
out=head+stubs+unav+rm+'\n'+''.join(tables)
out+="\npub(super) static COMPAT: &[(&CStr, Family, bool, &Object)] = &[\n"+''.join(entries)+"];\n"
out+="pub(super) static CLIENTS: &[(&CStr, &Object)] = &[\n"+''.join(clients)+"];\n"
open('/home/vp/Projects/steam-tui/crates/steam-client/src/compat.rs','w').write(out)
print(len(report),'versions; max slots',maxn)
tot=sum(r[1] for r in report); un=sum(r[2] for r in report); print('slots',tot,'unavailable',un)
for r in report:
    if r[3]: print(r[0],r[1],'unavail',r[2],'abi-mismatch',r[3][:6])
