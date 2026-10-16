// Compile against Valve's public SDK headers; no SDK library or Steam needed.
// c++ -std=c++17 -I/path/to/src/public tests/abi.cpp -ldl -o /tmp/client-abi
// /tmp/client-abi /absolute/path/to/libsteamclient.so
#include <steam/steam_api.h>
#include <steam/isteamtimeline.h>
#include <cassert>
#include <cstdlib>
#include <dlfcn.h>
#include <iostream>
#include <sys/resource.h>
#include <sys/wait.h>
#include <unistd.h>

static_assert(sizeof(GetTicketForWebApiResponse_t) == 2572);
static_assert(GetTicketForWebApiResponse_t::k_iCallback == 168);

int main(int argc, char **argv) {
    assert(argc == 2);
    setenv("SteamAppId", "440", 1);
    auto library = dlopen(argv[1], RTLD_NOW | RTLD_LOCAL);
    if (!library) { std::cerr << dlerror() << '\n'; return 1; }
    auto factory = reinterpret_cast<void *(*)(const char *, int *)>(dlsym(library, "CreateInterface"));
    assert(factory);
    int result = -1;
    assert(!factory(nullptr, &result) && result == 1);
    assert(factory("SteamClient017", &result) && result == 0); // Proton bootstrap
    assert(factory("SteamClient021", &result) && result == 0);
    assert(factory("SteamClient023", &result) && result == 0);
    assert(!factory("SteamClient022", &result) && result == 1);
    auto client = static_cast<ISteamClient *>(factory("SteamClient020", &result));
    assert(client && result == 0);
    assert(client->BShutdownIfAllPipesClosed());
    assert(client->ConnectToGlobalUser(-1) == 0);
    auto pipe = client->CreateSteamPipe();
    auto other = client->CreateSteamPipe();
    assert(pipe > 0 && other > pipe);
    assert(!client->BShutdownIfAllPipesClosed());
    assert(!client->GetISteamUser(1, pipe, "SteamUser021"));
    auto user = client->ConnectToGlobalUser(pipe);
    assert(user == 1);
    auto identity = client->GetISteamUser(user, pipe, "SteamUser021");
    assert(identity && identity->GetHSteamUser() == user);
    assert(!identity->BLoggedOn());
    assert(identity->GetSteamID().ConvertToUint64() == 0);
    auto user23 = client->GetISteamUser(user, pipe, "SteamUser023");
    assert(user23 && user23 != identity && user23->GetHSteamUser() == user);
    assert(!client->GetISteamUser(user, pipe, "SteamUser024"));
    assert(client->GetISteamUser(user, pipe, "SteamUser022") == identity);
    // Newer interface versions (SDK 1.50+) resolve through the generic getter;
    // unknown or unevidenced versions stay unsupported.
    for (auto name : {"SteamUtils011", "SteamFriends018", "STEAMAPPS_INTERFACE_VERSION009",
                      "STEAMUSERSTATS_INTERFACE_VERSION013", "STEAMREMOTESTORAGE_INTERFACE_VERSION014",
                      "STEAMUGC_INTERFACE_VERSION018", "STEAMUGC_INTERFACE_VERSION020",
                      "STEAMUGC_INTERFACE_VERSION021", "SteamInput005", "SteamInput007",
                      "STEAMREMOTEPLAY_INTERFACE_VERSION002", "STEAMREMOTEPLAY_INTERFACE_VERSION003",
                      "STEAMREMOTEPLAY_INTERFACE_VERSION004", "SteamMatchMakingServers003",
                      "STEAMTIMELINE_INTERFACE_V004", "SteamNetworkingSockets013"})
        assert(client->GetISteamGenericInterface(user, pipe, name));
    for (auto name : {"SteamUtils012", "SteamFriends016", "STEAMUGC_INTERFACE_VERSION019",
                      "STEAMREMOTEPLAY_INTERFACE_VERSION005", "STEAMVIDEO_INTERFACE_V003"})
        assert(!client->GetISteamGenericInterface(user, pipe, name));
    assert(!client->GetISteamUser(user, other, "SteamUser021"));
    assert(!client->GetISteamUtils(pipe, "SteamUser021"));
    auto utils = client->GetISteamUtils(pipe, "SteamUtils010");
    assert(utils && utils->GetAppID() == 440);
    assert(utils->GetConnectedUniverse() == k_EUniversePublic);
    assert(!utils->InitFilterText());
    char filtered[32] = {};
    assert(utils->FilterText(k_ETextFilteringContextChat, CSteamID(), "hello", filtered, sizeof(filtered)) == 0);
    assert(std::string(filtered) == "hello");
    char short_utf8[2] = {};
    utils->FilterText(k_ETextFilteringContextChat, CSteamID(), "\xc3\xa9", short_utf8, sizeof(short_utf8));
    assert(short_utf8[0] == '\0');
    auto apps = client->GetISteamApps(user, pipe, "STEAMAPPS_INTERFACE_VERSION008");
    assert(apps && !apps->BIsSubscribed()); // no backend, so no ownership grant
    assert(!apps->BIsSubscribedApp(570)); // service is scoped to one app
    assert(!client->GetISteamApps(user, pipe, "SteamUser021"));
    // Auth issuance must fail cleanly without a backend and clear the byte count.
    unsigned char auth_buffer[4096] = {};
    uint32 ticket_size = 999;
    assert(user23->GetAuthSessionTicket(auth_buffer, sizeof(auth_buffer), &ticket_size, nullptr) == 0);
    assert(ticket_size == 0);
    assert(user23->GetAuthTicketForWebApi(nullptr) == 0);
    assert(user23->GetAuthTicketForWebApi("service") == 0);
    // An unimplemented entry must stop with a diagnostic, never pretend to
    // return a usable result. Fork keeps this intentional trap out of the test.
    auto child = fork();
    assert(child >= 0);
    if (child == 0) {
        rlimit no_core = {0, 0};
        setrlimit(RLIMIT_CORE, &no_core);
        utils->CheckFileSignature("unimplemented-signature-check");
        _exit(99);
    }
    int status = 0;
    assert(waitpid(child, &status, 0) == child);
    assert(WIFSIGNALED(status) && WTERMSIG(status) == SIGABRT);
    auto get_callback = reinterpret_cast<bool (*)(int, void *, int *)>(
        dlsym(library, "Steam_BGetCallback"));
    auto get_result = reinterpret_cast<bool (*)(int, uint64_t, void *, int, int, bool *)>(
        dlsym(library, "Steam_GetAPICallResult"));
    assert(get_callback && get_result);
    assert(!get_callback(pipe, nullptr, nullptr));
    bool failed = false;
    assert(!get_result(pipe, 1, nullptr, 0, 0, &failed) && failed);
    assert(client->GetISteamGenericInterface(0, pipe, "SteamUtils010") == utils);
    auto friends = client->GetISteamFriends(user, pipe, "SteamFriends017");
    assert(friends && friends->GetPersonaName()[0] == '\0'); // disconnected
    assert(!client->GetISteamFriends(user, pipe, "SteamFriends016"));
    auto timeline = static_cast<ISteamTimeline *>(client->GetISteamGenericInterface(user, pipe, "STEAMTIMELINE_INTERFACE_V001"));
    assert(timeline);
    timeline->SetTimelineStateDescription("test", 0.0f);
    timeline->ClearTimelineStateDescription(0.0f);
    timeline->AddTimelineEvent("test", "test", "test", 1, 0.0f, 1.0f, static_cast<ETimelineEventClipPriority>(0));
    timeline->SetTimelineGameMode(static_cast<ETimelineGameMode>(0));
    assert(client->GetISteamRemotePlay(user, pipe, "STEAMREMOTEPLAY_INTERFACE_VERSION001"));
    client->SetWarningMessageHook(nullptr);
    assert(client->GetIPCCallCount() == 0);
    client->ReleaseUser(pipe, user);
    assert(!client->GetISteamUser(user, pipe, "SteamUser021"));
    assert(client->BReleaseSteamPipe(pipe));
    assert(!client->BReleaseSteamPipe(pipe));
    assert(!client->GetISteamUtils(pipe, "SteamUtils010"));
    assert(!client->BShutdownIfAllPipesClosed());
    assert(client->BReleaseSteamPipe(other));
    assert(client->BShutdownIfAllPipesClosed());
    dlclose(library);
    std::cout << "PASS: public SDK C++ ABI, version checks, pipe/user lifetime\n";
}
