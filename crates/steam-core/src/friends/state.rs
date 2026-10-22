use super::{Friend, Presence, failure};
use std::collections::{BTreeMap, BTreeSet};
use steam_vent_proto_steam::steammessages_clientserver_friends::{
    CMsgClientFriendsList, CMsgClientPersonaState,
};

pub(super) const MAX_FRIENDS: usize = 10_000;
pub(super) const STATUS: u32 = 1;
pub(super) const PLAYER_NAME: u32 = 2;
pub(super) const PRESENCE: u32 = 16;
pub(super) const GAME_EXTRA: u32 = 256;

#[derive(Default)]
struct Persona {
    name: Option<String>,
    state: Option<Presence>,
    app_id: u32,
    game_id: u64,
    game_name: Option<String>,
}

#[derive(Default)]
pub(super) struct State {
    pub complete: bool,
    owner: Option<u64>,
    friends: BTreeSet<u64>,
    personas: BTreeMap<u64, Persona>,
    pub names: BTreeMap<u32, Option<String>>,
}

fn individual(id: u64) -> bool {
    // SteamID's account type occupies bits 52..55; clans are not friends.
    (id >> 52) & 15 == 1 && id as u32 != 0
}

impl State {
    pub fn with_owner(owner: u64) -> Self {
        Self {
            owner: Some(owner),
            ..Self::default()
        }
    }

    pub fn profile(&self) -> Option<Friend> {
        let id = self.owner?;
        let name = self.personas.get(&id)?.name.clone()?;
        let mut profile = self.persona(id);
        profile.name = name;
        Some(profile)
    }

    pub fn relationships(&mut self, message: CMsgClientFriendsList) -> super::Result<Vec<u64>> {
        if message.friends.len() > MAX_FRIENDS {
            return Err(failure("Steam friend list exceeds the 10000 entry limit"));
        }
        let full = !message.bincremental.unwrap_or(false);
        let mut added = BTreeSet::new();
        if full {
            self.friends.clear();
            self.complete = true;
        }
        for friend in message.friends {
            let Some(id) = friend
                .ulfriendid
                .filter(|id| individual(*id) && Some(*id) != self.owner)
            else {
                continue;
            };
            // Only the actual Friend relationship (3), not invitations,
            // blocked users, or ignored users, belongs in the displayed list.
            if friend.efriendrelationship == Some(3) {
                if self.friends.insert(id) && !full {
                    added.insert(id);
                }
            } else {
                self.friends.remove(&id);
                self.personas.remove(&id);
            }
        }
        if self.friends.len() > MAX_FRIENDS {
            return Err(failure("Steam friend list exceeds the 10000 entry limit"));
        }
        if self.complete {
            self.personas
                .retain(|id, _| self.friends.contains(id) || Some(*id) == self.owner);
        }
        Ok(if full {
            self.ids()
        } else {
            added
                .into_iter()
                .filter(|id| self.friends.contains(id))
                .collect()
        })
    }

