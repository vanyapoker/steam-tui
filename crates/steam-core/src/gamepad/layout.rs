//! Which physical pads can be mirrored, and how their events map onto the
//! virtual Xbox 360 layout (the one Valve's virtual gamepad and Wine's XInput expect).
use super::ioctl::*;
use std::collections::{BTreeMap, BTreeSet};

pub const VIRTUAL_VENDOR: u16 = 0x28de;
pub const VIRTUAL_PRODUCT: u16 = 0x11ff;
const MICROSOFT: u16 = 0x045e;
const SONY: u16 = 0x054c;
const DUALSHOCK4: [u16; 3] = [0x05c4, 0x09cc, 0x0ba0];
const DUALSENSE: [u16; 2] = [0x0ce6, 0x0df2];

/// What Steam calls the pad: used for the info file's `type=` and the Steam Input type.
pub fn kind(vendor: u16, product: u16) -> &'static str {
    if vendor == SONY && DUALSHOCK4.contains(&product) {
        "ps4"
    } else if vendor == SONY && DUALSENSE.contains(&product) {
        "ps5"
    } else if (vendor, product) == (MICROSOFT, 0x028e) {
        "xbox360"
    } else {
        "xboxone"
    }
}

pub const STICK_MIN: i32 = -32768;
pub const STICK_MAX: i32 = 32767;
pub const TRIGGER_MAX: i32 = 255;

/// Virtual buttons, all with the codes the xpad driver uses.
pub const BUTTONS: [u16; 11] = [
    BTN_SOUTH, BTN_EAST, BTN_NORTH, BTN_WEST, BTN_TL, BTN_TR, BTN_SELECT, BTN_START, BTN_MODE,
    BTN_THUMBL, BTN_THUMBR,
];
/// Virtual axes with their ranges (min, max, fuzz, flat).
pub const AXES: [(u16, i32, i32, i32, i32); 8] = [
    (ABS_X, STICK_MIN, STICK_MAX, 16, 128),
    (ABS_Y, STICK_MIN, STICK_MAX, 16, 128),
    (ABS_Z, 0, TRIGGER_MAX, 0, 0),
    (ABS_RX, STICK_MIN, STICK_MAX, 16, 128),
    (ABS_RY, STICK_MIN, STICK_MAX, 16, 128),
    (ABS_RZ, 0, TRIGGER_MAX, 0, 0),
    (ABS_HAT0X, -1, 1, 0, 0),
    (ABS_HAT0Y, -1, 1, 0, 0),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    pub min: i32,
    pub max: i32,
}

/// What the kernel reports about an event device.
#[derive(Clone, Debug, Default)]
pub struct Capabilities {
    pub vendor: u16,
    pub product: u16,
    pub name: String,
    pub abs: BTreeMap<u16, Range>,
    pub keys: BTreeSet<u16>,
    pub rumble: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    Axis(u16),
    Button(u16),
}

/// Where each virtual control comes from on one physical pad.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub left: (u16, u16),
    pub right: (u16, u16),
    pub triggers: (Trigger, Trigger),
    pub hat_axes: bool,
}

