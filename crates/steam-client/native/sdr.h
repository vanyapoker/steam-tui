#pragma once
#include "sdr_ticket_abi.h"
#include "steam_tui_sdr.pb.h"
#include <array>
#include <cstddef>
#include <cstdint>
#include <string>
#include <vector>

namespace steam_tui_relay {
constexpr size_t kMaxDatagram = 1300;
std::array<uint8_t, kMaxDatagram>
ping_request(uint32_t timestamp, uint32_t cookie, uint32_t revision,
             uint32_t ipv4, uint16_t port, bool small = false);
bool ping_reply(const void *data, size_t size, uint32_t timestamp,
                uint32_t cookie,
                steam_tui::sdr::CMsgSteamDatagramRouterPingReply &reply);
void configure(uint32_t revision);
bool add_relay(uint32_t pop, uint32_t ipv4, uint16_t port);
void pump();
int ping(uint32_t pop);
float ping_age();
int routed_ping(uint32_t pop, uint32_t *via);
// Signed ticket validation is separate from connection creation. No credentials
// leave the game process, and no availability is implied by accepting a ticket.
bool receive_ticket(const void *data, int size, uint32_t app, uint64_t client,
                    SteamDatagramRelayAuthTicket *out = nullptr);
int find_ticket(const char *server, int port, std::string *wire = nullptr,
                SteamDatagramRelayAuthTicket *out = nullptr);
HSteamNetConnection connect_hosted(const SteamNetworkingIdentity &server,
                                   int port, int options,
                                   const SteamNetworkingConfigValue_t *values);
} // namespace steam_tui_relay