    pub fn personas(&mut self, message: CMsgClientPersonaState) -> super::Result<()> {
        if message.friends.len() > MAX_FRIENDS + usize::from(self.owner.is_some()) {
            return Err(failure(
                "Steam persona message exceeds the 10000 entry limit",
            ));
        }
        let flags = message.status_flags.unwrap_or(0);
        for friend in message.friends {
            let Some(id) = friend.friendid.filter(|id| individual(*id)) else {
                continue;
            };
            if self.complete && !self.friends.contains(&id) && Some(id) != self.owner {
                continue;
            }
            if !self.personas.contains_key(&id)
                && self.personas.len() >= MAX_FRIENDS + usize::from(self.owner.is_some())
            {
                return Err(failure(
                    "Steam initial persona cache exceeds the 10000 entry limit",
                ));
            }
            let persona = self.personas.entry(id).or_default();
            if friend.player_name.is_some() || flags & PLAYER_NAME != 0 {
                persona.name = friend.player_name.filter(|name| !name.is_empty());
            }
            if let Some(state) = friend.persona_state {
                persona.state = Some(Presence::from_protocol(state));
            } else if flags & PRESENCE != 0 {
                // Persona state belongs to Presence, not the Status/game group.
                // Default zero in an authoritative Presence update is Offline.
                persona.state = Some(Presence::Offline);
            }
            if friend.game_played_app_id.is_some() || flags & STATUS != 0 {
                let app_id = friend.game_played_app_id.unwrap_or(0);
                if app_id != persona.app_id {
                    persona.game_name = None;
                    persona.game_id = u64::from(app_id);
                }
                persona.app_id = app_id;
                if app_id == 0 && flags & GAME_EXTRA == 0 && friend.gameid.is_none() {
                    persona.game_id = 0;
                    persona.game_name = None;
                }
            }
            if friend.gameid.is_some() || flags & GAME_EXTRA != 0 {
                let game_id = friend.gameid.unwrap_or(0);
                if game_id != persona.game_id {
                    persona.game_name = None;
                }
                persona.game_id = game_id;
                if friend.game_played_app_id.is_none() {
                    // GameID's low 24 bits are the app ID, but only type 0 is
                    // a Steam app. Non-Steam shortcuts have type 2. Explicit
                    // zero clears the previous app as well as its game name.
                    persona.app_id = if game_id != 0 && (game_id >> 24) & 255 == 0 {
                        (game_id & 0x00ff_ffff) as u32
                    } else {
                        0
                    };
                }
            }
            if friend.game_name.is_some() || flags & GAME_EXTRA != 0 {
                persona.game_name = friend.game_name.filter(|name| !name.is_empty());
            }
            if persona.state == Some(Presence::Offline) {
                persona.app_id = 0;
                persona.game_id = 0;
                persona.game_name = None;
            }
        }
        Ok(())
    }

    pub fn ids(&self) -> Vec<u64> {
        self.friends.iter().copied().collect()
    }

    /// Once Online is confirmed, Steam sends initial persona pushes only for
    /// online friends. The full relationship roster supplies the Offline
    /// baseline; preserve any live state already received before confirmation.
    pub fn establish_social_baseline(&mut self) {
        if self.complete {
            for &id in &self.friends {
                self.personas
                    .entry(id)
                    .or_default()
                    .state
                    .get_or_insert(Presence::Offline);
            }
        }
    }

    pub fn invalidate_presence(&mut self) {
        for persona in self.personas.values_mut() {
            persona.state = None;
            persona.app_id = 0;
            persona.game_id = 0;
            persona.game_name = None;
        }
    }

    pub fn ready(&self) -> bool {
        self.complete
            && self.friends.iter().all(|id| {
                self.personas
                    .get(id)
                    .is_some_and(|p| p.name.is_some() && p.state.is_some())
            })
    }

    pub fn missing_apps(&self) -> BTreeSet<u32> {
        self.friends
            .iter()
            .chain(self.owner.iter())
            .filter_map(|id| {
                let p = self.personas.get(id)?;
                (p.app_id != 0 && p.game_name.is_none() && !self.names.contains_key(&p.app_id))
                    .then_some(p.app_id)
            })
            .collect()
    }

    pub fn snapshot(&self) -> Vec<Friend> {
        self.friends.iter().map(|&id| self.persona(id)).collect()
    }