/// Maps the controls of Xbox-compatible pads (the xpad driver: sticks X/Y and RX/RY,
/// triggers Z/RZ; the HID drivers used over Bluetooth: right stick Z/RZ, triggers on
/// Brake/Gas), of the DualSense (same axes as xpad) and of the DualShock 4 (right stick
/// Z/RZ, triggers RX/RY, as the hid-sony driver reports them). The touchpad and motion
/// sensor nodes of Sony pads have no face buttons and are never picked. Pads of other
/// families are left untouched.
pub fn detect(caps: &Capabilities) -> Option<Layout> {
    if caps.vendor == VIRTUAL_VENDOR && caps.product == VIRTUAL_PRODUCT {
        return None;
    }
    let xbox_family = caps.vendor == MICROSOFT
        || caps.name.to_ascii_lowercase().contains("xbox")
        || caps.name.to_ascii_lowercase().contains("x-box");
    let dualshock4 = caps.vendor == SONY && DUALSHOCK4.contains(&caps.product);
    let dualsense = caps.vendor == SONY && DUALSENSE.contains(&caps.product);
    if !(xbox_family || dualshock4 || dualsense) {
        return None;
    }
    let has = |axis: u16| caps.abs.contains_key(&axis);
    if !(has(ABS_X) && has(ABS_Y)) || !caps.keys.contains(&BTN_SOUTH) {
        return None;
    }
    let (right, triggers) = if dualshock4 {
        if !(has(ABS_Z) && has(ABS_RZ) && has(ABS_RX) && has(ABS_RY)) {
            return None;
        }
        (
            (ABS_Z, ABS_RZ),
            (Trigger::Axis(ABS_RX), Trigger::Axis(ABS_RY)),
        )
    } else if has(ABS_BRAKE) && has(ABS_GAS) {
        let right = if has(ABS_RX) && has(ABS_RY) {
            (ABS_RX, ABS_RY)
        } else if has(ABS_Z) && has(ABS_RZ) {
            (ABS_Z, ABS_RZ)
        } else {
            return None;
        };
        (right, (Trigger::Axis(ABS_BRAKE), Trigger::Axis(ABS_GAS)))
    } else if has(ABS_RX) && has(ABS_RY) {
        let triggers = if has(ABS_Z) && has(ABS_RZ) {
            (Trigger::Axis(ABS_Z), Trigger::Axis(ABS_RZ))
        } else if caps.keys.contains(&BTN_TL2) && caps.keys.contains(&BTN_TR2) {
            (Trigger::Button(BTN_TL2), Trigger::Button(BTN_TR2))
        } else {
            return None;
        };
        ((ABS_RX, ABS_RY), triggers)
    } else {
        return None;
    };
    let hat_axes = has(ABS_HAT0X) && has(ABS_HAT0Y);
    let dpad_buttons = [BTN_DPAD_UP, BTN_DPAD_DOWN, BTN_DPAD_LEFT, BTN_DPAD_RIGHT]
        .iter()
        .all(|b| caps.keys.contains(b));
    if !hat_axes && !dpad_buttons {
        return None;
    }
    Some(Layout {
        left: (ABS_X, ABS_Y),
        right,
        triggers,
        hat_axes,
    })
}

pub fn scale(value: i32, from: Range, min: i32, max: i32) -> i32 {
    let span = i64::from(from.max) - i64::from(from.min);
    if span <= 0 {
        return min;
    }
    let value = i64::from(value).clamp(i64::from(from.min), i64::from(from.max));
    let scaled = (value - i64::from(from.min)) * (i64::from(max) - i64::from(min)) / span;
    (scaled + i64::from(min)) as i32
}

pub type Event = (u16, u16, i32);

/// Turns physical events into virtual ones. Holds the d-pad button state for pads
/// that report their d-pad as buttons instead of a hat.
pub struct Translator {
    layout: Layout,
    abs: BTreeMap<u16, Range>,
    keys: BTreeSet<u16>,
    dpad: [bool; 4],
}

impl Translator {
    pub fn new(layout: Layout, caps: &Capabilities) -> Self {
        Self {
            layout,
            abs: caps.abs.clone(),
            keys: caps.keys.clone(),
            dpad: [false; 4],
        }
    }

    fn axis(&self, code: u16, value: i32, min: i32, max: i32) -> i32 {
        match self.abs.get(&code) {
            Some(range) => scale(value, *range, min, max),
            None => 0,
        }
    }

    pub fn translate(&mut self, kind: u16, code: u16, value: i32) -> Vec<Event> {
        match kind {
            EV_KEY => self.key(code, value),
            EV_ABS => self.absolute(code, value),
            _ => Vec::new(),
        }
    }

