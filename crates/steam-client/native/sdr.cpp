#include "sdr.h"
#include "crypto.h"
#include "steam_tui_auth.pb.h"
#include "steam_tui_sdr_hooks.h"
#include "steamnetworkingsockets/clientlib/csteamnetworkingsockets.h"
#include "steamnetworkingsockets/clientlib/steamnetworkingsockets_p2p.h"
#include "steamnetworkingsockets/steamnetworkingsockets_certstore.h"
#include <algorithm>
#include <arpa/inet.h>
#include <cerrno>
#include <cstdlib>
#include <cstring>
#include <ctime>
#include <map>
#include <mutex>
#include <steam/isteamnetworkingutils.h>
#include <steam/steamnetworkingcustomsignaling.h>
#include <sys/socket.h>
#include <unistd.h>

namespace steam_tui_relay {
using namespace SteamNetworkingSocketsLib;
static void put16(uint8_t *p, uint16_t x) {
  p[0] = x;
  p[1] = x >> 8;
}
static void put32(uint8_t *p, uint32_t x) {
  put16(p, x);
  put16(p + 2, x >> 16);
}
std::array<uint8_t, kMaxDatagram> ping_request(uint32_t timestamp,
                                               uint32_t cookie,
                                               uint32_t revision, uint32_t ipv4,
                                               uint16_t port, bool small) {
  std::array<uint8_t, kMaxDatagram> packet{};
  memcpy(packet.data(), "\x01\x02sdping", 8);
  if (small)
    packet[1] |= 0x80;
  put32(packet.data() + 8, timestamp);
  put32(packet.data() + 12, cookie);
  put32(packet.data() + 20, revision);
  put32(packet.data() + 24, ipv4 ^ cookie);
  put16(packet.data() + 28, port ^ uint16_t(cookie));
  put16(packet.data() + 30, 13); // Relay protocol version, binary 020a0504.
  return packet;
}
bool ping_reply(const void *data, size_t size, uint32_t timestamp,
                uint32_t cookie,
                steam_tui::sdr::CMsgSteamDatagramRouterPingReply &reply) {
  if (!data || size < 2 || size > kMaxDatagram || *(const uint8_t *)data != 2)
    return false;
  reply.Clear();
  if (!reply.ParseFromArray((const uint8_t *)data + 1, int(size - 1)))
    return false;
  return reply.has_client_timestamp() &&
         reply.client_timestamp() == timestamp && reply.has_client_cookie() &&
         reply.client_cookie() == cookie &&
         reply.latency_datacenter_ids_size() == reply.latency_ping_ms_size() &&
         reply.latency_datacenter_ids_p2p_size() ==
             reply.latency_ping_ms_p2p_size();
}
struct Relay {
  uint32_t pop, ip, cookie = 0, timestamp = 0;
  uint16_t port;
  int fd = -1, rtt = -1, attempts = 0;
  int64_t sent = 0, received = 0, next = 0;
  steam_tui::sdr::CMsgSteamDatagramRouterPingReply reply;
  Relay(uint32_t p, uint32_t i, uint16_t n) : pop(p), ip(i), port(n) {}
  ~Relay() {
    if (fd >= 0)
      close(fd);
  }
};

// Opt-in (STEAM_TUI_NET_TRACE=1) relay datagram trace, bounded per process.
static bool relay_trace(int &budget) {
  static const bool enabled = getenv("STEAM_TUI_NET_TRACE") != nullptr;
  return enabled && budget-- > 0;
}
static int recv_budget = 400, drop_budget = 200;
static void drop(uint32_t conn, const char *why, size_t size, uint8_t lead) {
  if (relay_trace(drop_budget))
    fprintf(stderr,
            "[steam-tui-network] SDR drop conn=%u lead=%02x len=%zu: %s\n",
            conn, lead, size, why);
}

static uint32_t revision;
static std::vector<std::shared_ptr<Relay>> relays;
class Transport;
static std::map<uint32_t, Transport *> transports; // GNS global lock
static const std::string *pending_hosted_ticket;
static uint16_t get16(const uint8_t *p) {
  return uint16_t(p[0]) | uint16_t(p[1]) << 8;
}
static uint32_t get32(const uint8_t *p) {
  return uint32_t(get16(p)) | uint32_t(get16(p + 2)) << 16;
}
static bool control(Relay &relay, uint8_t kind,
                    const google::protobuf::MessageLite &msg) {
  auto body = msg.SerializeAsString();
  if (body.size() + 1 > kMaxDatagram || relay.fd < 0)
    return false;
  std::string wire(1, char(kind));
  wire += body;
  return send(relay.fd, wire.data(), wire.size(), MSG_DONTWAIT) ==
         ssize_t(wire.size());
}

struct PacketContext : SendPacketContext_t {
  uint32_t flags = 0;
  PacketContext(int64_t now, const char *reason,
                EStatsReplyRequest request = k_EStatsReplyRequest_NothingToSend)
      : SendPacketContext_t(now, reason) {
    if (request == k_EStatsReplyRequest_Immediate)
      flags = 2 | 4;
    else if (request == k_EStatsReplyRequest_DelayedOK)
      flags = 2;
  }
};

class Transport final : public CConnectionTransport,
                        public CConnectionTransportP2PBase {
public:
  CSteamNetworkConnectionP2P &conn;
  std::shared_ptr<Relay> relay;
  std::string token, peer_token;
  std::string hosted_ticket;
  bool hosted_ready = false;
  uint32_t server_pop = 0;
  int64_t next_ticket_check = 0;
  int64_t created = 0;
  int64_t next_connect = 0;
  int64_t next_data_retry = 0;
  bool reported_send_error = false;
  uint32_t peer_pop = 0, peer_revision = 0, route_revision = 1;
  uint32_t acked_revision = 0;
  uint16_t relay_seq = 1;
  uint16_t last_relay_recv = 0;
  int64_t next_stats = 0, next_signal = 0;
  int64_t next_request = 0, last_received = 0;
  unsigned sent_data = 0, recv_data = 0;
  unsigned attempted_data = 0, drop_every = 0;
  unsigned session_attempts = 0;
  explicit Transport(CSteamNetworkConnectionP2P &c)
      : CConnectionTransport(c), CConnectionTransportP2PBase("SDR", this),
        conn(c) {
    if (pending_hosted_ticket)
      hosted_ticket = *pending_hosted_ticket;
    created = SteamNetworkingSockets_GetLocalTimestamp();
    if (const char *drop = getenv("STEAM_TUI_SDR_TEST_DROP_EVERY")) {
      char *end = nullptr;
      auto n = strtoul(drop, &end, 10);
      if (end && !*end && n >= 2 && n <= 10000)
        drop_every = unsigned(n);
    }
    if (!hosted_ticket.empty()) {
      steam_tui::sdr::CMsgSteamDatagramSignedRelayAuthTicket signed_ticket;
      steam_tui::sdr::CMsgSteamDatagramRelayAuthTicket ticket;
      if (signed_ticket.ParseFromString(hosted_ticket) &&
          ticket.ParseFromString(signed_ticket.ticket()) &&
          ticket.gameserver_address().size() >= 4) {
        const auto *b = (const uint8_t *)ticket.gameserver_address().data();
        server_pop = uint32_t(b[0]) << 16 | uint32_t(b[1]) << 8 |
                     uint32_t(b[2]) | uint32_t(b[3]) << 24;
      }
    }
    transports[c.m_unConnectionIDLocal] = this;
    c.m_vecAvailableTransports.push_back(this);
  }
  ~Transport() override { transports.erase(conn.m_unConnectionIDLocal); }
  bool BCanSendEndToEndData() const override {
    return relay &&
           (hosted_ticket.empty() ? !token.empty() && !peer_token.empty()
                                  : hosted_ready);
  }
  void
  P2PTransportUpdateRouteMetrics(SteamNetworkingMicroseconds now) override {
    m_routeMetrics.SetInvalid();
    if (BCanSendEndToEndData() && !m_bNeedToConfirmEndToEndConnectivity) {
      int score = m_pingEndToEnd.m_nSmoothedPing;
      if (score < 0)
        score = std::max(0, relay->rtt) * 2;
      m_routeMetrics.m_nScoreCurrent = m_routeMetrics.m_nScoreMin =
          m_routeMetrics.m_nScoreMax = score;
      m_routeMetrics.m_nBucketsValid = 1;
      m_routeMetrics.m_nTotalPenalty = 0;
      if (m_pingEndToEnd.m_nSmoothedPing >= 0)
        m_routeMetrics.m_nBucketsValid =
            m_pingEndToEnd.GetPingRangeFromRecentBuckets(
                m_routeMetrics.m_nScoreMin, m_routeMetrics.m_nScoreMax, now);
    }
  }
  bool SendDataPacket(SteamNetworkingMicroseconds now) override {
    PacketContext ctx(now, "SDR data");
    ctx.m_cbMaxEncryptedPayload =
        std::min<int>(m_connection.m_cbMTUPacketSize, kMaxDatagram) -
        std::max<int>(160, peer_token.size() + 80);
    return BCanSendEndToEndData() && conn.SNP_SendPacket(this, ctx);
  }
  int SendEncryptedDataChunk(const void *chunk, int size,
                             SendPacketContext_t &ctx) override {
    if (!BCanSendEndToEndData() || !conn.BStateIsActive() || size < 0 ||
        ctx.m_usecNow < next_data_retry)
      return 0;
    steam_tui::sdr::CMsgSteamDatagramConnectionStatsP2PClientToRouter stats;
    auto flags = static_cast<PacketContext &>(ctx).flags;
    if (conn.m_statsEndToEnd.BNeedToSendPingImmediate(ctx.m_usecNow))
      flags |= 2 | 4;
    else if (conn.m_statsEndToEnd.BNeedToSendKeepalive(ctx.m_usecNow))
      flags |= 2;
    stats.set_flags(flags | (conn.m_pTransport == this ? 0 : 16));
    stats.set_forward_target_relay_routing_token(peer_token);
    stats.set_forward_target_revision(peer_revision);
    stats.set_ack_peer_routes_revision(peer_revision);
    if (last_relay_recv)
      stats.add_ack_relay(last_relay_recv);
    // Always include routing metadata until the relay acknowledges it.
    std::string wire;
    if (hosted_ticket.empty())
      wire = stats.SerializeAsString();
    else {
      steam_tui::sdr::CMsgSteamDatagramConnectionStatsClientToRouter hosted;
      hosted.set_flags(flags);
      wire = hosted.SerializeAsString();
    }
    uint8_t packet[kMaxDatagram];
    packet[0] = hosted_ticket.empty() ? 0xa1 : 0x81;
    put16(packet + 3, relay_seq++);
    put32(packet + 5, conn.m_unConnectionIDLocal);
    uint8_t *out = SerializeVarInt(packet + 9, uint32(wire.size()));
    if (size_t(out - packet) + wire.size() + size_t(size) > sizeof(packet))
      return 0;
    put16(packet + 1, conn.m_statsEndToEnd.ConsumeSendPacketNumberAndGetWireFmt(
                          ctx.m_usecNow));
    memcpy(out, wire.data(), wire.size());
    out += wire.size();
    memcpy(out, chunk, size);
    out += size;
    auto length = out - packet;
    if (flags & 2)
      conn.m_statsEndToEnd.TrackSentMessageExpectingSeqNumAck(ctx.m_usecNow,
                                                              (flags & 4) == 0);
    conn.m_statsEndToEnd.TrackSentPacket(int(length));
    // Explicit fault injection used only by the relay retransmission probe.
    if (drop_every && ++attempted_data % drop_every == 0)
      return int(length);
    bool sent = send(relay->fd, packet, length, MSG_DONTWAIT) == length;
    if (!sent) {
      // A failed nonblocking UDP send must not burn sequence numbers in a
      // tight SNP retry loop. Bound sequence consumption while UDP is blocked.
      next_data_retry = ctx.m_usecNow + 20'000;
      if (!reported_send_error) {
        fprintf(stderr, "[steam-tui-network] SDR UDP send failed errno=%d\n",
                errno);
        reported_send_error = true;
      }
    }
    if (sent && sent_data++ == 0)
      fprintf(stderr,
              "[steam-tui-network] SDR first encrypted packet sent conn=%u\n",
              conn.m_unConnectionIDLocal);
    return sent ? int(length) : 0;
  }
  void SendEndToEndStatsMsg(EStatsReplyRequest request,
                            SteamNetworkingMicroseconds now,
                            const char *reason) override {
    if (!BCanSendEndToEndData())
      return;
    PacketContext ctx(now, reason, request);
    ctx.m_cbMaxEncryptedPayload =
        std::min<int>(m_connection.m_cbMTUPacketSize, kMaxDatagram) -
        std::max<int>(160, peer_token.size() + 80);
    if (conn.SNP_SendPacket(this, ctx) &&
        request != k_EStatsReplyRequest_NothingToSend)
      P2PTransportTrackSentEndToEndPingRequest(
          now, request != k_EStatsReplyRequest_Immediate);
  }
  void TransportPopulateConnectionInfo(
      SteamNetConnectionInfo_t &info) const override {
    info.m_nFlags |= k_nSteamNetworkConnectionInfoFlags_Relayed;
    if (relay)
      info.m_idPOPRelay = relay->pop;
    info.m_idPOPRemote = hosted_ticket.empty() ? peer_pop : server_pop;
  }
  void populate(CMsgSteamNetworkingP2PRendezvous &msg) {
    if (!hosted_ticket.empty())
      return;
    msg.set_ack_peer_routes_revision(peer_revision);
    if (!relay)
      return;
    steam_tui::sdr::CMsgSteamDatagramP2PRoutes routes;
    routes.set_revision(route_revision);
    auto *cluster = routes.add_relay_clusters();
    cluster->set_pop_id(relay->pop);
    cluster->set_ping_ms(std::max(0, relay->rtt));
    if (!token.empty())
      cluster->set_session_relay_routing_token(token);
    int interior = peer_pop == relay->pop ? 0 : -1;
    for (int i = 0; i < relay->reply.latency_datacenter_ids_p2p_size(); ++i)
      if (relay->reply.latency_datacenter_ids_p2p(i) == peer_pop)
        interior = relay->reply.latency_ping_ms_p2p(i);
    if (peer_pop && interior >= 0) {
      auto *route = routes.add_routes();
      route->set_my_pop_id(relay->pop);
      route->set_your_pop_id(peer_pop);
      route->set_interior_score(interior);
    }
    msg.set_sdr_routes(routes.SerializeAsString());
  }
  void receive_routes(const CMsgSteamNetworkingP2PRendezvous &msg) {
    ack_routes(msg.has_ack_peer_routes_revision(),
               msg.ack_peer_routes_revision());
    if (msg.has_sdr_routes())
      apply_routes(msg.sdr_routes());
  }
  void ack_routes(bool present, uint32_t revision) {
    if (present && revision <= route_revision)
      acked_revision = std::max(acked_revision, revision);
  }
  /// Peer routes, from signaling or forwarded by the relay in its stats.
  void apply_routes(const std::string &wire) {
    steam_tui::sdr::CMsgSteamDatagramP2PRoutes routes;
    if (wire.size() > 8192 || !routes.ParseFromString(wire) ||
        routes.relay_clusters_size() > 128 || routes.routes_size() > 512 ||
        routes.revision() < peer_revision)
      return;
    // A peer lists every relay cluster it measured; only clusters it holds a
    // session on carry a routing token, and only those can receive forwarded
    // data. Keep the cluster in use while it still has a token, else take the
    // first one that has one. Never trade a working token for an empty one:
    // that silently stops all end-to-end data.
    int chosen = -1, fallback = -1;
    for (int i = 0; i < routes.relay_clusters_size(); ++i) {
      const auto &cluster = routes.relay_clusters(i);
      if (!cluster.pop_id() || !cluster.has_ping_ms() ||
          cluster.session_relay_routing_token().size() > 256)
        continue;
      if (fallback < 0)
        fallback = i;
      if (cluster.session_relay_routing_token().empty())
        continue;
      if (chosen < 0 || cluster.pop_id() == peer_pop)
        chosen = i;
    }
    // No session token offered yet: remember the peer's preferred POP as a
    // route hint, but keep any token we already have.
    if (chosen < 0 && (fallback < 0 || !peer_token.empty())) {
      peer_revision = std::max(peer_revision, routes.revision());
      return;
    }
    const auto &cluster = routes.relay_clusters(chosen >= 0 ? chosen : fallback);
    if (peer_pop != cluster.pop_id() ||
        peer_token != cluster.session_relay_routing_token() ||
        peer_revision != routes.revision()) {
      peer_pop = cluster.pop_id();
      peer_token = cluster.session_relay_routing_token();
      peer_revision = routes.revision();
      if (relay && !token.empty())
        conn.ScheduleSendSignal("SDR peer routes");
      EnsureP2PTransportThink(k_nThinkTime_ASAP);
    }
  }
  void
  established(const steam_tui::sdr::CMsgSteamDatagramP2PSessionEstablished &msg,
              int64_t now) {
    if (msg.connection_id() != conn.m_unConnectionIDLocal ||
        msg.relay_routing_token().empty() ||
        msg.relay_routing_token().size() > 256)
      return;
    if (token != msg.relay_routing_token()) {
      token = msg.relay_routing_token();
      ++route_revision;
      conn.ScheduleSendSignal("SDR session established");
      fprintf(
          stderr,
          "[steam-tui-network] SDR P2P session established conn=%u pop=%u\n",
          conn.m_unConnectionIDLocal, relay->pop);
    }
    session_attempts = 0;
    last_received = now;
    last_relay_recv = msg.seq_num_r2c();
    EnsureP2PTransportThink(k_nThinkTime_ASAP);
  }
  void lose_connectivity(int64_t now) {
    if (!m_bNeedToConfirmEndToEndConnectivity) {
      m_bNeedToConfirmEndToEndConnectivity = true;
      conn.TransportEndToEndConnectivityChanged(this, now);
    }
  }
  void tick(int64_t now) {
    if (!conn.BStateIsActive())
      return;
    bool admitted = hosted_ticket.empty() ? !token.empty() : hosted_ready;
    if (relay &&
        ((admitted && last_received && now - last_received > 10'000'000) ||
         (!admitted && session_attempts >= 3 && now >= next_request))) {
      relay->rtt = -1;
      relay->next = now + 60'000'000;
      relay.reset();
      token.clear();
      hosted_ready = false;
      session_attempts = 0;
      last_received = 0;
      last_relay_recv = 0;
      relay_seq = 1;
      next_request = 0;
      ++route_revision;
      lose_connectivity(now);
      conn.ScheduleSendSignal("SDR relay failover");
    }
    if (hosted_ticket.empty() && relay && route_revision > acked_revision &&
        now >= next_signal) {
      conn.ScheduleSendSignal("SDR routing retry");
      next_signal = now + 1'000'000;
    }
    if (BCanSendEndToEndData() && m_bNeedToConfirmEndToEndConnectivity)
      EnsureP2PTransportThink(now);
    if (!relay && now - created >= 1'000'000) {
      int best = INT32_MAX;
      for (const auto &r : relays)
        if (r->rtt >= 0 && r->reply.has_challenge() &&
            r->reply.has_server_time()) {
          int score = r->rtt;
          if (server_pop) {
            int interior = server_pop == r->pop ? 0 : -1;
            for (int i = 0; i < r->reply.latency_datacenter_ids_size(); ++i)
              if (r->reply.latency_datacenter_ids(i) == server_pop &&
                  r->reply.latency_ping_ms(i) < 10000)
                interior = r->reply.latency_ping_ms(i);
            if (interior < 0)
              continue;
            score += interior;
          }
          if (score < best) {
            best = score;
            relay = r;
          }
        }
      if (relay) {
        ++route_revision;
        conn.ScheduleSendSignal("SDR relay selected");
      }
    }
    if (!relay)
      return;
    if (BCanSendEndToEndData() && now >= next_stats && hosted_ticket.empty()) {
      steam_tui::sdr::CMsgSteamDatagramConnectionStatsP2PClientToRouter stats;
      stats.set_connection_id(conn.m_unConnectionIDLocal);
      stats.set_seq_num_c2r(relay_seq++);
      if (last_relay_recv)
        stats.add_ack_relay(last_relay_recv);
      stats.set_forward_target_relay_routing_token(peer_token);
      stats.set_forward_target_revision(peer_revision);
      stats.set_ack_peer_routes_revision(peer_revision);
      stats.set_flags(stats.ACK_REQUEST_RELAY);
      control(*relay, 26, stats);
      next_stats = now + 1'000'000;
    }
    if (hosted_ready && now >= next_stats && !hosted_ticket.empty()) {
      steam_tui::sdr::CMsgSteamDatagramConnectionStatsClientToRouter stats;
      stats.set_client_connection_id(conn.m_unConnectionIDLocal);
      stats.set_seq_num_c2r(relay_seq++);
      if (last_relay_recv)
        stats.add_ack_relay(last_relay_recv);
      stats.set_flags(stats.ACK_REQUEST_RELAY);
      control(*relay, 13, stats);
      next_stats = now + 1'000'000;
    }
    if (!hosted_ticket.empty()) {
      if (now >= next_ticket_check) {
        char server[128];
        conn.m_identityRemote.ToString(server, sizeof(server));
        if (!find_ticket(server, conn.m_nRemoteVirtualPort)) {
          conn.ConnectionState_ProblemDetectedLocally(
              k_ESteamNetConnectionEnd_Local_Rights,
              "Relay ticket expired or revoked");
          return;
        }
        next_ticket_check = now + 1'000'000;
      }
      if (!hosted_ready && now >= next_request) {
        steam_tui::sdr::CMsgSteamDatagramGameserverSessionRequest request;
        request.set_ticket(hosted_ticket);
        request.set_challenge_time(relay->reply.server_time());
        request.set_challenge(relay->reply.challenge());
        request.set_client_connection_id(conn.m_unConnectionIDLocal);
        if (conn.m_unConnectionIDRemote)
          request.set_server_connection_id(conn.m_unConnectionIDRemote);
        request.set_network_config_version(revision);
        request.set_protocol_version(13);
        request.set_platform("linux");
        request.set_build("steam-tui");
        control(*relay, 5, request);
        ++session_attempts;
        next_request = now + 1'000'000;
      }
      if (hosted_ready &&
          conn.GetState() == k_ESteamNetworkingConnectionState_Connecting &&
          now >= next_connect && conn.GetSignedCertLocal().has_cert() &&
          conn.GetSignedCryptLocal().has_info()) {
        steam_tui::sdr::CMsgSteamDatagramConnectRequest request;
        request.set_connection_id(conn.m_unConnectionIDLocal);
        request.set_my_timestamp(now);
        request.set_virtual_port(conn.m_nRemoteVirtualPort);
        request.set_legacy_client_steam_id(conn.m_identityLocal.GetSteamID64());
        *request.mutable_cert() = conn.GetSignedCertLocal();
        *request.mutable_crypt() = conn.GetSignedCryptLocal();
        control(*relay, 18, request);
        next_connect = now + 1'000'000;
      }
      return;
    }
    if (!conn.m_unConnectionIDRemote || now < next_request || !token.empty())
      return;
    auto *sockets = conn.m_pSteamNetworkingSocketsInterface;
    if (!sockets->m_keyPrivateKey.IsValid() ||
        !sockets->m_msgSignedCert.has_cert())
      return;
    steam_tui::sdr::CMsgSteamDatagramP2PSessionRequestBody body;
    body.set_challenge_time(relay->reply.server_time());
    body.set_challenge(relay->reply.challenge());
    body.set_client_connection_id(conn.m_unConnectionIDLocal);
    body.set_peer_connection_id(conn.m_unConnectionIDRemote);
    char identity[128];
    conn.m_identityRemote.ToString(identity, sizeof(identity));
    body.set_peer_identity_string(identity);
    body.set_protocol_version(13);
    body.set_network_config_version(revision);
    body.set_platform("linux");
    body.set_build("steam-tui");
    auto wire = body.SerializeAsString();
    CryptoSignature_t signature;
    sockets->m_keyPrivateKey.GenerateSignature(wire.data(), wire.size(),
                                               &signature);
    steam_tui::sdr::CMsgSteamDatagramP2PSessionRequest request;
    *request.mutable_cert() = sockets->m_msgSignedCert;
    request.set_body(wire);
    request.set_signature(signature, sizeof(signature));
    control(*relay, 24, request);
    ++session_attempts;
    next_request = now + 1'000'000;
  }
  void receive_data(const uint8_t *packet, size_t size, int64_t now) {
    uint32_t id = conn.m_unConnectionIDLocal;
    if (!conn.BStateIsActive())
      return drop(id, "connection not active", size, packet[0]);
    if (size < 9 || get32(packet + 5) != id)
      return drop(id, "short or other connection", size, packet[0]);
    if (!BCanSendEndToEndData())
      return drop(id,
                  token.empty() ? "no session token"
                                : (peer_token.empty() ? "no peer token"
                                                      : "not admitted"),
                  size, packet[0]);
    const uint8_t *in = packet + 9, *end = packet + size;
    steam_tui::sdr::CMsgSteamDatagramConnectionStatsP2PRouterToClient stats;
    steam_tui::sdr::CMsgSteamDatagramConnectionStatsRouterToClient hosted_stats;
    if (packet[0] & 1) {
      uint32 length;
      in = DeserializeVarInt(in, end, length);
      if (!in || length > size_t(end - in))
        return drop(id, "bad stats length", size, packet[0]);
      if (hosted_ticket.empty() ? !stats.ParseFromArray(in, length)
                                : !hosted_stats.ParseFromArray(in, length))
        return drop(id, "bad stats protobuf", size, packet[0]);
      in += length;
    }
    int spacing = -1;
    // Router-to-client spacing contains end-to-end and relay spacing.
    if (packet[0] & 2) {
      if (end - in < 4)
        return drop(id, "short spacing", size, packet[0]);
      spacing = get16(in) << 4;
      in += 4;
    }
    RecvPacketContext_t ctx;
    ctx.m_usecNow = now;
    ctx.m_pTransport = this;
    // +1 is the end-to-end sequence; +3 is the relay sequence.
    if (!conn.DecryptDataChunk(get16(packet + 1), size, in, end - in, ctx))
      return drop(id, "decrypt failed", size, packet[0]);
    if (recv_data++ == 0)
      fprintf(
          stderr,
          "[steam-tui-network] SDR first encrypted packet verified conn=%u\n",
          conn.m_unConnectionIDLocal);
    if (!conn.ProcessPlainTextDataChunk(spacing, ctx))
      return drop(id, "rejected by SNP", size, packet[0]);
    last_relay_recv = get16(packet + 3);
    P2PTransportTrackRecvEndToEndPacket(now);
    P2PTransportEndToEndConnectivityConfirmed(now);
    if (hosted_ticket.empty() &&
        !(stats.flags() & stats.NOT_PRIMARY_TRANSPORT_E2E))
      conn.SetPeerSelectedTransport(this);
    last_received = now;
    if (stats.has_quality_e2e())
      conn.m_statsEndToEnd.ProcessMessage(stats.quality_e2e(), now);
    if (stats.flags() & stats.ACK_REQUEST_E2E)
      conn.QueueEndToEndAck((stats.flags() & stats.ACK_REQUEST_IMMEDIATE) != 0,
                            now);
    if (hosted_stats.has_quality_e2e())
      conn.m_statsEndToEnd.ProcessMessage(hosted_stats.quality_e2e(), now);
    if (hosted_stats.flags() & hosted_stats.ACK_REQUEST_E2E)
      conn.QueueEndToEndAck(
          (hosted_stats.flags() & hosted_stats.ACK_REQUEST_IMMEDIATE) != 0,
          now);
    uint32_t request =
        hosted_ticket.empty() ? stats.flags() : hosted_stats.flags();
    if ((request & 2) && ((request & 4) || conn.m_pTransport != this))
      SendEndToEndStatsMsg(k_EStatsReplyRequest_NothingToSend, now,
                           "SDR peer ack");
  }
  void hosted_established(
      const steam_tui::sdr::CMsgSteamDatagramGameserverSessionEstablished &msg,
      int64_t now) {
    if (hosted_ticket.empty() ||
        msg.connection_id() != conn.m_unConnectionIDLocal)
      return;
    SteamNetworkingIdentity server;
    server.Clear();
    if (msg.has_gameserver_identity_string()) {
      if (!server.ParseString(msg.gameserver_identity_string().c_str()))
        return;
    } else
      server.SetSteamID64(msg.legacy_gameserver_steamid());
    if (!(server == conn.m_identityRemote))
      return;
    hosted_ready = true;
    session_attempts = 0;
    last_received = now;
  }
  void
  hosted_connect_ok(const steam_tui::sdr::CMsgSteamDatagramConnectOK &msg) {
    if (!hosted_ready ||
        msg.client_connection_id() != conn.m_unConnectionIDLocal ||
        !msg.server_connection_id() || !msg.has_cert() || !msg.has_crypt())
      return;
    CMsgSteamNetworkingP2PRendezvous signal;
    char identity[128];
    conn.m_identityRemote.ToString(identity, sizeof(identity));
    signal.set_from_identity(identity);
    conn.m_identityLocal.ToString(identity, sizeof(identity));
    signal.set_to_identity(identity);
    signal.set_from_connection_id(msg.server_connection_id());
    signal.set_to_connection_id(conn.m_unConnectionIDLocal);
    *signal.mutable_connect_ok()->mutable_cert() = msg.cert();
    *signal.mutable_connect_ok()->mutable_crypt() = msg.crypt();
    class Context final : public ISteamNetworkingSignalingRecvContext {
      ISteamNetworkingConnectionSignaling *
      OnConnectRequest(HSteamNetConnection, const SteamNetworkingIdentity &,
                       int) override {
        return nullptr;
      }
      void SendRejectionSignal(const SteamNetworkingIdentity &, const void *,
                               int) override {}
    } context;
    auto wire = signal.SerializeAsString();
    SteamNetworkingSockets()->ReceivedP2PCustomSignal(wire.data(), wire.size(),
                                                      &context);
    EnsureP2PTransportThink(k_nThinkTime_ASAP);
  }
  void relay_control(uint8_t kind, const void *data, int size, int64_t now) {
    if (kind == 7) {
      steam_tui::sdr::CMsgSteamDatagramNoSessionRelayToClient msg;
      if (!msg.ParseFromArray(data, size) ||
          msg.connection_id() != conn.m_unConnectionIDLocal)
        return;
      if (msg.challenge() && msg.server_time()) {
        relay->reply.set_challenge(msg.challenge());
        relay->reply.set_server_time(msg.server_time());
      }
      token.clear();
      session_attempts = 0;
      hosted_ready = false;
      next_request = 0;
      ++route_revision;
      lose_connectivity(now);
      conn.ScheduleSendSignal("SDR session expired");
    } else if (kind == 13 && !hosted_ticket.empty()) {
      steam_tui::sdr::CMsgSteamDatagramConnectionStatsRouterToClient msg;
      if (!msg.ParseFromArray(data, size) ||
          msg.client_connection_id() != conn.m_unConnectionIDLocal)
        return;
      last_relay_recv = msg.seq_num_r2c();
      last_received = now;
      if (msg.flags() & msg.ACK_REQUEST_RELAY)
        next_stats = 0;
    } else if (kind == 26 || kind == 27) {
      // Relays send P2P stats to clients as kind 26, the same kind clients
      // send theirs with; Valve's client parses kind 26 from a relay as
      // CMsgSteamDatagramConnectionStatsP2PRouterToClient.
      steam_tui::sdr::CMsgSteamDatagramConnectionStatsP2PRouterToClient msg;
      if (!msg.ParseFromArray(data, size) ||
          msg.connection_id() != conn.m_unConnectionIDLocal)
        return;
      last_relay_recv = msg.seq_num_r2c();
      if (msg.flags() & msg.ACK_REQUEST_RELAY)
        next_stats = 0;
      last_received = now;
      ack_routes(msg.has_ack_peer_routes_revision(),
                 msg.ack_peer_routes_revision());
      if (!msg.routes().empty())
        apply_routes(msg.routes());
      if (!BCanSendEndToEndData() || !conn.BStateIsActive())
        return;
      // End-to-end stats forwarded from the peer carry their own sequence
      // number; record it so SNP sees no false loss and keeps the route.
      if (msg.has_seq_num_e2e()) {
        int64 number = conn.m_statsEndToEnd.ExpandWirePacketNumberAndCheck(
            uint16(msg.seq_num_e2e()), 0);
        if (number > 0) {
          conn.RecvNonDataSequencedPacket(number, now);
          P2PTransportTrackRecvEndToEndPacket(now);
          P2PTransportEndToEndConnectivityConfirmed(now);
        }
      }
      if (msg.has_quality_e2e())
        conn.m_statsEndToEnd.ProcessMessage(msg.quality_e2e(), now);
      if ((msg.flags() & msg.ACK_REQUEST_E2E) || msg.has_quality_e2e()) {
        bool immediate = (msg.flags() & msg.ACK_REQUEST_IMMEDIATE) != 0;
        conn.QueueEndToEndAck(immediate, now);
        if (immediate || conn.m_pTransport != this)
          SendEndToEndStatsMsg(k_EStatsReplyRequest_NothingToSend, now,
                               "SDR peer stats ack");
      }
    } else if (kind == 28) {
      steam_tui::sdr::CMsgSteamDatagramP2PBadRouteRouterToClient msg;
      if (!msg.ParseFromArray(data, size) ||
          msg.connection_id() != conn.m_unConnectionIDLocal ||
          msg.failed_relay_routing_token() != peer_token)
        return;
      token.clear();
      session_attempts = 0;
      next_request = now + 1'000'000;
      ++route_revision;
      lose_connectivity(now);
      conn.ScheduleSendSignal("SDR route unavailable");
    }
  }
};
void configure(uint32_t rev) {
  SteamNetworkingGlobalLock lock("steam-tui SDR");
  if (revision != rev) {
    relays.clear();
    revision = rev;
  }
}
bool add_relay(uint32_t pop, uint32_t ipv4, uint16_t port) {
  SteamNetworkingGlobalLock lock("steam-tui SDR");
  if (!pop || !ipv4 || !port || relays.size() >= 128)
    return false;
  for (const auto &r : relays)
    if (r->ip == ipv4 && r->port == port)
      return true;
  relays.emplace_back(new Relay(pop, ipv4, port));
  return true;
}
void pump() {
  SteamNetworkingGlobalLock lock("steam-tui SDR");
  int64_t now = SteamNetworkingUtils()->GetLocalTimestamp();
  int sends = 0;
  int received = 0;
  for (auto &r : relays) {
    if (r->fd >= 0) {
      uint8_t packet[kMaxDatagram + 1];
      // Connected UDP socket restricts replies to the configured endpoint.
      for (int count = 0; count < 64 && received < 128; ++count) {
        int n = recv(r->fd, packet, sizeof(packet), MSG_DONTWAIT);
        if (n < 0)
          break;
        ++received;
        if (n >= 1 && packet[0] != 2 && relay_trace(recv_budget))
          fprintf(stderr,
                  "[steam-tui-network] SDR recv pop=%u lead=%02x len=%d "
                  "conn=%u\n",
                  r->pop, packet[0], n, n >= 9 ? get32(packet + 5) : 0);
        if (n > int(kMaxDatagram))
          continue;
        if (n > 1 && (packet[0] == 7 || packet[0] == 13 || packet[0] == 26 ||
                      packet[0] == 27 || packet[0] == 28)) {
          for (auto &[id, transport] : transports)
            if (transport->relay == r) {
              ConnectionScopeLock connection(transport->conn, "SDR control");
              transport->relay_control(packet[0], packet + 1, n - 1, now);
            }
          continue;
        }
        // Relay-to-client data has the 0x40 lead-bit pattern; outbound
        // P2P client-to-relay data uses 0xa0.
        if (n > 0 && (packet[0] == 25 || packet[0] == 6 || packet[0] == 19 ||
                      (packet[0] & 0xe0) == 0x40)) {
          uint32_t id = 0;
          steam_tui::sdr::CMsgSteamDatagramP2PSessionEstablished established;
          steam_tui::sdr::CMsgSteamDatagramGameserverSessionEstablished hosted;
          steam_tui::sdr::CMsgSteamDatagramConnectOK connect_ok;
          if (packet[0] == 25 && established.ParseFromArray(packet + 1, n - 1))
            id = established.connection_id();
          else if (packet[0] == 6 && hosted.ParseFromArray(packet + 1, n - 1))
            id = hosted.connection_id();
          else if (packet[0] == 19 &&
                   connect_ok.ParseFromArray(packet + 1, n - 1))
            id = connect_ok.client_connection_id();
          else if (n >= 9)
            id = get32(packet + 5);
          auto it = transports.find(id);
          if (it != transports.end() && it->second->relay == r) {
            ConnectionScopeLock connection(it->second->conn, "SDR receive");
            if (packet[0] == 25)
              it->second->established(established, now);
            else if (packet[0] == 6)
              it->second->hosted_established(hosted, now);
            else if (packet[0] == 19)
              it->second->hosted_connect_ok(connect_ok);
            else
              it->second->receive_data(packet, n, now);
          }
          continue;
        }
        steam_tui::sdr::CMsgSteamDatagramRouterPingReply reply;
        if (r->sent && now - r->sent <= 10'000'000 &&
            ping_reply(packet, n, r->timestamp, r->cookie, reply)) {
          r->rtt = std::max<int>(1, (now - r->sent + 999) / 1000);
          r->received = now;
          r->sent = 0;
          r->attempts = 0;
          r->reply = std::move(reply);
          r->next = now + 60'000'000;
        }
      }
    }
    if (now < r->next || sends >= 4)
      continue;
    if (r->attempts >= 3) {
      r->next = now + 60'000'000;
      r->attempts = 0;
      r->rtt = -1;
      continue;
    }
    if (r->fd < 0) {
      r->fd = socket(AF_INET, SOCK_DGRAM | SOCK_CLOEXEC | SOCK_NONBLOCK, 0);
      if (r->fd < 0) {
        r->next = now + 60'000'000;
        continue;
      }
      sockaddr_in addr{};
      addr.sin_family = AF_INET;
      addr.sin_addr.s_addr = htonl(r->ip);
      addr.sin_port = htons(r->port);
      if (connect(r->fd, (sockaddr *)&addr, sizeof(addr)) < 0) {
        close(r->fd);
        r->fd = -1;
        r->next = now + 60'000'000;
        continue;
      }
    }
    CCrypto::GenerateRandomBlock(&r->cookie, sizeof(r->cookie));
    r->timestamp = uint32_t(now);
    auto request =
        ping_request(r->timestamp, r->cookie, revision, r->ip, r->port);
    r->sent = send(r->fd, request.data(), request.size(), MSG_DONTWAIT) ==
                      ssize_t(request.size())
                  ? now
                  : 0;
    r->next = now + 1'250'000;
    ++r->attempts;
    ++sends;
  }
  for (auto &[id, transport] : transports) {
    ConnectionScopeLock connection(transport->conn, "SDR session");
    transport->tick(now);
  }
}
int ping(uint32_t pop) {
  SteamNetworkingGlobalLock lock("steam-tui SDR");
  int best = -1;
  auto now = SteamNetworkingUtils()->GetLocalTimestamp();
  for (const auto &r : relays)
    if (r->pop == pop && r->rtt >= 0 && now - r->received <= 120'000'000)
      if (best < 0 || r->rtt < best)
        best = r->rtt;
  return best;
}
float ping_age() {
  SteamNetworkingGlobalLock lock("steam-tui SDR ping age");
  auto now = SteamNetworkingUtils()->GetLocalTimestamp();
  SteamNetworkingMicroseconds newest = 0;
  for (const auto &r : relays)
    if (r->rtt >= 0 && now - r->received <= 120'000'000)
      newest = std::max<SteamNetworkingMicroseconds>(newest, r->received);
  return newest ? float(now - newest) / 1'000'000.0f : -1.0f;
}
int routed_ping(uint32_t pop, uint32_t *via) {
  SteamNetworkingGlobalLock lock("steam-tui SDR ping route");
  int best = -1;
  uint32_t selected = 0;
  auto now = SteamNetworkingUtils()->GetLocalTimestamp();
  for (const auto &r : relays)
    if (r->rtt >= 0 && now - r->received <= 120'000'000) {
      if (r->pop == pop && (best < 0 || r->rtt < best)) {
        best = r->rtt;
        selected = r->pop;
      }
      for (int i = 0; i < r->reply.latency_datacenter_ids_size(); ++i) {
        auto interior = r->reply.latency_ping_ms(i);
        if (r->reply.latency_datacenter_ids(i) != pop || interior > 10'000)
          continue;
        int score = r->rtt + int(interior);
        if (best < 0 || score < best) {
          best = score;
          selected = r->pop;
        }
      }
    }
  if (via)
    *via = selected;
  return best;
}

struct Ticket {
  steam_tui::sdr::CMsgSteamDatagramRelayAuthTicket parsed;
  std::string wire;
  time_t trust_expiry;
  SteamDatagramRelayAuthTicket abi{};
};
static std::map<std::pair<std::string, int>, Ticket> tickets;
bool receive_ticket(const void *data, int size, uint32_t app, uint64_t client,
                    SteamDatagramRelayAuthTicket *out) {
  if (!data || size <= 0 || size > 4096)
    return false;
  SteamNetworkingGlobalLock global("steam-tui relay ticket");
  steam_tui::sdr::CMsgSteamDatagramSignedRelayAuthTicket signed_ticket;
  steam_tui::sdr::CMsgSteamDatagramRelayAuthTicket ticket;
  if (!signed_ticket.ParseFromArray(data, size) ||
      !ticket.ParseFromString(signed_ticket.ticket()) ||
      signed_ticket.signature().size() != 64 || signed_ticket.certs_size() > 16)
    return false;
  // Embedded CA certificates are untrusted inputs. The cert store validates
  // each chain against Valve's hardcoded root when checking the signature.
  for (const auto &cert : signed_ticket.certs()) {
    std::string base64;
    // Add through the same signed-certificate path as the public config.
    std::string wire = cert.SerializeAsString();
    base64.resize((wire.size() + 2) / 3 * 4 + 1);
    uint32 out = uint32(base64.size());
    if (!CCrypto::Base64Encode(wire.data(), uint32(wire.size()), base64.data(),
                               &out, nullptr))
      return false;
    SteamNetworkingErrMsg err{};
    if (!CertStore_AddCertFromBase64(base64.c_str(), err))
      return false;
  }
  SteamNetworkingErrMsg err{};
  auto now = time(nullptr);
  const auto *scope =
      CertStore_CheckCASignature(signed_ticket.ticket(), signed_ticket.key_id(),
                                 signed_ticket.signature(), now, err);
  if (!scope || !scope->m_apps.HasItem(app) || ticket.app_id() != app ||
      ticket.time_expiry() <= now || ticket.gameserver_address().size() < 4 ||
      ticket.gameserver_address().size() >= 128 ||
      ticket.virtual_port() > 65535 || ticket.extra_fields_size() > 16)
    return false;
  const auto *route = (const uint8_t *)ticket.gameserver_address().data();
  uint32_t pop = uint32_t(route[0]) << 16 | uint32_t(route[1]) << 8 |
                 uint32_t(route[2]) | uint32_t(route[3]) << 24;
  if (!pop || !scope->m_pops.HasItem(pop))
    return false;
  SteamNetworkingIdentity authorized, server;
  authorized.Clear();
  server.Clear();
  if (ticket.has_authorized_client_identity_string()) {
    if (!authorized.ParseString(
            ticket.authorized_client_identity_string().c_str()))
      return false;
  } else if (ticket.has_legacy_authorized_steam_id())
    authorized.SetSteamID64(ticket.legacy_authorized_steam_id());
  else
    return false;
  if (authorized.GetSteamID64() != client || !client)
    return false;
  if (ticket.has_gameserver_identity_string()) {
    if (!server.ParseString(ticket.gameserver_identity_string().c_str()))
      return false;
  } else if (ticket.has_legacy_gameserver_steam_id())
    server.SetSteamID64(ticket.legacy_gameserver_steam_id());
  else
    return false;
  if (server.IsInvalid())
    return false;
  // Do not accept IP-restricted tickets until the restriction is verified.
  if (ticket.authorized_public_ip() != 0)
    return false;
  char identity[128];
  server.ToString(identity, sizeof(identity));
  SteamDatagramRelayAuthTicket abi{};
  abi.authorized_client = authorized;
  abi.gameserver = server;
  abi.expiry = ticket.time_expiry();
  abi.routing.size = ticket.gameserver_address().size();
  memcpy(abi.routing.data, ticket.gameserver_address().data(),
         abi.routing.size);
  abi.app = app;
  abi.virtual_port =
      ticket.has_virtual_port() ? int(ticket.virtual_port()) : -1;
  for (const auto &extra : ticket.extra_fields()) {
    if (extra.name().size() >= sizeof(abi.extra[0].name) ||
        extra.string_value().size() >= 128)
      return false;
    auto &e = abi.extra[abi.extra_count++];
    memcpy(e.name, extra.name().data(), extra.name().size());
    if (extra.has_int64_value()) {
      e.type = 1;
      e.value.integer = extra.int64_value();
    } else if (extra.has_fixed64_value()) {
      e.type = 2;
      e.value.fixed = extra.fixed64_value();
    } else if (extra.has_string_value()) {
      e.type = 0;
      memcpy(e.value.text, extra.string_value().data(),
             extra.string_value().size());
    } else
      return false;
  }
  SteamNetworkingGlobalLock lock("steam-tui SDR");
  for (auto it = tickets.begin(); it != tickets.end();) {
    if (it->second.parsed.time_expiry() <= now ||
        it->second.trust_expiry <= now)
      it = tickets.erase(it);
    else
      ++it;
  }
  auto key = std::make_pair(std::string(identity), abi.virtual_port);
  if (tickets.size() >= 128 && !tickets.count(key))
    return false;
  tickets[key] = {std::move(ticket), std::string((const char *)data, size),
                  scope->m_timeExpiry, abi};
  if (out)
    *out = abi;
  return true;
}
int find_ticket(const char *server, int port, std::string *wire,
                SteamDatagramRelayAuthTicket *out) {
  SteamNetworkingGlobalLock lock("steam-tui SDR");
  auto now = time(nullptr);
  for (auto it = tickets.begin(); it != tickets.end();) {
    if (it->second.parsed.time_expiry() <= now ||
        it->second.trust_expiry <= now)
      it = tickets.erase(it);
    else
      ++it;
  }
  auto it = tickets.find({server, port});
  if (it == tickets.end() && port >= 0)
    it = tickets.find({server, -1});
  if (port == -1 && it == tickets.end()) {
    it = std::find_if(tickets.begin(), tickets.end(), [&](const auto &entry) {
      return entry.first.first == server;
    });
  }
  if (it == tickets.end())
    return 0;
  steam_tui::sdr::CMsgSteamDatagramSignedRelayAuthTicket signed_ticket;
  SteamNetworkingErrMsg err{};
  if (!signed_ticket.ParseFromString(it->second.wire) ||
      !CertStore_CheckCASignature(signed_ticket.ticket(),
                                  signed_ticket.key_id(),
                                  signed_ticket.signature(), now, err)) {
    tickets.erase(it);
    return 0;
  }
  int64_t remaining = std::min<int64_t>(it->second.parsed.time_expiry(),
                                        it->second.trust_expiry) -
                      now;
  if (remaining <= 0) {
    tickets.erase(it);
    return 0;
  }
  if (wire)
    *wire = it->second.wire;
  if (out)
    *out = it->second.abi;
  return int(std::min<int64_t>(remaining, INT32_MAX));
}
HSteamNetConnection connect_hosted(const SteamNetworkingIdentity &server,
                                   int port, int count,
                                   const SteamNetworkingConfigValue_t *values) {
  if (port < 0 || port > 65535 || count < 0 || count > 128 ||
      (count && !values))
    return 0;
  SteamNetworkingGlobalLock lock("steam-tui hosted connect");
  char identity[128];
  server.ToString(identity, sizeof(identity));
  std::string ticket;
  if (!find_ticket(identity, port, &ticket))
    return 0;
  class Signaling final : public ISteamNetworkingConnectionSignaling {
    bool SendSignal(HSteamNetConnection, const SteamNetConnectionInfo_t &,
                    const void *, int) override {
      return true;
    }
    void Release() override { delete this; }
  };
  std::vector<SteamNetworkingConfigValue_t> options;
  if (count)
    options.assign(values, values + count);
  SteamNetworkingConfigValue_t ice;
  ice.SetInt32(k_ESteamNetworkingConfig_P2P_Transport_ICE_Enable, 0);
  options.push_back(ice);
  pending_hosted_ticket = &ticket;
  auto handle = SteamNetworkingSockets()->ConnectP2PCustomSignaling(
      new Signaling, &server, port, options.size(), options.data());
  pending_hosted_ticket = nullptr;
  return handle;
}
} // namespace steam_tui_relay

namespace SteamNetworkingSocketsLib {
void SteamTuiAttachRelay(CSteamNetworkConnectionP2P &conn) {
  if (!steam_tui_relay::relays.empty())
    new steam_tui_relay::Transport(conn);
}
void SteamTuiRelayPopulate(CSteamNetworkConnectionP2P &conn,
                           CMsgSteamNetworkingP2PRendezvous &msg) {
  auto it = steam_tui_relay::transports.find(conn.m_unConnectionIDLocal);
  if (it != steam_tui_relay::transports.end())
    it->second->populate(msg);
}
void SteamTuiRelayReceive(CSteamNetworkConnectionP2P &conn,
                          const CMsgSteamNetworkingP2PRendezvous &msg) {
  auto it = steam_tui_relay::transports.find(conn.m_unConnectionIDLocal);
  if (it != steam_tui_relay::transports.end())
    it->second->receive_routes(msg);
}
} // namespace SteamNetworkingSocketsLib