    fn persona(&self, id: u64) -> Friend {
        let persona = self.personas.get(&id);
        Friend {
            steam_id: id,
            name: persona
                .and_then(|p| p.name.clone())
                .unwrap_or_else(|| id.to_string()),
            state: persona.and_then(|p| p.state).unwrap_or(Presence::Unknown),
            game: persona.and_then(|p| {
                p.game_name.clone().or_else(|| {
                    if p.app_id != 0 {
                        Some(
                            self.names
                                .get(&p.app_id)
                                .and_then(Clone::clone)
                                .unwrap_or_else(|| format!("App {}", p.app_id)),
                        )
                    } else if p.game_id != 0 {
                        Some("Non-Steam game".to_owned())
                    } else {
                        None
                    }
                })
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use steam_vent_proto_steam::steammessages_clientserver_friends::{
        cmsg_client_friends_list, cmsg_client_persona_state,
    };
    const A: u64 = 76561198000000001;
    const B: u64 = 76561198000000002;

    fn list(incremental: bool, entries: &[(u64, u32)]) -> CMsgClientFriendsList {
        CMsgClientFriendsList {
            bincremental: Some(incremental),
            friends: entries
                .iter()
                .map(|&(id, relationship)| cmsg_client_friends_list::Friend {
                    ulfriendid: Some(id),
                    efriendrelationship: Some(relationship),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }
    fn persona(flags: u32, friend: cmsg_client_persona_state::Friend) -> CMsgClientPersonaState {
        CMsgClientPersonaState {
            status_flags: Some(flags),
            friends: vec![friend],
            ..Default::default()
        }
    }

    #[test]
    fn own_persona_survives_roster_replacement_and_stays_out_of_friends() {
        let mut state = State::with_owner(A);
        state
            .personas(persona(
                PLAYER_NAME | PRESENCE,
                cmsg_client_persona_state::Friend {
                    friendid: Some(A),
                    player_name: Some("vanyapoker".into()),
                    persona_state: Some(1),
                    ..Default::default()
                },
            ))
            .unwrap();
        assert_eq!(state.profile().unwrap().name, "vanyapoker");
        state.relationships(list(false, &[(B, 3), (A, 3)])).unwrap();
        assert_eq!(state.ids(), vec![B]);
        assert_eq!(state.profile().unwrap().state, Presence::Online);
        state
            .personas(persona(
                PRESENCE,
                cmsg_client_persona_state::Friend {
                    friendid: Some(A),
                    persona_state: Some(3),
                    ..Default::default()
                },
            ))
            .unwrap();
        assert_eq!(state.profile().unwrap().name, "vanyapoker");
        assert_eq!(state.profile().unwrap().state, Presence::Away);
        state.relationships(list(false, &[])).unwrap();
        assert!(state.snapshot().is_empty());
        assert_eq!(state.profile().unwrap().name, "vanyapoker");
        state
            .personas(persona(
                PLAYER_NAME,
                cmsg_client_persona_state::Friend {
                    friendid: Some(A),
                    player_name: Some("New display name".into()),
                    ..Default::default()
                },
            ))
            .unwrap();
        assert_eq!(state.profile().unwrap().name, "New display name");
        state.invalidate_presence();
        assert_eq!(state.profile().unwrap().state, Presence::Unknown);
    }

    #[test]
    fn relationship_removal_and_full_replacement_are_authoritative() {
        let mut state = State::default();
        assert_eq!(
            state.relationships(list(false, &[(B, 3), (A, 3)])).unwrap(),
            vec![A, B]
        );
        state.relationships(list(true, &[(A, 0), (B, 1)])).unwrap();
        assert!(state.snapshot().is_empty());
        state.relationships(list(true, &[(B, 3)])).unwrap();
        state.relationships(list(false, &[(A, 3)])).unwrap();
        assert_eq!(state.ids(), vec![A]);
    }

    #[test]
    fn early_personas_survive_list_and_name_only_updates_preserve_game() {
        let mut state = State::default();
        state
            .personas(persona(
                STATUS | PLAYER_NAME | GAME_EXTRA,
                cmsg_client_persona_state::Friend {
                    friendid: Some(A),
                    player_name: Some("Alice".into()),
                    persona_state: Some(1),
                    game_played_app_id: Some(440),
                    gameid: Some(440),
                    game_name: Some("Team Fortress 2".into()),
                    ..Default::default()
                },
            ))
            .unwrap();
        state.relationships(list(false, &[(A, 3)])).unwrap();
        state
            .personas(persona(
                PLAYER_NAME,
                cmsg_client_persona_state::Friend {
                    friendid: Some(A),
                    player_name: Some("Alicia".into()),
                    ..Default::default()
                },
            ))
            .unwrap();
        let snapshot = state.snapshot();
        assert_eq!(snapshot[0].name, "Alicia");
        assert_eq!(snapshot[0].state, Presence::Online);
        assert_eq!(snapshot[0].game.as_deref(), Some("Team Fortress 2"));
        state
            .personas(persona(
                STATUS | GAME_EXTRA,
                cmsg_client_persona_state::Friend {
                    friendid: Some(A),
                    persona_state: Some(1),
                    ..Default::default()
                },
            ))
            .unwrap();
        assert_eq!(state.snapshot()[0].game, None);
    }

    #[test]
    fn status_game_updates_do_not_change_online_presence() {
        let mut state = State::default();
        state.relationships(list(false, &[(A, 3)])).unwrap();
        state
            .personas(persona(
                PRESENCE,
                cmsg_client_persona_state::Friend {
                    friendid: Some(A),
                    persona_state: Some(1),
                    ..Default::default()
                },
            ))
            .unwrap();
        state
            .personas(persona(
                STATUS,
                cmsg_client_persona_state::Friend {
                    friendid: Some(A),
                    game_played_app_id: Some(570),
                    ..Default::default()
                },
            ))
            .unwrap();
        let friend = &state.snapshot()[0];
        assert_eq!(friend.state, Presence::Online);
        assert_eq!(friend.game.as_deref(), Some("App 570"));
    }

    #[test]
    fn presence_default_offline_clears_a_playing_friend() {
        let mut state = State::default();
        state.relationships(list(false, &[(A, 3)])).unwrap();
        state
            .personas(persona(
                PRESENCE | STATUS,
                cmsg_client_persona_state::Friend {
                    friendid: Some(A),
                    persona_state: Some(1),
                    game_played_app_id: Some(440),
                    ..Default::default()
                },
            ))
            .unwrap();
        state
            .personas(persona(
                PRESENCE,
                cmsg_client_persona_state::Friend {
                    friendid: Some(A),
                    ..Default::default()
                },
            ))
            .unwrap();
        let friend = &state.snapshot()[0];
        assert_eq!(friend.state, Presence::Offline);
        assert_eq!(friend.game, None);
    }

    #[test]
    fn confirmed_social_baseline_preserves_live_state_and_metadata_refresh() {
        let mut state = State::default();
        state.relationships(list(false, &[(A, 3), (B, 3)])).unwrap();
        assert_eq!(state.snapshot()[1].state, Presence::Unknown);
        state
            .personas(persona(
                PRESENCE | STATUS,
                cmsg_client_persona_state::Friend {
                    friendid: Some(A),
                    persona_state: Some(1),
                    game_played_app_id: Some(440),
                    ..Default::default()
                },
            ))
            .unwrap();
        state.establish_social_baseline();
        state
            .personas(persona(
                PLAYER_NAME,
                cmsg_client_persona_state::Friend {
                    friendid: Some(A),
                    player_name: Some("Alice".into()),
                    ..Default::default()
                },
            ))
            .unwrap();
        let friends = state.snapshot();
        assert_eq!(friends[0].state, Presence::Online);
        assert_eq!(friends[0].game.as_deref(), Some("App 440"));
        assert_eq!(friends[1].state, Presence::Offline);
        state.invalidate_presence();
        assert!(
            state
                .snapshot()
                .iter()
                .all(|friend| friend.state == Presence::Unknown)
        );
        assert_eq!(Presence::from_protocol(7), Presence::Unknown);
        assert_eq!(Presence::from_protocol(99), Presence::Unknown);
    }

    #[test]
    fn non_steam_names_and_offline_game_clearing() {
        let mut state = State::default();
        state.relationships(list(false, &[(A, 3)])).unwrap();
        state
            .personas(persona(
                STATUS | GAME_EXTRA,
                cmsg_client_persona_state::Friend {
                    friendid: Some(A),
                    persona_state: Some(1),
                    gameid: Some(2 << 24),
                    game_name: Some("My non-Steam game".into()),
                    ..Default::default()
                },
            ))
            .unwrap();
        assert_eq!(
            state.snapshot()[0].game.as_deref(),
            Some("My non-Steam game")
        );
        assert!(state.missing_apps().is_empty());
        state
            .personas(persona(
                STATUS,
                cmsg_client_persona_state::Friend {
                    friendid: Some(A),
                    persona_state: Some(0),
                    ..Default::default()
                },
            ))
            .unwrap();
        assert_eq!(state.snapshot()[0].game, None);
    }

    #[test]
    fn metadata_does_not_resurrect_removed_or_changed_games() {
        let mut state = State::default();
        state.relationships(list(false, &[(A, 3)])).unwrap();
        state
            .personas(persona(
                STATUS,
                cmsg_client_persona_state::Friend {
                    friendid: Some(A),
                    persona_state: Some(1),
                    game_played_app_id: Some(440),
                    ..Default::default()
                },
            ))
            .unwrap();
        assert_eq!(state.snapshot()[0].game.as_deref(), Some("App 440"));
        state.names.insert(440, Some("Team Fortress 2".into()));
        assert_eq!(state.snapshot()[0].game.as_deref(), Some("Team Fortress 2"));
        state
            .personas(persona(
                STATUS,
                cmsg_client_persona_state::Friend {
                    friendid: Some(A),
                    persona_state: Some(1),
                    ..Default::default()
                },
            ))
            .unwrap();
        assert_eq!(state.snapshot()[0].game, None);
        state.relationships(list(true, &[(A, 0)])).unwrap();
        state
            .personas(persona(
                PLAYER_NAME,
                cmsg_client_persona_state::Friend {
                    friendid: Some(A),
                    player_name: Some("stale".into()),
                    ..Default::default()
                },
            ))
            .unwrap();
        assert!(state.snapshot().is_empty());
    }

    #[test]
    fn explicit_zero_game_id_clears_app_fallback_and_title() {
        let mut state = State::default();
        state.relationships(list(false, &[(A, 3)])).unwrap();
        state
            .personas(persona(
                STATUS | GAME_EXTRA,
                cmsg_client_persona_state::Friend {
                    friendid: Some(A),
                    persona_state: Some(1),
                    game_played_app_id: Some(440),
                    gameid: Some(440),
                    game_name: Some("Team Fortress 2".into()),
                    ..Default::default()
                },
            ))
            .unwrap();
        state
            .personas(persona(
                GAME_EXTRA,
                cmsg_client_persona_state::Friend {
                    friendid: Some(A),
                    gameid: Some(0),
                    ..Default::default()
                },
            ))
            .unwrap();
        assert_eq!(state.snapshot()[0].state, Presence::Online);
        assert_eq!(state.snapshot()[0].game, None);
    }

    #[test]
    fn lag_recovery_keeps_membership_and_names_but_invalidates_presence() {
        let mut state = State::default();
        state.relationships(list(false, &[(A, 3)])).unwrap();
        state
            .personas(persona(
                STATUS | PLAYER_NAME | GAME_EXTRA,
                cmsg_client_persona_state::Friend {
                    friendid: Some(A),
                    player_name: Some("Alice".into()),
                    persona_state: Some(1),
                    game_played_app_id: Some(440),
                    gameid: Some(440),
                    game_name: Some("Team Fortress 2".into()),
                    ..Default::default()
                },
            ))
            .unwrap();
        assert!(state.ready());
        state.invalidate_presence();
        let snapshot = state.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].name, "Alice");
        assert_eq!(snapshot[0].state, Presence::Unknown);
        assert_eq!(snapshot[0].game, None);
        assert!(!state.ready());
        state
            .personas(persona(
                STATUS,
                cmsg_client_persona_state::Friend {
                    friendid: Some(A),
                    persona_state: Some(0),
                    ..Default::default()
                },
            ))
            .unwrap();
        assert_eq!(state.snapshot()[0].state, Presence::Offline);
        assert!(state.ready());
    }
}