    fn key(&mut self, code: u16, value: i32) -> Vec<Event> {
        let pressed = i32::from(value != 0);
        let pad = [BTN_DPAD_UP, BTN_DPAD_DOWN, BTN_DPAD_LEFT, BTN_DPAD_RIGHT];
        if !self.layout.hat_axes
            && let Some(index) = pad.iter().position(|b| *b == code)
        {
            self.dpad[index] = pressed != 0;
            let x = i32::from(self.dpad[3]) - i32::from(self.dpad[2]);
            let y = i32::from(self.dpad[1]) - i32::from(self.dpad[0]);
            return vec![(EV_ABS, ABS_HAT0X, x), (EV_ABS, ABS_HAT0Y, y)];
        }
        let (left, right) = self.layout.triggers;
        for (trigger, axis) in [(left, ABS_Z), (right, ABS_RZ)] {
            if trigger == Trigger::Button(code) {
                return vec![(EV_ABS, axis, pressed * TRIGGER_MAX)];
            }
        }
        if BUTTONS.contains(&code) && self.keys.contains(&code) {
            return vec![(EV_KEY, code, pressed)];
        }
        Vec::new()
    }

    fn absolute(&mut self, code: u16, value: i32) -> Vec<Event> {
        let (lx, ly) = self.layout.left;
        let (rx, ry) = self.layout.right;
        let stick = |axis| (EV_ABS, axis, self.axis(code, value, STICK_MIN, STICK_MAX));
        let (left, right) = self.layout.triggers;
        let trigger = |axis| (EV_ABS, axis, self.axis(code, value, 0, TRIGGER_MAX));
        if code == lx {
            return vec![stick(ABS_X)];
        }
        if code == ly {
            return vec![stick(ABS_Y)];
        }
        if code == rx {
            return vec![stick(ABS_RX)];
        }
        if code == ry {
            return vec![stick(ABS_RY)];
        }
        if left == Trigger::Axis(code) {
            return vec![trigger(ABS_Z)];
        }
        if right == Trigger::Axis(code) {
            return vec![trigger(ABS_RZ)];
        }
        if self.layout.hat_axes && (code == ABS_HAT0X || code == ABS_HAT0Y) {
            return vec![(EV_ABS, code, value.clamp(-1, 1))];
        }
        Vec::new()
    }

