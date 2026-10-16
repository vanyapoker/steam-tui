//! The live `CloudApi`: Steam service calls over the CM connection plus the HTTP
//! transfers they point at. Host and path come from Steam and are validated;
//! redirects are never followed; sizes are bounded. Tokens in request headers
//! are never logged.

use super::state::hex;
use super::{Batch, Changelist, CloudApi, Fetched, MAX_FILE_BYTES, RemoteFile, UploadFile};
use crate::installed::{Result, failure};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use sha1::{Digest, Sha1};
use std::time::Duration;
use steam_vent::{Connection, ConnectionTrait};
use steam_vent_proto_steam::enums::ECloudStoragePersistState;
use steam_vent_proto_steam::steammessages_cloud_steamclient::{
    CCloud_BeginAppUploadBatch_Request, CCloud_ClientBeginFileUpload_Request,
    CCloud_ClientCommitFileUpload_Request, CCloud_ClientDeleteFile_Request,
    CCloud_ClientFileDownload_Request, CCloud_CompleteAppUploadBatch_Request,
    CCloud_GetAppFileChangelist_Request, CCloud_GetClientEncryptionKey_Request,
    ClientCloudFileUploadBlockDetails,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// `EResult` values for the batch result.
const ERESULT_OK: u32 = 1;
const ERESULT_FAIL: u32 = 2;

pub struct LiveApi {
    connection: Connection,
    http: reqwest::Client,
    machine_name: String,
    client_id: u64,
}

impl LiveApi {
    async fn encryption_key(&self) -> Result<zeroize::Zeroizing<[u8; 32]>> {
        let response = self
            .call(CCloud_GetClientEncryptionKey_Request::new())
            .await?;
        let mut response = response;
        let bytes = zeroize::Zeroizing::new(response.key.take().unwrap_or_default());
        let key = zeroize::Zeroizing::new(
            <[u8; 32]>::try_from(bytes.as_slice())
                .map_err(|_| failure("invalid cloud encryption key length"))?,
        );
        if crc32fast::hash(key.as_ref()) != response.crc() as u32 {
            return Err(failure("cloud encryption key CRC mismatch"));
        }
        Ok(key)
    }
    pub fn new(connection: Connection) -> Result<Self> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .user_agent("steam-tui/cloud")
            .build()
            .map_err(|_| failure("could not initialize the HTTP client"))?;
        let machine_name: String = std::fs::read_to_string("/proc/sys/kernel/hostname")
            .unwrap_or_default()
            .trim()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            .take(64)
            .collect();
        let machine_name = if machine_name.is_empty() {
            "steam-tui".to_owned()
        } else {
            machine_name
        };
        // A stable per-machine, per-account id; it only has to be consistent.
        let digest = Sha1::digest(
            format!(
                "steam-tui:{machine_name}:{}",
                connection.steam_id().steam64()
            )
            .as_bytes(),
        );
        let client_id = u64::from_be_bytes(digest[..8].try_into().expect("8 bytes"));
        Ok(Self {
            connection,
            http,
            machine_name,
            client_id,
        })
    }

    async fn call<R: steam_vent::ServiceMethodRequest>(&self, request: R) -> Result<R::Response> {
        tokio::time::timeout(REQUEST_TIMEOUT, self.connection.service_method(request))
            .await
            .map_err(|_| failure("Steam Cloud service request timed out"))?
            .map_err(|e| failure(format!("Steam Cloud request failed: {e}")))
    }

    fn url(host: &str, path: &str, https: bool) -> Result<String> {
        let host_ok = !host.is_empty()
            && host.len() <= 253
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':'));
        let path_ok = path.starts_with('/')
            && !path
                .chars()
                .any(|c| c.is_control() || c.is_whitespace() || c == '#');
        if !host_ok || !path_ok {
            return Err(failure("Steam returned an invalid transfer location"));
        }
        Ok(format!(
            "{}://{host}{path}",
            if https { "https" } else { "http" }
        ))
    }

    /// Applies Steam's request headers; framing headers are left to the client.
    fn headers(pairs: impl IntoIterator<Item = (String, String)>) -> Result<HeaderMap> {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            if ["host", "content-length", "transfer-encoding"]
                .iter()
                .any(|skip| name.eq_ignore_ascii_case(skip))
            {
                continue;
            }
            map.append(
                HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| failure("Steam returned an invalid header name"))?,
                HeaderValue::from_str(&value)
                    .map_err(|_| failure("Steam returned an invalid header value"))?,
            );
        }
        Ok(map)
    }

    async fn send_blocks(
        &self,
        blocks: &[ClientCloudFileUploadBlockDetails],
        bytes: &[u8],
    ) -> Result<()> {
        for block in blocks {
            let url = Self::url(block.url_host(), block.url_path(), block.use_https())?;
            let method = match block.http_method() {
                1 => reqwest::Method::GET,
                3 => reqwest::Method::POST,
                4 => reqwest::Method::PUT,
                other => return Err(failure(format!("unsupported upload HTTP method {other}"))),
            };
            let body = if block.explicit_body_data().is_empty() {
                let start = usize::try_from(block.block_offset())
                    .map_err(|_| failure("upload block offset is out of range"))?;
                let end = start
                    .checked_add(block.block_length() as usize)
                    .ok_or_else(|| failure("upload block range overflows"))?;
                bytes
                    .get(start..end)
                    .ok_or_else(|| failure("upload block lies outside the file"))?
                    .to_vec()
            } else {
                block.explicit_body_data().to_vec()
            };
            let headers = Self::headers(
                block
                    .request_headers
                    .iter()
                    .map(|h| (h.name().to_owned(), h.value().to_owned())),
            )?;
            let response = self
                .http
                .request(method, url)
                .headers(headers)
                .body(body)
                .send()
                .await
                .map_err(|_| failure("the upload request failed"))?;
            if !response.status().is_success() {
                return Err(failure(format!(
                    "upload server answered HTTP {}",
                    response.status().as_u16()
                )));
            }
        }
        Ok(())
    }
}

