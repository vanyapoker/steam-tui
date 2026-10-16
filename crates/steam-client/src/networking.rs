//! SteamNetworkingUtils004: backend POP measurements and ping locations, native
//! message allocation, timestamps and transport config. Hosted availability stays gated.
//! The legacy SteamNetworking006 transport still reports honest failure.
use super::*;
use std::sync::Mutex;
use std::time::Instant;
mod ping;

/// SteamNetworkingPOPID from a 3-4 letter code such as "ams" or "sto2".
fn pop_id(code: &str) -> Option<u32> {
    let b = code.as_bytes();
    ((3..=4).contains(&b.len()) && b.iter().all(u8::is_ascii_alphanumeric)).then(|| {
        (u32::from(b[0]) << 16)
            | (u32::from(b[1]) << 8)
            | u32::from(b[2])
            | (u32::from(b.get(3).copied().unwrap_or(0)) << 24)
    })
}

/// Data-center pings measured by the account backend, cached once available.
fn pops() -> Vec<(u32, i32)> {
    if let Some(entries) = sockets::relay_pings() {
        return entries;
    }
    use steam_ipc::{Operation, Response};
    static CACHE: Mutex<Option<(Instant, Vec<(u32, i32)>)>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, entries)) = &*cache {
        return entries.clone();
    }
    if let Ok(Response::PopPings {
        ready: true,
        entries,
    }) = bridge::request(current_app_id(), Operation::PopPings)
    {
        let entries: Vec<_> = entries
            .iter()
            .filter_map(|(code, ms)| Some((pop_id(code)?, *ms)))
            .collect();
        trace(format_args!(
            "sdr: {} data-center pings from the backend",
            entries.len()
        ));
        *cache = Some((Instant::now(), entries.clone()));
        return entries;
    }
    Vec::new()
}

macro_rules! stubs {
    ($table:ident, $object:ident, [$($name:ident => $label:literal;)*]) => {
        $(unsafe extern "C" fn $name() -> usize {
            trace(concat!("networking.", $label, " (no networking support)"));
            0
        })*
        static $table: VTable<{ [$($label),*].len() }> = VTable([$($name as *const ()),*]);
        pub(super) static $object: Object = Object { table: $table.0.as_ptr() };
    };
}
stubs!(NET_TABLE, NETWORKING, [
    n0 => "SendP2PPacket";
    n1 => "IsP2PPacketAvailable";
    n2 => "ReadP2PPacket";
    n3 => "AcceptP2PSessionWithUser";
    n4 => "CloseP2PSessionWithUser";
    n5 => "CloseP2PChannelWithUser";
    n6 => "GetP2PSessionState";
    n7 => "AllowP2PPacketRelay";
    n8 => "CreateListenSocket";
    n9 => "CreateP2PConnectionSocket";
    n10 => "CreateConnectionSocket";
    n11 => "DestroySocket";
    n12 => "DestroyListenSocket";
    n13 => "SendDataOnSocket";
    n14 => "IsDataAvailableOnSocket";
    n15 => "RetrieveDataFromSocket";
    n16 => "IsDataAvailable";
    n17 => "RetrieveData";
    n18 => "GetSocketInfo";
    n19 => "GetListenSocketInfo";
    n20 => "GetSocketConnectionType";
    n21 => "GetMaxPacketSize";
]);

macro_rules! unsupported_slots {
    ($($name:ident => $label:literal;)*) => {
        $(unsafe extern "C" fn $name() -> ! { unsupported(concat!("networkingutils.", $label)) })*
    };
}
unsupported_slots! {}
// ---- ping locations ---------------------------------------------------------
// Opaque location blobs contain Valve-compatible markers; only the text form
// is exchanged between clients. Never put invented values into missing routes.
const LOCATION_SIZE: usize = ping::SIZE;
fn local_markers() -> Vec<ping::Marker> {
    pops()
        .iter()
        .filter_map(|(id, direct)| {
            let routed = unsafe { sockets::routed_ping(*id, ptr::null_mut()) };
            let routed = match (*direct >= 0, routed >= 0) {
                (true, true) => (*direct).min(routed),
                (true, false) => *direct,
                (false, true) => routed,
                (false, false) => return None,
            };
            (routed <= 5000).then(|| ping::Marker::measured(*id, *direct, routed))
        })
        .collect()
}

