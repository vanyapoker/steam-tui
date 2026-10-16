#include "sdr.h"
#include "steam_tui_auth.pb.h"
#include <cstring>
#include <fstream>
#include <iostream>
#include <stdexcept>
#include <steam/steamnetworkingsockets.h>
static void check(bool ok, const char *why) {
  if (!ok)
    throw std::runtime_error(why);
}
int main() {
  using namespace steam_tui_relay;
  auto request = ping_request(0x87654321, 0x12345678, 42, 0xaabbccdd, 27015);
  check(!memcmp(request.data(), "\x01\x02sdping", 8), "ping header");
  check(request[8] == 0x21 && request[11] == 0x87, "timestamp endian");
  check(request[12] == 0x78 && request[15] == 0x12, "cookie endian");
  check(request[20] == 42 && request[24] == (0xdd ^ 0x78),
        "revision/address encoding");
  check(request[30] == 13 && request[31] == 0 && request[1299] == 0,
        "protocol/padding");
  check(ping_request(1, 2, 3, 4, 5, true)[1] == 0x82,
        "small ping request flag");
  std::ifstream fixture(SDR_FIXTURE_PATH);
  std::string hex;
  fixture >> hex;
  check(!hex.empty() && hex.size() % 2 == 0, "fixture missing");
  std::string packet;
  for (size_t i = 0; i < hex.size(); i += 2)
    packet += char(std::stoul(hex.substr(i, 2), nullptr, 16));
  steam_tui::sdr::CMsgSteamDatagramRouterPingReply reply;
  check(ping_reply(packet.data(), packet.size(), 0x87654321, 0x12345678, reply),
        "live ping fixture");
  check(reply.latency_datacenter_ids_size() > 0,
        "relay supplies real data-center routes");
  check(!ping_reply(packet.data(), packet.size(), 1, 0x12345678, reply),
        "reject stale timestamp");
  check(!ping_reply(packet.data(), packet.size(), 0x87654321, 1, reply),
        "reject foreign cookie");
  check(!ping_reply(packet.data(), 1, 0x87654321, 0x12345678, reply),
        "reject short packet");
  check(!ping_reply(packet.data(), 1301, 0x87654321, 0x12345678, reply),
        "reject oversize without reading buffer");
  check(!ping_reply(nullptr, 100, 0, 0, reply), "reject null input");
  packet[0] = 3;
  check(
      !ping_reply(packet.data(), packet.size(), 0x87654321, 0x12345678, reply),
      "reject wrong message kind");
  reply.Clear();
  reply.set_client_cookie(2);
  reply.set_client_timestamp(1);
  reply.add_latency_datacenter_ids(4);
  packet = std::string(1, char(2)) + reply.SerializeAsString();
  check(!ping_reply(packet.data(), packet.size(), 1, 2, reply),
        "reject mismatched routing arrays");
  SteamNetworkingErrMsg error{};
  check(GameNetworkingSockets_Init(nullptr, error), error);
  steam_tui::sdr::CMsgSteamDatagramSignedRelayAuthTicket ticket;
  ticket.set_signature(std::string(64, 'x'));
  ticket.set_key_id(1);
  steam_tui::sdr::CMsgSteamDatagramRelayAuthTicket body;
  body.set_app_id(480);
  body.set_time_expiry(UINT32_MAX);
  body.set_authorized_client_identity_string("steamid:76561198000000001");
  body.set_gameserver_identity_string("steamid:90071992547409921");
  body.set_gameserver_address("ams");
  ticket.set_ticket(body.SerializeAsString());
  auto wire = ticket.SerializeAsString();
  SteamDatagramRelayAuthTicket parsed;
  memset(&parsed, 0xa5, sizeof(parsed));
  check(!receive_ticket(wire.data(), wire.size(), 480, 76561198000000001ULL,
                        &parsed),
        "reject untrusted ticket signature");
  check(((unsigned char *)&parsed)[0] == 0xa5,
        "failure leaves caller output untouched");
  check(!receive_ticket(nullptr, 0, 480, 1), "reject empty ticket");
  check(find_ticket("steamid:90071992547409921", 7777) == 0,
        "invalid ticket was not cached");
  GameNetworkingSockets_Kill();
  std::cout << "PASS: SDR ping framing, live reply fixture, malformed replies, "
               "untrusted tickets, SDK ABI\n";
}
