//! STEAMMUSICREMOTE_INTERFACE_VERSION001: no remote music control (registration fails).
use super::*;

macro_rules! stubs {
    ($($name:ident => $label:literal;)*) => {
        $(unsafe extern "C" fn $name() -> usize {
            trace(concat!("musicremote.", $label, " (unavailable)"));
            0
        })*
    };
}
stubs! {
    f0 => "RegisterSteamMusicRemote";
    f1 => "DeregisterSteamMusicRemote";
    f2 => "BIsCurrentMusicRemote";
    f3 => "BActivationSuccess";
    f4 => "SetDisplayName";
    f5 => "SetPNGIcon_64x64";
    f6 => "EnablePlayPrevious";
    f7 => "EnablePlayNext";
    f8 => "EnableShuffled";
    f9 => "EnableLooped";
    f10 => "EnableQueue";
    f11 => "EnablePlaylists";
    f12 => "UpdatePlaybackStatus";
    f13 => "UpdateShuffled";
    f14 => "UpdateLooped";
    f15 => "UpdateVolume";
    f16 => "CurrentEntryWillChange";
    f17 => "CurrentEntryIsAvailable";
    f18 => "UpdateCurrentEntryText";
    f19 => "UpdateCurrentEntryElapsedSeconds";
    f20 => "UpdateCurrentEntryCoverArt";
    f21 => "CurrentEntryDidChange";
    f22 => "QueueWillChange";
    f23 => "ResetQueueEntries";
    f24 => "SetQueueEntry";
    f25 => "SetCurrentQueueEntry";
    f26 => "QueueDidChange";
    f27 => "PlaylistWillChange";
    f28 => "ResetPlaylistEntries";
    f29 => "SetPlaylistEntry";
    f30 => "SetCurrentPlaylistEntry";
    f31 => "PlaylistDidChange";
}
static TABLE: VTable<32> = VTable([
    f0 as *const (),
    f1 as *const (),
    f2 as *const (),
    f3 as *const (),
    f4 as *const (),
    f5 as *const (),
    f6 as *const (),
    f7 as *const (),
    f8 as *const (),
    f9 as *const (),
    f10 as *const (),
    f11 as *const (),
    f12 as *const (),
    f13 as *const (),
    f14 as *const (),
    f15 as *const (),
    f16 as *const (),
    f17 as *const (),
    f18 as *const (),
    f19 as *const (),
    f20 as *const (),
    f21 as *const (),
    f22 as *const (),
    f23 as *const (),
    f24 as *const (),
    f25 as *const (),
    f26 as *const (),
    f27 as *const (),
    f28 as *const (),
    f29 as *const (),
    f30 as *const (),
    f31 as *const (),
]);
pub(super) static OBJECT: Object = Object {
    table: TABLE.0.as_ptr(),
};