fn pop_code(id: u32) -> String {
    [
        (id >> 16) as u8,
        (id >> 8) as u8,
        id as u8,
        (id >> 24) as u8,
    ]
    .iter()
    .filter(|b| **b != 0)
    .map(|b| *b as char)
    .collect()
}

fn location_at<'a>(pointer: *const u8) -> &'a [u8] {
    // SAFETY: SteamNetworkPingLocation_t is 512 bytes.
    unsafe { std::slice::from_raw_parts(pointer, LOCATION_SIZE) }
}

unsafe extern "C" fn u_loc(_this: *mut c_void, result: *mut u8) -> f32 {
    if result.is_null() {
        return -1.0;
    }
    let markers = local_markers();
    let blob = ping::encode(&markers);
    unsafe { ptr::copy_nonoverlapping(blob.as_ptr(), result, LOCATION_SIZE) };
    trace(format_args!(
        "networkingutils.GetLocalPingLocation markers={}",
        markers.len()
    ));
    if markers.is_empty() {
        -1.0
    } else {
        sockets::ping_age()
    }
}
unsafe extern "C" fn u_est2(_this: *mut c_void, a: *const u8, b: *const u8) -> i32 {
    if a.is_null() || b.is_null() {
        return -2;
    }
    ping::estimate(&ping::decode(location_at(a)), &ping::decode(location_at(b)))
}
unsafe extern "C" fn u_estlocal(_this: *mut c_void, remote: *const u8) -> i32 {
    if remote.is_null() {
        return -2;
    }
    ping::estimate(&local_markers(), &ping::decode(location_at(remote)))
}
unsafe extern "C" fn u_tostr(_this: *mut c_void, location: *const u8, buffer: *mut u8, size: i32) {
    if location.is_null() {
        write_text(buffer, size.max(0) as usize, "");
        return;
    }
    let text = ping::format(&ping::decode(location_at(location)));
    // A truncated marker is not a usable location. Leave the result empty.
    write_text(
        buffer,
        size.max(0) as usize,
        if text.len() < size.max(0) as usize {
            &text
        } else {
            ""
        },
    );
}
unsafe extern "C" fn u_parse(_this: *mut c_void, text: *const c_char, result: *mut u8) -> bool {
    if result.is_null() {
        return false;
    }
    unsafe { ptr::write_bytes(result, 0, LOCATION_SIZE) };
    if text.is_null() {
        return false;
    }
    // Bound scanning to the SDK maximum string size, including the terminator.
    let mut len = 0;
    while len < 1024 && unsafe { *text.add(len) } != 0 {
        len += 1;
    }
    if len == 1024 {
        return false;
    }
    let Ok(text) = std::str::from_utf8(unsafe { std::slice::from_raw_parts(text.cast(), len) })
    else {
        return false;
    };
    let Some(blob) = ping::parse(text) else {
        trace("networkingutils.ParsePingLocationString = false (invalid marker)");
        return false;
    };
    unsafe { ptr::copy_nonoverlapping(blob.as_ptr(), result, LOCATION_SIZE) };
    true
}

// ---- address / identity text forms -------------------------------------------
// SteamNetworkingIPAddr: 16 bytes (IPv6, IPv4 as ::ffff:a.b.c.d) + u16 port = 18 bytes.
// SteamNetworkingIdentity: i32 type, i32 size, 128-byte union (136 bytes).

fn addr_from(bytes: &[u8]) -> (std::net::Ipv6Addr, u16) {
    let mut octets = [0u8; 16];
    octets.copy_from_slice(&bytes[..16]);
    (
        std::net::Ipv6Addr::from(octets),
        u16::from_le_bytes([bytes[16], bytes[17]]),
    )
}

fn addr_text(bytes: &[u8], with_port: bool) -> String {
    let (ip, port) = addr_from(bytes);
    match ip.to_ipv4_mapped() {
        Some(v4) if with_port => format!("{v4}:{port}"),
        Some(v4) => v4.to_string(),
        None if with_port => format!("[{ip}]:{port}"),
        None => ip.to_string(),
    }
}