/// `path_prefixes[index] + file_name`, inserting `/` unless the prefix already
/// ends one or is just a `%Root%` token.
pub(super) fn compose_name(prefixes: &[String], index: Option<u32>, file_name: &str) -> String {
    if file_name.starts_with('%') {
        return file_name.to_owned();
    }
    let Some(prefix) = index.and_then(|i| prefixes.get(i as usize)) else {
        return file_name.to_owned();
    };
    let bare_token = prefix.len() > 2
        && prefix.starts_with('%')
        && prefix.ends_with('%')
        && prefix[1..prefix.len() - 1]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric());
    if prefix.is_empty() || prefix.ends_with('/') || bare_token {
        format!("{prefix}{file_name}")
    } else {
        format!("{prefix}/{file_name}")
    }
}

impl CloudApi for LiveApi {
    async fn changelist(&self, app_id: u32, since: u64) -> Result<Changelist> {
        let response = self
            .call(CCloud_GetAppFileChangelist_Request {
                appid: Some(app_id),
                synced_change_number: Some(since),
                ..Default::default()
            })
            .await?;
        let files = response
            .files
            .iter()
            .map(|file| RemoteFile {
                name: compose_name(
                    &response.path_prefixes,
                    file.path_prefix_index,
                    file.file_name(),
                ),
                sha1: (file.sha_file().len() == 20).then(|| hex(file.sha_file())),
                time: file.time_stamp(),
                size: u64::from(file.raw_file_size()),
                persisted: file.persist_state.is_none_or(|state| {
                    state.enum_value()
                        == Ok(ECloudStoragePersistState::k_ECloudStoragePersistStatePersisted)
                }),
                deleted: file.persist_state.is_some_and(|state| {
                    state.enum_value()
                        == Ok(ECloudStoragePersistState::k_ECloudStoragePersistStateDeleted)
                }),
                platforms: file.platforms_to_sync,
            })
            .collect();
        Ok(Changelist {
            current: response.current_change_number(),
            delta: response.is_only_delta(),
            files,
        })
    }

    async fn fetch(&self, app_id: u32, name: &str) -> Result<Fetched> {
        let response = self
            .call(CCloud_ClientFileDownload_Request {
                appid: Some(app_id),
                filename: Some(name.to_owned()),
                ..Default::default()
            })
            .await?;
        let mut fetched = Fetched {
            file_size: u64::from(response.file_size()),
            raw_file_size: u64::from(response.raw_file_size()),
            sha1: (response.sha_file().len() == 20).then(|| hex(response.sha_file())),
            time: response.time_stamp(),
            explicit_delete: response.is_explicit_delete(),
            encrypted: response.encrypted(),
            body: Vec::new(),
        };
        if fetched.explicit_delete {
            return Ok(fetched);
        }
        if fetched.file_size > MAX_FILE_BYTES || fetched.raw_file_size > MAX_FILE_BYTES {
            return Err(failure("cloud file exceeds the size limit"));
        }
        let url = Self::url(
            response.url_host(),
            response.url_path(),
            response.use_https(),
        )?;
        let headers = Self::headers(
            response
                .request_headers
                .iter()
                .map(|h| (h.name().to_owned(), h.value().to_owned())),
        )?;
        let mut http = self
            .http
            .get(url)
            .headers(headers)
            .send()
            .await
            .map_err(|_| failure("the download request failed"))?;
        if !http.status().is_success() {
            return Err(failure(format!(
                "download server answered HTTP {}",
                http.status().as_u16()
            )));
        }
        let limit = fetched.file_size as usize;
        let mut body = Vec::with_capacity(limit);
        while let Some(part) = http
            .chunk()
            .await
            .map_err(|_| failure("the download was interrupted"))?
        {
            if body.len() + part.len() > limit {
                return Err(failure("download is larger than announced"));
            }
            body.extend_from_slice(&part);
        }
        fetched.body = if fetched.encrypted {
            let key = self.encryption_key().await?;
            decrypt_cloud(&body, key.as_slice(), crc32fast::hash(key.as_slice()))?
        } else {
            body
        };
        fetched.file_size = fetched.body.len() as u64;
        fetched.encrypted = false;
        Ok(fetched)
    }

