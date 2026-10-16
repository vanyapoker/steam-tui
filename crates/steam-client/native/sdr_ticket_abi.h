#pragma once
#include <cstddef>
#include <steam/steamnetworkingtypes.h>
// SDK layout recovered from steamclient.so 0216da00 / 0216d4e0.
// SDK 1.57 uses pack(4), including the ExtraField integer/string union.
#pragma pack(push, 4)
struct SteamDatagramHostedAddress {
  int size;
  unsigned char data[128];
};
struct SteamDatagramRelayAuthTicket {
  SteamNetworkingIdentity authorized_client;
  SteamNetworkingIdentity gameserver;
  uint32 authorized_public_ip;
  uint32 expiry;
  SteamDatagramHostedAddress routing;
  uint32 app;
  int virtual_port;
  int extra_count;
  struct ExtraField {
    int type; // 0=string, 1=int64, 2=fixed64
    char name[28];
    union {
      int64 integer;
      uint64 fixed;
      char text[128];
    } value;
  } extra[16];
};
#pragma pack(pop)
static_assert(offsetof(SteamDatagramRelayAuthTicket, routing) == 0x118);
static_assert(offsetof(SteamDatagramRelayAuthTicket, app) == 0x19c);
static_assert(offsetof(SteamDatagramRelayAuthTicket, extra) == 0x1a8);
static_assert(sizeof(SteamDatagramRelayAuthTicket::ExtraField) == 0xa0);
static_assert(sizeof(SteamDatagramRelayAuthTicket) == 0xba8);