fn parse_addr(text: &str) -> Option<[u8; 18]> {
    let text = text.trim();
    let (ip, port): (std::net::IpAddr, u16) = if let Ok(sock) = text.parse::<std::net::SocketAddr>()
    {
        (sock.ip(), sock.port())
    } else {
        (text.trim_matches(['[', ']']).parse().ok()?, 0)
    };
    let v6 = match ip {
        std::net::IpAddr::V4(v4) => v4.to_ipv6_mapped(),
        std::net::IpAddr::V6(v6) => v6,
    };
    let mut out = [0u8; 18];
    out[..16].copy_from_slice(&v6.octets());
    out[16..].copy_from_slice(&port.to_le_bytes());
    Some(out)
}

fn write_text(buffer: *mut u8, size: usize, text: &str) {
    if buffer.is_null() || size == 0 {
        return;
    }
    let n = text.len().min(size - 1);
    // SAFETY: `size` writable bytes at `buffer`; n + 1 <= size.
    unsafe {
        ptr::copy_nonoverlapping(text.as_ptr(), buffer, n);
        buffer.add(n).write(0);
    }
}

unsafe extern "C" fn u_ip_to(
    _this: *mut c_void,
    addr: *const u8,
    buffer: *mut u8,
    size: usize,
    with_port: bool,
) {
    // SAFETY: the caller passes an 18-byte SteamNetworkingIPAddr.
    let bytes = unsafe { std::slice::from_raw_parts(addr, 18) };
    write_text(buffer, size, &addr_text(bytes, with_port));
}
unsafe extern "C" fn u_ip_parse(_this: *mut c_void, addr: *mut u8, text: *const c_char) -> bool {
    if addr.is_null() || text.is_null() {
        return false;
    }
    // SAFETY: caller-supplied NUL-terminated string and 18-byte out struct.
    let text = unsafe { CStr::from_ptr(text) }
        .to_string_lossy()
        .into_owned();
    let Some(parsed) = parse_addr(&text) else {
        return false;
    };
    unsafe { ptr::copy_nonoverlapping(parsed.as_ptr(), addr, 18) };
    true
}
// ESteamNetworkingFakeIPType_NotFake: real FakeIPs are not supported.
unsafe extern "C" fn u_ip_fake(_this: *mut c_void, _addr: *const u8) -> i32 {
    1
}
unsafe extern "C" fn u_id_to(
    _this: *mut c_void,
    identity: *const u8,
    buffer: *mut u8,
    size: usize,
) {
    // SAFETY: the caller passes a 136-byte SteamNetworkingIdentity.
    let bytes = unsafe { std::slice::from_raw_parts(identity, 136) };
    let kind = i32::from_le_bytes(bytes[..4].try_into().unwrap());
    let payload = &bytes[8..];
    let size_field = (i32::from_le_bytes(bytes[4..8].try_into().unwrap()).max(0) as usize).min(128);
    let text = match kind {
        16 => format!(
            "steamid:{}",
            u64::from_le_bytes(payload[..8].try_into().unwrap())
        ),
        1 => format!("ip:{}", addr_text(payload, true)),
        2 => {
            let end = payload[..32].iter().position(|b| *b == 0).unwrap_or(32);
            format!("str:{}", String::from_utf8_lossy(&payload[..end]))
        }
        3 => {
            let hex: String = payload[..size_field.min(32)]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            format!("gen:{hex}")
        }
        18 => format!(
            "psn:{}",
            u64::from_le_bytes(payload[..8].try_into().unwrap())
        ),
        19 => format!(
            "stadia:{}",
            u64::from_le_bytes(payload[..8].try_into().unwrap())
        ),
        _ => String::new(),
    };
    write_text(buffer, size, &text);
}
// Older SDKs call ParseString(identity, text); newer headers insert sizeof(identity)
// before the string. A size is never a valid pointer, so accept both forms.
unsafe extern "C" fn u_id_parse(
    _this: *mut c_void,
    identity: *mut u8,
    second: usize,
    third: usize,
) -> bool {
    let text = if second < 65536 {
        if second < 136 {
            return false;
        }
        third as *const c_char
    } else {
        second as *const c_char
    };
    if identity.is_null() || text.is_null() {
        return false;
    }
    // SAFETY: caller-supplied NUL-terminated string and 136-byte out struct.
    let text = unsafe { CStr::from_ptr(text) }
        .to_string_lossy()
        .into_owned();
    let mut out = [0u8; 136];
    let mut set = |kind: i32, size: i32, payload: &[u8]| {
        out[..4].copy_from_slice(&kind.to_le_bytes());
        out[4..8].copy_from_slice(&size.to_le_bytes());
        out[8..8 + payload.len()].copy_from_slice(payload);
    };
    if let Some(rest) = text.strip_prefix("steamid:") {
        let Ok(id) = rest.trim().parse::<u64>() else {
            return false;
        };
        set(16, 8, &id.to_le_bytes());
    } else if let Some(rest) = text.strip_prefix("ip:") {
        let Some(addr) = parse_addr(rest) else {
            return false;
        };
        set(1, 18, &addr);
    } else if let Some(rest) = text.strip_prefix("str:") {
        if rest.len() >= 32 {
            return false;
        }
        let mut payload = rest.as_bytes().to_vec();
        payload.push(0);
        set(2, payload.len() as i32, &payload);
    } else if let Some(rest) = text.strip_prefix("gen:") {
        if rest.len() % 2 != 0 || rest.len() / 2 > 32 || !rest.is_ascii() {
            return false;
        }
        let Some(bytes) = (0..rest.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&rest[i..i + 2], 16).ok())
            .collect::<Option<Vec<u8>>>()
        else {
            return false;
        };
        set(3, bytes.len() as i32, &bytes);
    } else {
        return false;
    }
    // SAFETY: the caller supplied a 136-byte SteamNetworkingIdentity.
    unsafe { ptr::copy_nonoverlapping(out.as_ptr(), identity, 136) };
    true
}

