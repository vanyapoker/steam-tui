// SteamNetworkingSockets012 adapter. Keep SDK 1.57's four-argument SendMessages ABI.
#include <steam/steamnetworkingsockets.h>
#include <steam/isteamnetworkingutils.h>
#include <steam/isteamnetworkingmessages.h>
#include <steam/steamnetworkingcustomsignaling.h>
#include "steamnetworkingsockets/clientlib/csteamnetworkingsockets.h"
#include "steamnetworkingsockets/steamnetworkingsockets_certstore.h"
#include <cstddef>
#include <cstring>
#include <ctime>
#include <cstdio>
#include <map>
#include <mutex>
#include <cstdlib>
#include "sdr.h"
using namespace SteamNetworkingSocketsLib;
static_assert(sizeof(SteamNetworkingIdentity)==136);
static_assert(sizeof(SteamNetConnectionInfo_t)==696);
static_assert(sizeof(SteamNetConnectionStatusChangedCallback_t)==704);
static_assert(offsetof(SteamNetworkingMessage_t,m_pfnRelease)==184);
static_assert(sizeof(SteamNetworkingMessage_t)==216);
static_assert(SteamNetConnectionStatusChangedCallback_t::k_iCallback==1221);
using SendSignalFn=bool(*)(uint64,const void*,int);
using StatusFn=void(*)(int,const void*,int);
static SendSignalFn send_signal;
static StatusFn status_callback;
static uint64 local_id;
static uint32 app_id;
static std::mutex listen_mutex;
static std::map<HSteamListenSocket,int> listen_ports;
static bool messages_enabled = false;
static_assert(sizeof(SteamNetworkingMessagesSessionRequest_t)==136);
static_assert(sizeof(SteamNetworkingMessagesSessionFailed_t)==696);
static void debug(ESteamNetworkingSocketsDebugOutputType,const char *message) { fprintf(stderr,"[steam-tui-network] %s\n",message); }
static ISteamNetworkingSockets *native() { return SteamNetworkingSockets(); }
static void changed(SteamNetConnectionStatusChangedCallback_t *info) {
    status_callback(1221,info,sizeof(*info));
}
static void messages_request(SteamNetworkingMessagesSessionRequest_t *info) { status_callback(1251,info,sizeof(*info)); }
static void messages_failed(SteamNetworkingMessagesSessionFailed_t *info) { status_callback(1252,info,sizeof(*info)); }
class Signaling final: public ISteamNetworkingConnectionSignaling {
    uint64 peer;
public:
    explicit Signaling(uint64 id):peer(id){}
    bool SendSignal(HSteamNetConnection, const SteamNetConnectionInfo_t&,const void *data,int len) override {
        return send_signal(peer,data,len);
    }
    void Release() override { delete this; }
};
static ISteamNetworkingConnectionSignaling *create_signaling(ISteamNetworkingSockets*,const SteamNetworkingIdentity &peer,int,int) {
    uint64 id=peer.GetSteamID64();
    return id?new Signaling(id):nullptr;
}
class Receive final: public ISteamNetworkingSignalingRecvContext {
    uint64 source;
public:
    explicit Receive(uint64 id):source(id){}
    ISteamNetworkingConnectionSignaling *OnConnectRequest(HSteamNetConnection,const SteamNetworkingIdentity &peer,int port) override {
        std::lock_guard<std::mutex> lock(listen_mutex);
        bool listening=messages_enabled && port==k_nVirtualPort_Messages;
        for(const auto &entry:listen_ports) if(entry.second==port) listening=true;
        return listening && peer.GetSteamID64()==source?new Signaling(source):nullptr;
    }
    void SendRejectionSignal(const SteamNetworkingIdentity &peer,const void *msg,int size) override {
        if(peer.GetSteamID64()==source) send_signal(source,msg,size);
    }
};
static bool set_cert(const void *data,int size,SteamNetworkingErrMsg &err) {
    SteamNetworkingGlobalLock lock("steam-tui certificate");
    CMsgSteamDatagramCertificateSigned signed_cert;
    CMsgSteamDatagramCertificate cert;
    if(size<=0 || !signed_cert.ParseFromArray(data,size)) { strcpy(err,"Malformed signed certificate"); return false; }
    const auto *scope=CertStore_CheckCert(signed_cert,cert,time(nullptr),err);
    if(!scope || !CheckCertAppID(cert,scope,app_id,err)) return false;
    SteamNetworkingIdentity identity;
    if(SteamNetworkingIdentityFromCert(identity,cert,err)<0 || identity.GetSteamID64()!=local_id) {
        strcpy(err,"Certificate identity mismatch"); return false;
    }
    return native()->SetCertificate(data,size,err);
}
extern "C" void steam_tui_networking_pump();
// No virtual destructor: slot zero must remain CreateListenSocketIP.
class LegacySockets {
public:
    virtual HSteamListenSocket CreateListenSocketIP(const SteamNetworkingIPAddr &localAddress, int nOptions, const SteamNetworkingConfigValue_t *pOptions) { return native()->CreateListenSocketIP(localAddress, nOptions, pOptions); }
    virtual HSteamNetConnection ConnectByIPAddress(const SteamNetworkingIPAddr &address, int nOptions, const SteamNetworkingConfigValue_t *pOptions) { return native()->ConnectByIPAddress(address, nOptions, pOptions); }
    virtual HSteamListenSocket CreateListenSocketP2P(int nLocalVirtualPort, int nOptions, const SteamNetworkingConfigValue_t *pOptions) { auto handle=native()->CreateListenSocketP2P(nLocalVirtualPort, nOptions, pOptions);
        if(handle) { std::lock_guard<std::mutex> lock(listen_mutex); listen_ports[handle]=nLocalVirtualPort; }
        fprintf(stderr,"[steam-tui-network] CreateListenSocketP2P port=%d handle=%u\n",nLocalVirtualPort,handle);
        return handle; }
    virtual HSteamNetConnection ConnectP2P(const SteamNetworkingIdentity &identityRemote, int nRemoteVirtualPort, int nOptions, const SteamNetworkingConfigValue_t *pOptions) { auto handle=native()->ConnectP2P(identityRemote, nRemoteVirtualPort, nOptions, pOptions);
        fprintf(stderr,"[steam-tui-network] ConnectP2P port=%d handle=%u\n",nRemoteVirtualPort,handle);
        return handle; }
    virtual EResult AcceptConnection(HSteamNetConnection hConn) { return native()->AcceptConnection(hConn); }
    virtual bool CloseConnection(HSteamNetConnection hPeer, int nReason, const char *pszDebug, bool bEnableLinger) { return native()->CloseConnection(hPeer, nReason, pszDebug, bEnableLinger); }
    virtual bool CloseListenSocket(HSteamListenSocket hSocket) { bool closed=native()->CloseListenSocket(hSocket);
        if(closed) { std::lock_guard<std::mutex> lock(listen_mutex); listen_ports.erase(hSocket); }
        return closed; }
    virtual bool SetConnectionUserData(HSteamNetConnection hPeer, int64 nUserData) { return native()->SetConnectionUserData(hPeer, nUserData); }
    virtual int64 GetConnectionUserData(HSteamNetConnection hPeer) { return native()->GetConnectionUserData(hPeer); }
    virtual void SetConnectionName(HSteamNetConnection hPeer, const char *pszName) { native()->SetConnectionName(hPeer, pszName); }
    virtual bool GetConnectionName(HSteamNetConnection hPeer, char *pszName, int nMaxLen) { return native()->GetConnectionName(hPeer, pszName, nMaxLen); }
    virtual EResult SendMessageToConnection(HSteamNetConnection hConn, const void *pData, uint32 cbData, int nSendFlags, int64 *pOutMessageNumber) { return native()->SendMessageToConnection(hConn, pData, cbData, nSendFlags, pOutMessageNumber); }
    virtual void SendMessages(int nMessages, SteamNetworkingMessage_t **pMessages, int64 *pOutMessageNumberOrResult) { native()->SendMessages(nMessages, pMessages, pOutMessageNumberOrResult, true); }
    virtual EResult FlushMessagesOnConnection(HSteamNetConnection hConn) { return native()->FlushMessagesOnConnection(hConn); }
    virtual int ReceiveMessagesOnConnection(HSteamNetConnection hConn, SteamNetworkingMessage_t **ppOutMessages, int nMaxMessages) { return native()->ReceiveMessagesOnConnection(hConn, ppOutMessages, nMaxMessages); }
    virtual bool GetConnectionInfo(HSteamNetConnection hConn, SteamNetConnectionInfo_t *pInfo) { return native()->GetConnectionInfo(hConn, pInfo); }
    virtual EResult GetConnectionRealTimeStatus(HSteamNetConnection hConn, SteamNetConnectionRealTimeStatus_t *pStatus, int nLanes, SteamNetConnectionRealTimeLaneStatus_t *pLanes) { return native()->GetConnectionRealTimeStatus(hConn, pStatus, nLanes, pLanes); }
    virtual int GetDetailedConnectionStatus(HSteamNetConnection hConn, char *pszBuf, int cbBuf) { return native()->GetDetailedConnectionStatus(hConn, pszBuf, cbBuf); }
    virtual bool GetListenSocketAddress(HSteamListenSocket hSocket, SteamNetworkingIPAddr *address) { return native()->GetListenSocketAddress(hSocket, address); }
    virtual bool CreateSocketPair(HSteamNetConnection *pOutConnection1, HSteamNetConnection *pOutConnection2, bool bUseNetworkLoopback, const SteamNetworkingIdentity *pPeerIdentity1, const SteamNetworkingIdentity *pPeerIdentity2) { return native()->CreateSocketPair(pOutConnection1, pOutConnection2, bUseNetworkLoopback, pPeerIdentity1, pPeerIdentity2); }
    virtual EResult ConfigureConnectionLanes(HSteamNetConnection hConn, int nNumLanes, const int *pLanePriorities, const uint16 *pLaneWeights) { return native()->ConfigureConnectionLanes(hConn, nNumLanes, pLanePriorities, pLaneWeights); }
    virtual bool GetIdentity(SteamNetworkingIdentity *pIdentity) { return native()->GetIdentity(pIdentity); }
    virtual ESteamNetworkingAvailability InitAuthentication() { return k_ESteamNetworkingAvailability_CannotTry; }
    virtual ESteamNetworkingAvailability GetAuthenticationStatus(SteamNetAuthenticationStatus_t *pDetails) { if(pDetails) { memset(pDetails,0,sizeof(*pDetails)); pDetails->m_eAvail=k_ESteamNetworkingAvailability_CannotTry; strcpy(pDetails->m_debugMsg,"Hosted SDR interoperability pending verification"); } return k_ESteamNetworkingAvailability_CannotTry; }
    virtual HSteamNetPollGroup CreatePollGroup() { return native()->CreatePollGroup(); }
    virtual bool DestroyPollGroup(HSteamNetPollGroup hPollGroup) { return native()->DestroyPollGroup(hPollGroup); }
    virtual bool SetConnectionPollGroup(HSteamNetConnection hConn, HSteamNetPollGroup hPollGroup) { return native()->SetConnectionPollGroup(hConn, hPollGroup); }
    virtual int ReceiveMessagesOnPollGroup(HSteamNetPollGroup hPollGroup, SteamNetworkingMessage_t **ppOutMessages, int nMaxMessages) { return native()->ReceiveMessagesOnPollGroup(hPollGroup, ppOutMessages, nMaxMessages); }
    virtual bool ReceivedRelayAuthTicket(const void *pvTicket, int cbTicket, SteamDatagramRelayAuthTicket *pOutParsedTicket) { return steam_tui_relay::receive_ticket(pvTicket,cbTicket,app_id,local_id,pOutParsedTicket); }
    virtual int FindRelayAuthTicketForServer(const SteamNetworkingIdentity &identityGameServer, int nRemoteVirtualPort, SteamDatagramRelayAuthTicket *pOutParsedTicket) { char server[128]; identityGameServer.ToString(server,sizeof(server)); return steam_tui_relay::find_ticket(server,nRemoteVirtualPort,nullptr,pOutParsedTicket); }
    virtual HSteamNetConnection ConnectToHostedDedicatedServer(const SteamNetworkingIdentity &identityTarget, int nRemoteVirtualPort, int nOptions, const SteamNetworkingConfigValue_t *pOptions) { return steam_tui_relay::connect_hosted(identityTarget,nRemoteVirtualPort,nOptions,pOptions); }
    virtual uint16 GetHostedDedicatedServerPort() { return native()->GetHostedDedicatedServerPort(); }
    virtual SteamNetworkingPOPID GetHostedDedicatedServerPOPID() { return native()->GetHostedDedicatedServerPOPID(); }
    virtual EResult GetHostedDedicatedServerAddress(SteamDatagramHostedAddress *pRouting) { return native()->GetHostedDedicatedServerAddress(pRouting); }
    virtual HSteamListenSocket CreateHostedDedicatedServerListenSocket(int nLocalVirtualPort, int nOptions, const SteamNetworkingConfigValue_t *pOptions) { return native()->CreateHostedDedicatedServerListenSocket(nLocalVirtualPort, nOptions, pOptions); }
    virtual EResult GetGameCoordinatorServerLogin(SteamDatagramGameCoordinatorServerLogin *pLoginInfo, int *pcbSignedBlob, void *pBlob) { return native()->GetGameCoordinatorServerLogin(pLoginInfo, pcbSignedBlob, pBlob); }
    virtual HSteamNetConnection ConnectP2PCustomSignaling(ISteamNetworkingConnectionSignaling *pSignaling, const SteamNetworkingIdentity *pPeerIdentity, int nRemoteVirtualPort, int nOptions, const SteamNetworkingConfigValue_t *pOptions) { return native()->ConnectP2PCustomSignaling(pSignaling, pPeerIdentity, nRemoteVirtualPort, nOptions, pOptions); }
    virtual bool ReceivedP2PCustomSignal(const void *pMsg, int cbMsg, ISteamNetworkingSignalingRecvContext *pContext) { return native()->ReceivedP2PCustomSignal(pMsg, cbMsg, pContext); }
    virtual bool GetCertificateRequest(int *pcbBlob, void *pBlob, SteamNetworkingErrMsg &errMsg) { return native()->GetCertificateRequest(pcbBlob, pBlob, errMsg); }
    virtual bool SetCertificate(const void *pCertificate, int cbCertificate, SteamNetworkingErrMsg &errMsg) { return set_cert(pCertificate,cbCertificate,errMsg); }
    virtual void ResetIdentity(const SteamNetworkingIdentity *pIdentity) { native()->ResetIdentity(pIdentity); }
    virtual void RunCallbacks() { steam_tui_networking_pump(); }
    virtual bool BeginAsyncRequestFakeIP(int nNumPorts) { return native()->BeginAsyncRequestFakeIP(nNumPorts); }
    virtual void GetFakeIP(int idxFirstPort, SteamNetworkingFakeIPResult_t *pInfo) { native()->GetFakeIP(idxFirstPort, pInfo); }
    virtual HSteamListenSocket CreateListenSocketP2PFakeIP(int idxFakePort, int nOptions, const SteamNetworkingConfigValue_t *pOptions) { return native()->CreateListenSocketP2PFakeIP(idxFakePort, nOptions, pOptions); }
    virtual EResult GetRemoteFakeIPForConnection(HSteamNetConnection hConn, SteamNetworkingIPAddr *pOutAddr) { return native()->GetRemoteFakeIPForConnection(hConn, pOutAddr); }
    virtual ISteamNetworkingFakeUDPPort* CreateFakeUDPPort(int idxFakeServerPort) { return native()->CreateFakeUDPPort(idxFakeServerPort); }
};
static LegacySockets legacy;
extern "C" {
void *steam_tui_transport_init(uint64 id,uint32 app,SendSignalFn send,StatusFn status,char *error) {
    SteamNetworkingIdentity identity; identity.Clear();
    if(id) identity.SetSteamID64(id); else identity.SetLocalHost();
    SteamNetworkingErrMsg err{};
    if(!GameNetworkingSockets_Init(&identity,err)) { memcpy(error,err,sizeof(err)); return nullptr; }
    local_id=id; app_id=app; send_signal=send; status_callback=status;
    static_cast<CSteamNetworkingUtils*>(SteamNetworkingUtils())->SetAppID(app);
    auto *utils=SteamNetworkingUtils();
    utils->SetDebugOutputFunction(k_ESteamNetworkingSocketsDebugOutputType_Warning,debug);
    utils->SetGlobalCallback_SteamNetConnectionStatusChanged(changed);
    utils->SetGlobalCallback_MessagesSessionRequest(messages_request);
    utils->SetGlobalCallback_MessagesSessionFailed(messages_failed);
    utils->SetGlobalConfigValuePtr(k_ESteamNetworkingConfig_Callback_CreateConnectionSignaling,(void*)create_signaling);
    if(const char *force=getenv("STEAM_TUI_FORCE_RELAY"); force && !strcmp(force,"1"))
        utils->SetGlobalConfigValueInt32(k_ESteamNetworkingConfig_P2P_Transport_ICE_Enable,0);
    // Native ICE has no default STUN service. Caller configures one explicitly.
    return &legacy;
}
void *steam_tui_transport_messages() {
    auto *messages = SteamNetworkingMessages();
    std::lock_guard<std::mutex> lock(listen_mutex);
    messages_enabled = messages != nullptr;
    return messages;
}
bool steam_tui_transport_public_key(unsigned char *key) {
    SteamNetworkingErrMsg err{}; unsigned char buf[4096]; int size=sizeof(buf);
    if(!native()->GetCertificateRequest(&size,buf,err)) return false;
    CMsgSteamDatagramCertificateRequest request;
    if(!request.ParseFromArray(buf,size) || request.cert().key_data().size()!=32) return false;
    memcpy(key,request.cert().key_data().data(),32); return true;
}
bool steam_tui_transport_add_ca(const char *cert,char *error) {
    SteamNetworkingGlobalLock lock("steam-tui CA");
    SteamNetworkingErrMsg err{}; bool ok=CertStore_AddCertFromBase64(cert,err);
    if(!ok) memcpy(error,err,sizeof(err)); return ok;
}
void steam_tui_transport_revoke(uint64 key) {
    SteamNetworkingGlobalLock lock("steam-tui revoke"); CertStore_AddKeyRevocation(key);
}
bool steam_tui_transport_set_cert(const void *data,int size,char *error) {
    SteamNetworkingErrMsg err{}; bool ok=set_cert(data,size,err);
    if(!ok) memcpy(error,err,sizeof(err)); return ok;
}
void steam_tui_transport_recv(uint64 peer,const void *data,int size) {
    Receive context(peer); native()->ReceivedP2PCustomSignal(data,size,&context);
}
void steam_tui_transport_callbacks() { steam_tui_relay::pump(); native()->RunCallbacks(); }
void steam_tui_sdr_configure(uint32 revision) { steam_tui_relay::configure(revision); }
bool steam_tui_sdr_add_relay(uint32 pop,uint32 ip,uint16 port) { return steam_tui_relay::add_relay(pop,ip,port); }
float steam_tui_sdr_ping_age() { steam_tui_relay::pump(); return steam_tui_relay::ping_age(); }
int steam_tui_sdr_ping(uint32 pop) { steam_tui_relay::pump(); return steam_tui_relay::ping(pop); }
int steam_tui_sdr_routed_ping(uint32 pop,uint32 *via) { steam_tui_relay::pump(); return steam_tui_relay::routed_ping(pop,via); }
bool steam_tui_transport_config(int value,int scope,intptr_t object,int type,const void *data) {
    void *default_callback=(void*)changed;
    if(value==201 && scope==1 && type==5 && (!data || !*(void*const*)data)) data=&default_callback;
    return SteamNetworkingUtils()->SetConfigValue((ESteamNetworkingConfigValue)value,(ESteamNetworkingConfigScope)scope,object,(ESteamNetworkingConfigDataType)type,data);
}
int steam_tui_transport_get_config(int value,int scope,intptr_t object,int *type,void *data,size_t *size) {
    return SteamNetworkingUtils()->GetConfigValue((ESteamNetworkingConfigValue)value,(ESteamNetworkingConfigScope)scope,object,(ESteamNetworkingConfigDataType*)type,data,size);
}
int64 steam_tui_transport_timestamp() { return SteamNetworkingUtils()->GetLocalTimestamp(); }
void steam_tui_transport_debug(int level,void *callback) {
    SteamNetworkingUtils()->SetDebugOutputFunction((ESteamNetworkingSocketsDebugOutputType)level,(FSteamNetworkingSocketsDebugOutput)callback);
}
void *steam_tui_transport_allocate(int size) { return SteamNetworkingUtils()->AllocateMessage(size); }
void steam_tui_transport_stun(const char *servers) {
    SteamNetworkingUtils()->SetGlobalConfigValueString(k_ESteamNetworkingConfig_P2P_STUN_ServerList,servers);
}
}