    async fn begin_batch(
        &self,
        app_id: u32,
        upload: &[String],
        delete: &[String],
        build_id: u64,
    ) -> Result<Batch> {
        let response = self
            .call(CCloud_BeginAppUploadBatch_Request {
                appid: Some(app_id),
                machine_name: Some(self.machine_name.clone()),
                files_to_upload: upload.to_vec(),
                files_to_delete: delete.to_vec(),
                client_id: Some(self.client_id),
                app_build_id: Some(build_id),
                ..Default::default()
            })
            .await?;
        Ok(Batch {
            id: response.batch_id(),
            change_number: response.app_change_number,
        })
    }

    async fn upload(&self, file: &UploadFile<'_>) -> Result<bool> {
        let length =
            u32::try_from(file.bytes.len()).map_err(|_| failure("file is too large to upload"))?;
        let begin = self
            .call(CCloud_ClientBeginFileUpload_Request {
                appid: Some(file.app_id),
                file_size: Some(length),
                raw_file_size: Some(length),
                file_sha: Some(file.sha1.to_vec()),
                time_stamp: Some(file.time),
                filename: Some(file.name.to_owned()),
                cell_id: Some(self.connection.cell_id()),
                can_encrypt: Some(true),
                platforms_to_sync: Some(file.platforms),
                upload_batch_id: Some(file.batch_id),
                ..Default::default()
            })
            .await?;
        let transfer = async {
            if begin.encrypt_file() {
                let key = self.encryption_key().await?;
                let encrypted = steam_vent_crypto::symmetric_encrypt_without_hmac(
                    bytes::BytesMut::from(file.bytes),
                    &key,
                );
                self.send_blocks(&begin.block_requests, &encrypted).await
            } else {
                self.send_blocks(&begin.block_requests, file.bytes).await
            }
        }
        .await;
        let commit = self
            .call(CCloud_ClientCommitFileUpload_Request {
                transfer_succeeded: Some(transfer.is_ok()),
                appid: Some(file.app_id),
                file_sha: Some(file.sha1.to_vec()),
                filename: Some(file.name.to_owned()),
                ..Default::default()
            })
            .await;
        transfer?;
        Ok(commit?.file_committed())
    }

    async fn delete(&self, app_id: u32, batch_id: u64, name: &str) -> Result<()> {
        self.call(CCloud_ClientDeleteFile_Request {
            appid: Some(app_id),
            filename: Some(name.to_owned()),
            is_explicit_delete: Some(true),
            upload_batch_id: Some(batch_id),
            ..Default::default()
        })
        .await
        .map(|_| ())
    }

    async fn complete_batch(&self, app_id: u32, batch_id: u64, success: bool) -> Result<()> {
        self.call(CCloud_CompleteAppUploadBatch_Request {
            appid: Some(app_id),
            batch_id: Some(batch_id),
            batch_eresult: Some(if success { ERESULT_OK } else { ERESULT_FAIL }),
            ..Default::default()
        })
        .await
        .map(|_| ())
    }
    async fn recover_batch(&self, app_id: u32, batch_id: u64) -> Result<()> {
        let request = CCloud_CompleteAppUploadBatch_Request {
            appid: Some(app_id),
            batch_id: Some(batch_id),
            batch_eresult: Some(ERESULT_FAIL),
            ..Default::default()
        };
        match tokio::time::timeout(REQUEST_TIMEOUT, self.connection.service_method(request)).await {
            Ok(Ok(_)) => Ok(()),
            // A completed/expired batch may no longer be addressable. Never
            // interpret transport failures or permission errors as confirmation.
            Ok(Err(steam_vent::NetworkError::ApiError(
                steam_vent::EResult::InvalidParam | steam_vent::EResult::NoMatch,
            ))) => Ok(()),
            Ok(Err(error)) => Err(failure(format!("cannot recover cloud batch: {error}"))),
            Err(_) => Err(failure("cloud batch recovery timed out")),
        }
    }
}

/// Valve CRemoteStorageSyncContext::ThreadedDecryptFile -> SymmetricDecrypt:
/// AES-ECB encrypted IV, AES-CBC payload with PKCS#7 padding, no IV HMAC.
/// The caller still validates the listing's plaintext SHA1 before disk writes.
pub(super) fn decrypt_cloud(body: &[u8], key: &[u8], crc: u32) -> Result<Vec<u8>> {
    let key: &[u8; 32] = key
        .try_into()
        .map_err(|_| failure("invalid cloud encryption key length"))?;
    if crc32fast::hash(key) != crc {
        return Err(failure("cloud encryption key CRC mismatch"));
    }
    if body.len() < 32 || body.len() % 16 != 0 {
        return Err(failure("invalid encrypted cloud file length"));
    }
    steam_vent_crypto::symmetric_decrypt_without_hmac(bytes::BytesMut::from(body), key)
        .map(|bytes| bytes.to_vec())
        .map_err(|_| failure("cloud file decryption failed"))
}
