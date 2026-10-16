//! STEAMINVENTORY_INTERFACE_V003: no item inventory (results are invalid handles, calls report failure).
use super::*;

macro_rules! stubs {
    ($($name:ident => $label:literal;)*) => {
        $(unsafe extern "C" fn $name() -> usize {
            trace(concat!("inventory.", $label, " (unavailable)"));
            0
        })*
    };
}
stubs! {
    f0 => "GetResultStatus";
    f1 => "GetResultItems";
    f2 => "GetResultItemProperty";
    f3 => "GetResultTimestamp";
    f4 => "CheckResultSteamID";
    f5 => "DestroyResult";
    f6 => "GetAllItems";
    f7 => "GetItemsByID";
    f8 => "SerializeResult";
    f9 => "DeserializeResult";
    f10 => "GenerateItems";
    f11 => "GrantPromoItems";
    f12 => "AddPromoItem";
    f13 => "AddPromoItems";
    f14 => "ConsumeItem";
    f15 => "ExchangeItems";
    f16 => "TransferItemQuantity";
    f17 => "SendItemDropHeartbeat";
    f18 => "TriggerItemDrop";
    f19 => "TradeItems";
    f20 => "LoadItemDefinitions";
    f21 => "GetItemDefinitionIDs";
    f22 => "GetItemDefinitionProperty";
    f23 => "RequestEligiblePromoItemDefinitionsIDs";
    f24 => "GetEligiblePromoItemDefinitionIDs";
    f25 => "StartPurchase";
    f26 => "RequestPrices";
    f27 => "GetNumItemsWithPrices";
    f28 => "GetItemsWithPrices";
    f29 => "GetItemPrice";
    f30 => "StartUpdateProperties";
    f31 => "RemoveProperty";
    f32 => "SetProperty";
    f33 => "SetProperty";
    f34 => "SetProperty";
    f35 => "SetProperty";
    f36 => "SubmitUpdateProperties";
    f37 => "InspectItem";
}
pub(super) static TABLE: VTable<38> = VTable([
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
    f32 as *const (),
    f33 as *const (),
    f34 as *const (),
    f35 as *const (),
    f36 as *const (),
    f37 as *const (),
]);
pub(super) static OBJECT: Object = Object {
    table: TABLE.0.as_ptr(),
};
