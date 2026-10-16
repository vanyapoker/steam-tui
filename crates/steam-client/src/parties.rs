//! SteamParties002: no beacon service is connected. Enumerations are empty,
//! queries fail and asynchronous operations return the SDK invalid call handle.
use super::*;

#[repr(C, packed(4))]
struct BeaconLocation {
    kind: i32,
    id: u64,
}

macro_rules! unavailable {
    ($($name:ident($($arg:ident: $ty:ty),*) -> $ret:ty = $value:expr;)*) => {
        $(unsafe extern "C" fn $name(_this: *mut c_void, $($arg: $ty),*) -> $ret {
            trace(concat!("parties.", stringify!($name), " (beacon service unavailable)"));
            $value
        })*
    };
}
unavailable! {
    count() -> u32 = 0;
    by_index(_index: u32) -> u64 = 0;
    details(_id: u64, _owner: *mut u64, _location: *mut BeaconLocation, _metadata: *mut c_char, _capacity: i32) -> bool = false;
    join(_id: u64) -> u64 = 0;
    locations(_list: *mut BeaconLocation, _capacity: u32) -> bool = false;
    create(_slots: u32, _location: *const BeaconLocation, _connect: *const c_char, _metadata: *const c_char) -> u64 = 0;
    reservation_completed(_beacon: u64, _user: u64) -> () = ();
    cancel_reservation(_beacon: u64, _user: u64) -> () = ();
    change_slots(_beacon: u64, _slots: u32) -> u64 = 0;
    destroy(_beacon: u64) -> bool = false;
    location_data(_location: BeaconLocation, _kind: i32, _out: *mut c_char, _capacity: i32) -> bool = false;
}
unsafe extern "C" fn location_count(_this: *mut c_void, count: *mut u32) -> bool {
    if !count.is_null() {
        unsafe { count.write(0) };
    }
    trace("parties.GetNumAvailableBeaconLocations (beacon service unavailable)");
    false
}
static TABLE: VTable<12> = VTable([
    count as *const (),
    by_index as *const (),
    details as *const (),
    join as *const (),
    location_count as *const (),
    locations as *const (),
    create as *const (),
    reservation_completed as *const (),
    cancel_reservation as *const (),
    change_slots as *const (),
    destroy as *const (),
    location_data as *const (),
]);
pub(super) static OBJECT: Object = Object {
    table: TABLE.0.as_ptr(),
};