// GNS owns the message header and release function, including its private tail.
unsafe extern "C" fn u_alloc(_this: *mut c_void, size: i32) -> *mut u8 {
    if !(0..=512 * 1024).contains(&size) {
        return ptr::null_mut();
    }
    sockets::allocate(size).cast()
}
// EResult_Fail: FakeIPs are not supported.
unsafe extern "C" fn u_realid(_this: *mut c_void, _addr: *const u8, _identity: *mut u8) -> i32 {
    2
}
unsafe extern "C" fn u_info(_this: *mut c_void) -> *const c_char {
    ptr::null()
}
// ESteamNetworkingConfig_Invalid: no editable generic values are exposed.
unsafe extern "C" fn u_iter(_this: *mut c_void, _current: i32, _dev: bool) -> i32 {
    0
}

unsafe extern "C" fn u_up_to_date(_this: *mut c_void, max_age: f32) -> bool {
    let ready = !local_markers().is_empty() && max_age >= 0.0 && {
        let age = sockets::ping_age();
        age >= 0.0 && age <= max_age
    };
    trace(format_args!(
        "networkingutils.CheckPingDataUpToDate = {ready}"
    ));
    ready
}
fn direct_ping(pop: u32) -> i32 {
    pops()
        .iter()
        .find(|(id, _)| *id == pop)
        .map_or(-1, |(_, ms)| *ms)
}
unsafe extern "C" fn u_ping_dc(_this: *mut c_void, pop: u32, via: *mut u32) -> i32 {
    unsafe { sockets::routed_ping(pop, via) }
}
unsafe extern "C" fn u_direct(_this: *mut c_void, pop: u32) -> i32 {
    direct_ping(pop)
}
unsafe extern "C" fn u_pop_count(_this: *mut c_void) -> i32 {
    let count = pops().len() as i32;
    trace(format_args!("networkingutils.GetPOPCount = {count}"));
    count
}
unsafe extern "C" fn u_pop_list(_this: *mut c_void, list: *mut u32, size: i32) -> i32 {
    let entries = pops();
    let written = entries.len().min(size.max(0) as usize);
    if !list.is_null() {
        for (index, (id, _)) in entries.iter().take(written).enumerate() {
            // SAFETY: the caller supplied room for `size` ids.
            unsafe { list.add(index).write(*id) };
        }
    }
    trace(format_args!("networkingutils.GetPOPList = {written}"));
    written as i32
}
unsafe extern "C" fn u_timestamp(_this: *mut c_void) -> i64 {
    sockets::timestamp()
}
unsafe extern "C" fn u_debug(_this: *mut c_void, _level: i32, _func: *const c_void) {
    unsafe { sockets::debug(_level, _func) };
}
unsafe extern "C" fn u_ipv4_type(_this: *mut c_void, _ip: u32) -> i32 {
    trace("networkingutils.GetIPv4FakeIPType = NotFake");
    1
}
unsafe extern "C" fn u_set_config(
    _this: *mut c_void,
    _value: i32,
    _scope: i32,
    _obj: isize,
    _data_type: i32,
    _arg: *const c_void,
) -> bool {
    unsafe { sockets::config(_value, _scope, _obj, _data_type, _arg) }
}
unsafe extern "C" fn u_get_config(
    _this: *mut c_void,
    value: i32,
    scope: i32,
    object: isize,
    kind: *mut i32,
    data: *mut c_void,
    size: *mut usize,
) -> i32 {
    unsafe { sockets::get_config(value, scope, object, kind, data, size) }
}
// SteamRelayNetworkStatus_t: i32 avail, i32 pingInProgress, i32 availNetworkConfig,
// i32 availAnyRelay, char debugMsg[256]. Hosted interop gate => CannotTry (-102).
unsafe extern "C" fn u_relay(_this: *mut c_void, details: *mut u8) -> i32 {
    const CANNOT_TRY: i32 = -102;
    const CURRENT: i32 = 100;
    // P2P relay transport is verified. Keep hosted matchmaking gated until a
    // ticket-authorized dedicated-server exchange has been verified.
    let config = if pops().is_empty() {
        CANNOT_TRY
    } else {
        CURRENT
    };
    trace(format_args!(
        "networkingutils.GetRelayNetworkStatus = CannotTry (config {config})"
    ));
    if !details.is_null() {
        let mut block = [0u8; 272];
        for (i, value) in [CANNOT_TRY, 0, config, CANNOT_TRY].iter().enumerate() {
            block[i * 4..i * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
        let message = b"hosted SDR interoperability pending verification";
        block[16..16 + message.len()].copy_from_slice(message);
        unsafe { ptr::copy_nonoverlapping(block.as_ptr(), details, block.len()) };
    }
    CANNOT_TRY
}
static UTILS_TABLE: VTable<25> = VTable([
    u_alloc as *const (),
    u_relay as *const (),
    u_loc as *const (),
    u_est2 as *const (),
    u_estlocal as *const (),
    u_tostr as *const (),
    u_parse as *const (),
    u_up_to_date as *const (),
    u_ping_dc as *const (),
    u_direct as *const (),
    u_pop_count as *const (),
    u_pop_list as *const (),
    u_timestamp as *const (),
    u_debug as *const (),
    u_ipv4_type as *const (),
    u_realid as *const (),
    u_set_config as *const (),
    u_get_config as *const (),
    u_info as *const (),
    u_iter as *const (),
    u_ip_to as *const (),
    u_ip_parse as *const (),
    u_ip_fake as *const (),
    u_id_to as *const (),
    u_id_parse as *const (),
]);
pub(super) static UTILS: Object = Object {
    table: UTILS_TABLE.0.as_ptr(),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_and_identities_format_and_parse() {
        let addr = parse_addr("192.168.1.7:27015").unwrap();
        assert_eq!(addr_text(&addr, true), "192.168.1.7:27015");
        assert_eq!(addr_text(&addr, false), "192.168.1.7");
        let v6 = parse_addr("[::1]:99").unwrap();
        assert_eq!(addr_text(&v6, true), "[::1]:99");
        assert!(parse_addr("nonsense").is_none());
    }
}
