#pragma once
class CMsgSteamNetworkingP2PRendezvous;
namespace SteamNetworkingSocketsLib {
class CSteamNetworkConnectionP2P;
void SteamTuiAttachRelay(CSteamNetworkConnectionP2P &connection);
void SteamTuiRelayPopulate(CSteamNetworkConnectionP2P &connection,
                           CMsgSteamNetworkingP2PRendezvous &msg);
void SteamTuiRelayReceive(CSteamNetworkConnectionP2P &connection,
                          const CMsgSteamNetworkingP2PRendezvous &msg);
} // namespace SteamNetworkingSocketsLib
