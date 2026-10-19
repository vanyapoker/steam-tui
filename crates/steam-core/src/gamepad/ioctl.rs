//! evdev / uinput ioctl requests and event codes (linux/input.h, linux/uinput.h).
use libc::c_ulong;
use std::mem::size_of;

const WRITE: u32 = 1;
const READ: u32 = 2;

const fn ioc(dir: u32, kind: u8, nr: u32, size: usize) -> c_ulong {
    ((dir << 30) | ((size as u32) << 16) | ((kind as u32) << 8) | nr) as c_ulong
}

pub const fn eviocgname(len: usize) -> c_ulong {
    ioc(READ, b'E', 0x06, len)
}
pub const fn eviocgbit(event: u16, len: usize) -> c_ulong {
    ioc(READ, b'E', 0x20 + event as u32, len)
}
pub const fn eviocgabs(axis: u16) -> c_ulong {
    ioc(
        READ,
        b'E',
        0x40 + axis as u32,
        size_of::<libc::input_absinfo>(),
    )
}
pub const fn eviocgkey(len: usize) -> c_ulong {
    ioc(READ, b'E', 0x18, len)
}
pub const EVIOCGID: c_ulong = ioc(READ, b'E', 0x02, size_of::<libc::input_id>());
pub const EVIOCGRAB: c_ulong = ioc(WRITE, b'E', 0x90, size_of::<libc::c_int>());
pub const EVIOCSFF: c_ulong = ioc(WRITE, b'E', 0x80, size_of::<libc::ff_effect>());
pub const EVIOCRMFF: c_ulong = ioc(WRITE, b'E', 0x81, size_of::<libc::c_int>());

pub const UI_DEV_CREATE: c_ulong = ioc(0, b'U', 1, 0);
pub const UI_DEV_SETUP: c_ulong = ioc(WRITE, b'U', 3, size_of::<libc::uinput_setup>());
pub const UI_ABS_SETUP: c_ulong = ioc(WRITE, b'U', 4, size_of::<libc::uinput_abs_setup>());
pub const UI_SET_EVBIT: c_ulong = ioc(WRITE, b'U', 100, size_of::<libc::c_int>());
pub const UI_SET_KEYBIT: c_ulong = ioc(WRITE, b'U', 101, size_of::<libc::c_int>());
pub const UI_SET_ABSBIT: c_ulong = ioc(WRITE, b'U', 103, size_of::<libc::c_int>());
pub const UI_SET_FFBIT: c_ulong = ioc(WRITE, b'U', 107, size_of::<libc::c_int>());
pub const UI_BEGIN_FF_UPLOAD: c_ulong =
    ioc(READ | WRITE, b'U', 200, size_of::<libc::uinput_ff_upload>());
pub const UI_END_FF_UPLOAD: c_ulong = ioc(WRITE, b'U', 201, size_of::<libc::uinput_ff_upload>());
pub const UI_BEGIN_FF_ERASE: c_ulong =
    ioc(READ | WRITE, b'U', 202, size_of::<libc::uinput_ff_erase>());
pub const UI_END_FF_ERASE: c_ulong = ioc(WRITE, b'U', 203, size_of::<libc::uinput_ff_erase>());

pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_ABS: u16 = 0x03;
pub const EV_FF: u16 = 0x15;
pub const EV_UINPUT: u16 = 0x0101;
pub const SYN_REPORT: u16 = 0;
pub const SYN_DROPPED: u16 = 3;
pub const UI_FF_UPLOAD: u16 = 1;
pub const UI_FF_ERASE: u16 = 2;
pub const FF_RUMBLE: u16 = 0x50;
pub const BUS_USB: u16 = 0x03;

pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const ABS_Z: u16 = 0x02;
pub const ABS_RX: u16 = 0x03;
pub const ABS_RY: u16 = 0x04;
pub const ABS_RZ: u16 = 0x05;
pub const ABS_GAS: u16 = 0x09;
pub const ABS_BRAKE: u16 = 0x0a;
pub const ABS_HAT0X: u16 = 0x10;
pub const ABS_HAT0Y: u16 = 0x11;

pub const BTN_SOUTH: u16 = 0x130;
pub const BTN_EAST: u16 = 0x131;
pub const BTN_NORTH: u16 = 0x133;
pub const BTN_WEST: u16 = 0x134;
pub const BTN_TL: u16 = 0x136;
pub const BTN_TR: u16 = 0x137;
pub const BTN_TL2: u16 = 0x138;
pub const BTN_TR2: u16 = 0x139;
pub const BTN_SELECT: u16 = 0x13a;
pub const BTN_START: u16 = 0x13b;
pub const BTN_MODE: u16 = 0x13c;
pub const BTN_THUMBL: u16 = 0x13d;
pub const BTN_THUMBR: u16 = 0x13e;
pub const BTN_DPAD_UP: u16 = 0x220;
pub const BTN_DPAD_DOWN: u16 = 0x221;
pub const BTN_DPAD_LEFT: u16 = 0x222;
pub const BTN_DPAD_RIGHT: u16 = 0x223;

#[cfg(test)]
mod tests {
    use super::*;

    // Values from the kernel headers as compiled on x86-64.
    #[test]
    fn requests_match_the_kernel_values() {
        assert_eq!(EVIOCGID, 0x8008_4502);
        assert_eq!(EVIOCGRAB, 0x4004_4590);
        assert_eq!(eviocgabs(0), 0x8018_4540);
        assert_eq!(eviocgname(128), 0x8080_4506);
        assert_eq!(eviocgbit(1, 96), 0x8060_4521);
        assert_eq!(EVIOCSFF, 0x4030_4580);
        assert_eq!(EVIOCRMFF, 0x4004_4581);
        assert_eq!(UI_DEV_CREATE, 0x5501);
        assert_eq!(UI_SET_EVBIT, 0x4004_5564);
        assert_eq!(UI_DEV_SETUP, 0x405c_5503);
        assert_eq!(UI_ABS_SETUP, 0x401c_5504);
        assert_eq!(UI_BEGIN_FF_UPLOAD, 0xc068_55c8);
        assert_eq!(UI_END_FF_UPLOAD, 0x4068_55c9);
        assert_eq!(UI_BEGIN_FF_ERASE, 0xc00c_55ca);
        assert_eq!(UI_END_FF_ERASE, 0x400c_55cb);
    }
}