    /// Every virtual control at rest (used while the physical pad is away).
    pub fn neutral() -> Vec<Event> {
        let mut events: Vec<Event> = BUTTONS.iter().map(|b| (EV_KEY, *b, 0)).collect();
        for (axis, min, max, _, _) in AXES {
            let rest = if min < 0 && max > 0 && max - min > 2 {
                0
            } else {
                min.max(0)
            };
            events.push((EV_ABS, axis, rest));
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(vendor: u16, name: &str, abs: &[(u16, i32, i32)], keys: &[u16]) -> Capabilities {
        Capabilities {
            vendor,
            product: 0x0b13,
            name: name.into(),
            abs: abs
                .iter()
                .map(|(a, min, max)| {
                    (
                        *a,
                        Range {
                            min: *min,
                            max: *max,
                        },
                    )
                })
                .collect(),
            keys: keys.iter().copied().collect(),
            rumble: true,
        }
    }
    const PAD_KEYS: [u16; 12] = [
        BTN_SOUTH, BTN_EAST, BTN_NORTH, BTN_WEST, BTN_TL, BTN_TR, BTN_SELECT, BTN_START, BTN_MODE,
        BTN_THUMBL, BTN_THUMBR, BTN_TL2,
    ];
    fn xpad() -> Capabilities {
        caps(
            0x045e,
            "Microsoft X-Box 360 pad",
            &[
                (ABS_X, -32768, 32767),
                (ABS_Y, -32768, 32767),
                (ABS_Z, 0, 255),
                (ABS_RX, -32768, 32767),
                (ABS_RY, -32768, 32767),
                (ABS_RZ, 0, 255),
                (ABS_HAT0X, -1, 1),
                (ABS_HAT0Y, -1, 1),
            ],
            &PAD_KEYS,
        )
    }
    fn bluetooth() -> Capabilities {
        caps(
            0x045e,
            "Xbox Wireless Controller",
            &[
                (ABS_X, 0, 65535),
                (ABS_Y, 0, 65535),
                (ABS_Z, 0, 65535),
                (ABS_RZ, 0, 65535),
                (ABS_BRAKE, 0, 1023),
                (ABS_GAS, 0, 1023),
                (ABS_HAT0X, -1, 1),
                (ABS_HAT0Y, -1, 1),
            ],
            &PAD_KEYS,
        )
    }

    #[test]
    fn xpad_and_bluetooth_layouts_are_recognised() {
        let layout = detect(&xpad()).unwrap();
        assert_eq!(layout.right, (ABS_RX, ABS_RY));
        assert_eq!(
            layout.triggers,
            (Trigger::Axis(ABS_Z), Trigger::Axis(ABS_RZ))
        );
        let layout = detect(&bluetooth()).unwrap();
        assert_eq!(layout.right, (ABS_Z, ABS_RZ));
        assert_eq!(
            layout.triggers,
            (Trigger::Axis(ABS_BRAKE), Trigger::Axis(ABS_GAS))
        );
    }

    fn sony(product: u16, abs: &[(u16, i32, i32)], keys: &[u16]) -> Capabilities {
        let mut caps = caps(0x054c, "Wireless Controller", abs, keys);
        caps.product = product;
        caps
    }

    #[test]
    fn dualshock_4_and_dualsense_layouts_are_recognised() {
        let dualshock_axes = [
            (ABS_X, 0, 255),
            (ABS_Y, 0, 255),
            (ABS_Z, 0, 255),
            (ABS_RZ, 0, 255),
            (ABS_RX, 0, 255),
            (ABS_RY, 0, 255),
            (ABS_HAT0X, -1, 1),
            (ABS_HAT0Y, -1, 1),
        ];
        let ds4 = sony(0x09cc, &dualshock_axes, &PAD_KEYS);
        let layout = detect(&ds4).unwrap();
        assert_eq!(layout.right, (ABS_Z, ABS_RZ));
        assert_eq!(
            layout.triggers,
            (Trigger::Axis(ABS_RX), Trigger::Axis(ABS_RY))
        );
        let mut t = Translator::new(layout, &ds4);
        // R2 is on RY, the right stick on Z/RZ.
        assert_eq!(
            t.translate(EV_ABS, ABS_RY, 255),
            vec![(EV_ABS, ABS_RZ, 255)]
        );
        assert_eq!(
            t.translate(EV_ABS, ABS_RZ, 0),
            vec![(EV_ABS, ABS_RY, -32768)]
        );
        // The DualSense driver reports the xpad layout.
        let mut ds5 = xpad();
        ds5.vendor = 0x054c;
        ds5.product = 0x0ce6;
        ds5.name = "DualSense Wireless Controller".into();
        let layout = detect(&ds5).unwrap();
        assert_eq!(layout.right, (ABS_RX, ABS_RY));
        assert_eq!(
            layout.triggers,
            (Trigger::Axis(ABS_Z), Trigger::Axis(ABS_RZ))
        );
        assert_eq!(kind(0x054c, 0x09cc), "ps4");
        assert_eq!(kind(0x054c, 0x0ce6), "ps5");
        assert_eq!(kind(0x045e, 0x028e), "xbox360");
        assert_eq!(kind(0x045e, 0x0b13), "xboxone");
    }

    #[test]
    fn sony_touchpad_and_motion_nodes_are_not_pads() {
        let axes = [
            (ABS_X, -8192, 8191),
            (ABS_Y, -8192, 8191),
            (ABS_Z, -8192, 8191),
        ];
        assert!(detect(&sony(0x09cc, &axes, &[])).is_none());
        assert!(
            detect(&sony(
                0x09cc,
                &[(ABS_X, 0, 1920), (ABS_Y, 0, 940)],
                &[0x14a]
            ))
            .is_none()
        );
        // An unknown Sony product is left to Wine.
        assert!(detect(&sony(0x1234, &axes, &PAD_KEYS)).is_none());
    }

    #[test]
    fn other_pads_and_the_virtual_pad_itself_are_left_alone() {
        let mut sony = xpad();
        sony.vendor = 0x054c;
        sony.name = "Wireless Controller".into();
        sony.product = 0x1234;
        assert!(detect(&sony).is_none());
        let mut ours = xpad();
        ours.vendor = VIRTUAL_VENDOR;
        ours.product = VIRTUAL_PRODUCT;
        assert!(detect(&ours).is_none());
        let mut no_buttons = xpad();
        no_buttons.keys.clear();
        assert!(detect(&no_buttons).is_none());
    }

    #[test]
    fn bluetooth_axes_are_normalised() {
        let caps = bluetooth();
        let mut t = Translator::new(detect(&caps).unwrap(), &caps);
        assert_eq!(t.translate(EV_ABS, ABS_X, 0), vec![(EV_ABS, ABS_X, -32768)]);
        assert_eq!(
            t.translate(EV_ABS, ABS_X, 65535),
            vec![(EV_ABS, ABS_X, 32767)]
        );
        let centre = t.translate(EV_ABS, ABS_Y, 32768)[0].2;
        assert!(centre.abs() <= 1, "{centre}");
        // The Z/RZ axes are the right stick on this layout, Brake/Gas the triggers.
        assert_eq!(
            t.translate(EV_ABS, ABS_Z, 0),
            vec![(EV_ABS, ABS_RX, -32768)]
        );
        assert_eq!(
            t.translate(EV_ABS, ABS_BRAKE, 1023),
            vec![(EV_ABS, ABS_Z, 255)]
        );
        assert_eq!(t.translate(EV_ABS, ABS_GAS, 0), vec![(EV_ABS, ABS_RZ, 0)]);
    }

    #[test]
    fn buttons_and_the_hat_pass_through() {
        let caps = xpad();
        let mut t = Translator::new(detect(&caps).unwrap(), &caps);
        assert_eq!(
            t.translate(EV_KEY, BTN_SOUTH, 1),
            vec![(EV_KEY, BTN_SOUTH, 1)]
        );
        assert_eq!(
            t.translate(EV_KEY, BTN_MODE, 0),
            vec![(EV_KEY, BTN_MODE, 0)]
        );
        assert_eq!(t.translate(EV_KEY, 0x2fd, 1), vec![]);
        assert_eq!(
            t.translate(EV_ABS, ABS_HAT0X, -1),
            vec![(EV_ABS, ABS_HAT0X, -1)]
        );
        assert_eq!(t.translate(EV_ABS, ABS_Z, 255), vec![(EV_ABS, ABS_Z, 255)]);
    }

    #[test]
    fn button_dpads_and_digital_triggers_become_axes() {
        let mut caps = xpad();
        for axis in [ABS_HAT0X, ABS_HAT0Y, ABS_Z, ABS_RZ] {
            caps.abs.remove(&axis);
        }
        caps.keys.extend([
            BTN_DPAD_UP,
            BTN_DPAD_DOWN,
            BTN_DPAD_LEFT,
            BTN_DPAD_RIGHT,
            BTN_TR2,
        ]);
        let layout = detect(&caps).unwrap();
        assert!(!layout.hat_axes);
        let mut t = Translator::new(layout, &caps);
        assert_eq!(
            t.translate(EV_KEY, BTN_DPAD_LEFT, 1),
            vec![(EV_ABS, ABS_HAT0X, -1), (EV_ABS, ABS_HAT0Y, 0)]
        );
        assert_eq!(
            t.translate(EV_KEY, BTN_DPAD_DOWN, 1),
            vec![(EV_ABS, ABS_HAT0X, -1), (EV_ABS, ABS_HAT0Y, 1)]
        );
        assert_eq!(t.translate(EV_KEY, BTN_TR2, 1), vec![(EV_ABS, ABS_RZ, 255)]);
        assert_eq!(t.translate(EV_KEY, BTN_TL2, 0), vec![(EV_ABS, ABS_Z, 0)]);
    }

    #[test]
    fn neutral_state_centres_sticks_and_releases_everything() {
        let events = Translator::neutral();
        assert!(events.contains(&(EV_ABS, ABS_X, 0)));
        assert!(events.contains(&(EV_ABS, ABS_Z, 0)));
        assert!(events.contains(&(EV_ABS, ABS_HAT0Y, 0)));
        assert!(events.contains(&(EV_KEY, BTN_SOUTH, 0)));
        assert_eq!(events.len(), BUTTONS.len() + AXES.len());
    }

    #[test]
    fn scaling_clamps_and_rejects_empty_ranges() {
        let range = Range { min: 0, max: 100 };
        assert_eq!(scale(-5, range, 0, 255), 0);
        assert_eq!(scale(500, range, 0, 255), 255);
        assert_eq!(scale(5, Range { min: 3, max: 3 }, 7, 9), 7);
    }
}
