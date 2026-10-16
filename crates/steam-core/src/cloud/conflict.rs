use super::*;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SaveVersion {
    pub sha1: String,
    pub size: u64,
    pub time: u64,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Conflict {
    pub name: String,
    pub local: Option<SaveVersion>,
    pub cloud: Option<SaveVersion>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Choice {
    Local,
    Cloud,
}

pub(super) fn inspect(app: &CloudApp, state: &State, name: &str) -> Result<Conflict> {
    let target = app
        .roots
        .target(&app.ufs, name)
        .map_err(|e| failure(e.to_string()))?;
    let local = layout::locate(&target, false)?
        .map(|p| fingerprint(&p))
        .transpose()?
        .map(|(l, _)| SaveVersion {
            sha1: l.sha1,
            size: l.size,
            time: l.mtime,
        });
    let cloud = state.cloud.get(name).map(|e| SaveVersion {
        sha1: e.sha1.clone(),
        size: e.size,
        time: e.time,
    });
    Ok(Conflict {
        name: name.into(),
        local,
        cloud,
    })
}

pub async fn conflicts_game(
    connection: &Connection,
    game: &InstalledGame,
    account: &str,
    names: &[String],
) -> Result<Vec<Conflict>> {
    let app = CloudApp::for_game(game, account, connection.steam_id().account_id())?
        .ok_or_else(|| failure("cloud is not configured"))?;
    let _lock = app.lock()?;
    if crate::launch::is_running(app.app_id) {
        return Err(failure("close the game before resolving cloud saves"));
    }
    let api = LiveApi::new(connection.clone())?;
    let mut state = load_state(&app.state_path())?;
    refresh_listing(&api, &app, &mut state).await?;
    let mut conflicts = Vec::new();
    for name in names.iter().collect::<BTreeSet<_>>() {
        let item = inspect(&app, &state, name)?;
        if item.local.as_ref().map(|v| &v.sha1) != item.cloud.as_ref().map(|v| &v.sha1) {
            conflicts.push(item);
        }
    }
    Ok(conflicts)
}

pub async fn resolve_game(
    connection: &Connection,
    game: &InstalledGame,
    account: &str,
    expected: &Conflict,
    choice: Choice,
) -> Result<SyncReport> {
    let app = CloudApp::for_game(game, account, connection.steam_id().account_id())?
        .ok_or_else(|| failure("cloud is not configured"))?;
    let _lock = app.lock()?;
    if crate::launch::is_running(app.app_id) {
        return Err(failure("close the game before resolving cloud saves"));
    }
    resolve(&LiveApi::new(connection.clone())?, &app, expected, choice).await
}

pub(super) async fn resolve<A: CloudApi>(
    api: &A,
    app: &CloudApp,
    expected: &Conflict,
    choice: Choice,
) -> Result<SyncReport> {
    let path = app.state_path();
    let mut state = load_state(&path)?;
    refresh_listing(api, app, &mut state).await?;
    if inspect(app, &state, &expected.name)? != *expected {
        return Err(failure(
            "save changed since the conflict was shown; sync again to review both versions",
        ));
    }
    let name = &expected.name;
    let target = app
        .roots
        .target(&app.ufs, name)
        .map_err(|e| failure(e.to_string()))?;
    let mut report = SyncReport::default();
    let mut backups = None;
    match choice {
        Choice::Cloud => {
            let Some(entry) = state.cloud.get(name).cloned() else {
                let Some(path) = layout::locate(&target, false)? else {
                    return Err(failure("local copy disappeared"));
                };
                let (local, bytes) = fingerprint(&path)?;
                report.backups.push(app.backup(
                    &mut backups,
                    "resolved-local-deletion",
                    name,
                    &bytes,
                    local.mtime,
                )?);
                // inspect() just checked the exact local version chosen in the UI.
                fs::remove_file(&path)?;
                if let Some(parent) = path.parent() {
                    fs::File::open(parent)?.sync_all()?;
                }
                state.files.remove(name);
                state.deleted_cloud.remove(name);
                state.save(&app.state_path())?;
                report.deleted_locally.push(name.clone());
                return Ok(report);
            };
            let local = expected.local.as_ref().map(|l| Local {
                sha1: l.sha1.clone(),
                mtime: l.time,
                size: l.size,
            });
            let backup = download_one(
                api,
                app,
                name,
                &entry,
                &target,
                local.as_ref(),
                &mut backups,
            )
            .await?;
            report.backups.extend(backup);
            state.files.insert(
                name.clone(),
                FileRecord {
                    sha1: entry.sha1,
                    size: entry.size,
                    time: entry.time,
                },
            );
            // Choosing cloud also cancels an explicit local delete/forget intent.
            if !name.starts_with('%') {
                let store = steam_ipc::storage::Store::open(&app.roots.remote)?;
                let mut meta = store.metadata()?;
                meta.deleted.remove(name);
                meta.forgotten.remove(name);
                meta.dirty.remove(name);
                store.save_metadata(&meta)?;
            }
            state.save(&path)?;
            report.downloaded.push(name.clone());
        }
        Choice::Local => {
            // Preserve the remote version before the user-authorized replacement.
            if let Some(entry) = state.cloud.get(name) {
                let fetched = api.fetch(app.app_id, name).await?;
                let bytes = decode_download(&fetched, &entry.sha1)?;
                report.backups.push(app.backup(
                    &mut backups,
                    "resolved-cloud",
                    name,
                    &bytes,
                    entry.time,
                )?);
            }
            if inspect(app, &state, name)? != *expected {
                return Err(failure(
                    "local save changed while backing up the cloud copy; sync again",
                ));
            }
            let upload = expected.local.is_some();
            if let Some(local) = &expected.local {
                if local.size > MAX_UPLOAD_BYTES
                    || !within_quota(
                        &app.ufs,
                        state.cloud.len() as u64,
                        state.cloud.values().map(|e| e.size).sum(),
                        u64::from(expected.cloud.is_none()),
                        local.size,
                        expected.cloud.as_ref().map_or(0, |v| v.size),
                    )
                {
                    return Err(failure("chosen local save exceeds cloud quota"));
                }
            }
            let names = vec![name.clone()];
            let batch = api
                .begin_batch(
                    app.app_id,
                    if upload { &names } else { &[] },
                    if upload { &[] } else { &names },
                    app.build_id,
                )
                .await?;
            if !batch_matches(batch, state.change_number) {
                api.complete_batch(app.app_id, batch.id, false).await?;
                return Err(failure("cloud changed while resolving; sync again"));
            }
            state.pending_batch = Some(batch.id);
            if let Err(error) = state.save(&path) {
                api.complete_batch(app.app_id, batch.id, false).await?;
                return Err(error);
            }
            let transfer = async {
                if let Some(local) = &expected.local {
                    let file = LocalFile {
                        name: name.clone(),
                        path: layout::locate(&target, false)?
                            .ok_or_else(|| failure("local save disappeared"))?,
                        size: local.size,
                        mtime: local.time,
                    };
                    upload_one(api, app, batch, &file, &local.sha1)
                        .await
                        .and_then(|r| {
                            r.ok_or_else(|| failure("chosen local save was not committed"))
                                .map(Some)
                        })
                } else {
                    api.delete(app.app_id, batch.id, name).await.map(|_| None)
                }
            }
            .await;
            let complete = api
                .complete_batch(app.app_id, batch.id, transfer.is_ok())
                .await;
            if complete.is_ok() {
                state.pending_batch = None;
            }
            state.change_number = 0;
            if let (Ok(record), Ok(())) = (&transfer, &complete) {
                if let Some(record) = record {
                    state.files.insert(name.clone(), record.clone());
                    state.cloud.insert(
                        name.clone(),
                        CloudEntry {
                            sha1: record.sha1.clone(),
                            size: record.size,
                            time: record.time,
                        },
                    );
                    report.uploaded.push(name.clone());
                } else {
                    state.files.remove(name);
                    state.cloud.remove(name);
                    report.deleted_in_cloud.push(name.clone());
                }
            }
            if complete.is_ok() && transfer.is_ok() {
                state.deleted_cloud.remove(name);
                if upload && !name.starts_with('%') {
                    let store = steam_ipc::storage::Store::open(&app.roots.remote)?;
                    let mut meta = store.metadata()?;
                    meta.dirty.remove(name);
                    meta.deleted.remove(name);
                    meta.forgotten.remove(name);
                    store.save_metadata(&meta)?;
                }
            }
            state.save(&path)?;
            complete?;
            transfer?;
        }
    }
    Ok(report)
}
