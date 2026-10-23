//! Read-only probe of the matchmaking servers: lists an app's lobbies, then
//! requests one lobby's data and prints both raw metadata blobs. Never creates,
//! joins or edits a lobby.
//! Usage: lobby-probe ACCOUNT APP [KEY=VALUE ...]
use std::error::Error;
use std::time::Duration;

use futures_util::StreamExt;
use steam_core::account;
use steam_vent::ConnectionTrait;
use steam_vent_proto_steam::steammessages_base::CMsgIPAddress;
use steam_vent_proto_steam::steammessages_clientserver_mms::{
    CMsgClientMMSGetLobbyData, CMsgClientMMSGetLobbyList, CMsgClientMMSGetLobbyListResponse,
    CMsgClientMMSLobbyData, cmsg_client_mmsget_lobby_list::Filter,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let usage = "Usage: lobby-probe ACCOUNT APP [KEY=VALUE ...]";
    let account = args.next().ok_or(usage)?;
    let app: u32 = args.next().ok_or(usage)?.parse()?;
    let filters: Vec<Filter> = args
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            Some(Filter {
                key: Some(key.to_owned()),
                value: Some(value.to_owned()),
                comparision: Some(0),
                filter_type: Some(0),
                ..Default::default()
            })
        })
        .collect();
    let connection = account::resume(&account).await?.with_app_id(app);
    let mut ip = CMsgIPAddress::new();
    match connection.public_ip() {
        std::net::IpAddr::V4(v4) => ip.set_v4(u32::from(v4)),
        std::net::IpAddr::V6(v6) => ip.set_v6(v6.octets().to_vec()),
    }
    let list = connection
        .job::<_, CMsgClientMMSGetLobbyListResponse>(CMsgClientMMSGetLobbyList {
            app_id: Some(app),
            num_lobbies_requested: Some(5),
            cell_id: Some(connection.cell_id()),
            public_ip: Some(ip).into(),
            filters,
            ..Default::default()
        })
        .await?;
    println!(
        "list: eresult {}, {} lobbies",
        list.eresult(),
        list.lobbies.len()
    );
    for lobby in &list.lobbies {
        println!(
            "lobby {} members {}/{} type {} flags {}\n  metadata {}",
            lobby.steam_id(),
            lobby.num_members(),
            lobby.max_members(),
            lobby.lobby_type(),
            lobby.lobby_flags(),
            steam_ipc::hex_encode(lobby.metadata())
        );
    }
    let Some(first) = list.lobbies.first() else {
        return Ok(());
    };
    let target = first.steam_id();
    let mut pushes = Box::pin(connection.on::<CMsgClientMMSLobbyData>());
    connection
        .send(CMsgClientMMSGetLobbyData {
            app_id: Some(app),
            steam_id_lobby: Some(target),
            ..Default::default()
        })
        .await?;
    let reply = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(message) = pushes.next().await {
            if let Ok(message) = message
                && message.steam_id_lobby() == target
            {
                return Some(message);
            }
        }
        None
    })
    .await;
    match reply {
        Ok(Some(data)) => println!(
            "data {target}: owner {} members {} (listed {}) max {} type {} flags {}\n  metadata {}",
            data.steam_id_owner(),
            data.num_members(),
            data.members.len(),
            data.max_members(),
            data.lobby_type(),
            data.lobby_flags(),
            steam_ipc::hex_encode(data.metadata())
        ),
        Ok(None) => println!("data {target}: push stream ended"),
        Err(_) => println!("data {target}: no reply in 10s"),
    }
    Ok(())
}
