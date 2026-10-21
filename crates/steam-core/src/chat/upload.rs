//! Steam chat's authenticated begin, object upload, and recipient commit flow.
use bytes::Bytes;
use image::{DynamicImage, ImageFormat, ImageReader, Limits};
use reqwest::{
    Client,
    header::{COOKIE, HeaderMap, HeaderName, HeaderValue},
};
use serde::Deserialize;
use serde_json::Value;
use sha1::{Digest, Sha1};
use std::{
    io::{Cursor, Read},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use steam_vent::{Connection, ConnectionTrait};
use steam_vent_proto_steam::steammessages_auth_steamclient::CAuthentication_AccessToken_GenerateForApp_Request;
use tokio::time::timeout;
use zeroize::Zeroizing;

pub const MAX_IMAGE_BYTES: usize = 30 * 1024 * 1024;
const BASE: &str = "https://steamcommunity.com/chat/";

#[derive(Clone)]
pub struct Attachment {
    pub png: Bytes,
    pub image: Arc<DynamicImage>,
    pub sha: String,
}
impl std::fmt::Debug for Attachment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Attachment")
            .field("bytes", &self.png.len())
            .field("width", &self.image.width())
            .field("height", &self.image.height())
            .finish()
    }
}
impl Attachment {
    /// Normalize clipboard pixels before uploading, retaining no source filename
    /// or image metadata. Run this on a blocking worker.
    pub fn decode(bytes: Vec<u8>) -> Result<Self, String> {
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err("Clipboard image exceeds 30 MiB".into());
        }
        let mut reader = ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|_| "Clipboard does not contain a supported image")?;
        let mut limits = Limits::default();
        limits.max_image_width = Some(4096);
        limits.max_image_height = Some(4096);
        limits.max_alloc = Some(64 * 1024 * 1024);
        reader.limits(limits);
        let image = reader
            .decode()
            .map_err(|_| "Unsupported image or dimensions above 4096×4096")?;
        let mut png = Cursor::new(Vec::new());
        image
            .write_to(&mut png, ImageFormat::Png)
            .map_err(|_| "Could not encode clipboard image")?;
        let png = png.into_inner();
        if png.len() > MAX_IMAGE_BYTES {
            return Err("Encoded clipboard image exceeds 30 MiB".into());
        }
        Ok(Self {
            sha: format!("{:x}", Sha1::digest(&png)),
            png: Bytes::from(png),
            image: Arc::new(image),
        })
    }
}

#[derive(Deserialize)]
struct UploadHeader {
    name: String,
    value: String,
}
#[derive(Deserialize)]
struct Location {
    ugcid: Value,
    url_host: String,
    url_path: String,
    #[serde(deserialize_with = "upload_bool")]
    use_https: bool,
    #[serde(default)]
    request_headers: Vec<UploadHeader>,
}
#[derive(Deserialize)]
struct Begin {
    result: Location,
    timestamp: Value,
    hmac: String,
}

// The web endpoint uses PHP-style 0/1 flags while CM responses use booleans.
fn upload_bool<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    match Value::deserialize(deserializer)? {
        Value::Bool(value) => Ok(value),
        Value::Number(value) if value.as_u64() == Some(1) => Ok(true),
        Value::Number(value) if value.as_u64() == Some(0) => Ok(false),
        _ => Err(serde::de::Error::custom("expected a boolean or 0/1 flag")),
    }
}

fn rejection(stage: &str, body: &Value) -> String {
    let code = body
        .get("eresult")
        .or_else(|| body.get("success"))
        .and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
        })
        .filter(|code| *code != 1)
        .and_then(|code| i32::try_from(code).ok())
        .map(|code| match steam_vent::EResult::try_from(code) {
            Ok(result) => format!("{result:?} ({code})"),
            Err(_) => format!("error {code}"),
        });
    match code {
        Some(code) => format!("Steam {stage} rejected: {code}; attachment retained"),
        None => {
            format!("Steam {stage} response has missing or invalid fields; attachment retained")
        }
    }
}

fn parse_begin(body: Value) -> Result<Begin, String> {
    let error = rejection("begin-upload", &body);
    serde_json::from_value(body).map_err(|_| error)
}

fn scalar(value: &Value) -> Result<String, String> {
    let string = match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return Err("Invalid upload identifier from Steam".into()),
    };
    if string.is_empty() || !string.bytes().all(|b| b.is_ascii_digit()) {
        return Err("Invalid upload identifier from Steam".into());
    }
    Ok(string)
}

fn location(location: &Location) -> Result<reqwest::Url, String> {
    if !location.use_https
        || !location.url_path.starts_with('/')
        || location.url_host.contains(['/', '@', ':', '?', '#'])
    {
        return Err("Steam returned an invalid secure upload location".into());
    }
    let url = reqwest::Url::parse(&format!(
        "https://{}{}",
        location.url_host, location.url_path
    ))
    .map_err(|_| "Invalid upload URL")?;
    let host = url.host_str().unwrap_or_default();
    let permitted = [
        ".blob.core.windows.net",
        ".amazonaws.com",
        ".steamusercontent.com",
        ".steamcontent.com",
        ".steamstatic.com",
    ]
    .iter()
    .any(|suffix| host.ends_with(suffix));
    if !permitted
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("Steam returned an unsupported upload host".into());
    }
    Ok(url)
}

async fn json(mut response: reqwest::Response) -> Result<Value, String> {
    let status = response.status();
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Upload response interrupted")?
    {
        if bytes.len() + chunk.len() > 256 * 1024 {
            return Err("Steam upload response too large".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    let parsed = serde_json::from_slice(&bytes);
    if !status.is_success() {
        let reason = match parsed {
            Ok(body) => rejection("image-upload", &body),
            Err(_) => "Steam image upload failed; attachment retained".into(),
        };
        return Err(format!("{reason} (HTTP {})", status.as_u16()));
    }
    parsed.map_err(|_| "Invalid Steam upload response; attachment retained".into())
}

fn cookie(steam_id: u64, token: &str, session: &str) -> Result<HeaderValue, String> {
    let text = Zeroizing::new(format!(
        "steamLoginSecure={steam_id}%7C%7C{token}; sessionid={session}"
    ));
    let mut header =
        HeaderValue::from_str(&text).map_err(|_| "Invalid Steam web authentication")?;
    header.set_sensitive(true);
    Ok(header)
}

/// Commit delivers the image to the recipient. Never send an additional URL
/// message afterwards, and never retry an uncertain commit automatically.
pub async fn send(
    connection: &Connection,
    friend: u64,
    attachment: Attachment,
) -> Result<(), String> {
    let token = connection
        .refresh_token()
        .ok_or("Image upload requires an authenticated account")?;
    let response = timeout(
        Duration::from_secs(20),
        connection.service_method(CAuthentication_AccessToken_GenerateForApp_Request {
            refresh_token: Some(token.token().into()),
            steamid: Some(connection.steam_id().steam64()),
            ..Default::default()
        }),
    )
    .await
    .map_err(|_| "Steam web authentication timed out")?
    .map_err(|_| "Steam web authentication failed")?;
    let access = Zeroizing::new(response.access_token.unwrap_or_default());
    if access.is_empty() {
        return Err("Steam did not provide web authentication for image upload".into());
    }
    let mut random = [0u8; 12];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut random))
        .map_err(|_| "Could not create upload session")?;
    let session = random
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let cookie = cookie(connection.steam_id().steam64(), &access, &session)?;
    let client = Client::builder()
        .timeout(Duration::from_secs(45))
        .connect_timeout(Duration::from_secs(8))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "Image upload unavailable")?;
    let filename = format!(
        "steam-tui-{}-{}.png",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        attachment.sha
    );
    let mut fields = vec![
        ("sessionid", session),
        ("l", "english".into()),
        ("file_name", filename),
        ("file_sha", attachment.sha.clone()),
        ("file_size", attachment.png.len().to_string()),
        ("file_image_width", attachment.image.width().to_string()),
        ("file_image_height", attachment.image.height().to_string()),
        ("file_type", "image/png".into()),
    ];
    let response = client
        .post(format!("{BASE}beginfileupload/"))
        .header(COOKIE, cookie.clone())
        .form(&fields)
        .send()
        .await
        .map_err(|_| "Could not begin Steam image upload")?;
    let begin = parse_begin(json(response).await?)?;
    let url = location(&begin.result)?;
    let ugcid = scalar(&begin.result.ugcid)?;
    let timestamp = scalar(&begin.timestamp)?;
    if begin.hmac.is_empty() || begin.hmac.len() > 4096 {
        return Err("Invalid upload authorization from Steam".into());
    }
    let mut headers = HeaderMap::new();
    for header in &begin.result.request_headers {
        let name =
            HeaderName::from_bytes(header.name.as_bytes()).map_err(|_| "Invalid upload header")?;
        if name == "host" || name == "content-length" {
            continue;
        }
        let mut value =
            HeaderValue::from_str(&header.value).map_err(|_| "Invalid upload header value")?;
        value.set_sensitive(true);
        headers.insert(name, value);
    }
    let transfer = client
        .put(url)
        .headers(headers)
        .body(attachment.png)
        .send()
        .await;
    let succeeded = transfer.is_ok_and(|response| response.status().is_success());
    fields.extend([
        ("success", if succeeded { "1" } else { "0" }.into()),
        ("ugcid", ugcid),
        ("timestamp", timestamp),
        ("hmac", begin.hmac),
        ("friend_steamid", friend.to_string()),
        ("spoiler", "0".into()),
    ]);
    let commit = client
        .post(format!("{BASE}commitfileupload/"))
        .header(COOKIE, cookie)
        .form(&fields)
        .send()
        .await;
    if !succeeded {
        return Err("Image data upload failed; attachment retained".into());
    }
    let result = json(commit.map_err(
        |_| "Image commit interrupted; delivery unknown. Check history before retrying.",
    )?)
    .await?;
    if result
        .get("success")
        .is_some_and(|value| value == &Value::Bool(true) || value.as_i64() == Some(1))
    {
        Ok(())
    } else {
        Err(format!(
            "{}. Delivery unconfirmed; check history before retrying.",
            rejection("commit-upload", &result)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    #[test]
    fn clipboard_images_are_normalized_with_verified_pixels_and_bounded_dimensions() {
        let image =
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(32, 16, Rgba([80, 10, 100, 128])));
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, ImageFormat::Png).unwrap();
        let attachment = Attachment::decode(bytes.into_inner()).unwrap();
        assert_eq!(
            (attachment.image.width(), attachment.image.height()),
            (32, 16)
        );
        assert_eq!(
            attachment.image.to_rgba8().get_pixel(0, 0).0,
            [80, 10, 100, 128]
        );
        assert_eq!(
            attachment.sha,
            format!("{:x}", Sha1::digest(&attachment.png))
        );
        assert!(attachment.png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(
            !format!("{attachment:?}").contains(&attachment.sha),
            "debug output is metadata only"
        );
        assert!(Attachment::decode(b"not an image".to_vec()).is_err());
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::new_rgba8(4097, 1)
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        assert!(Attachment::decode(bytes.into_inner()).is_err());
    }

    #[test]
    fn upload_directives_cannot_redirect_image_data_to_untrusted_or_insecure_locations() {
        let mut directive = Location {
            ugcid: Value::String("18446744073709551614".into()),
            url_host: "steamugc.blob.core.windows.net".into(),
            url_path: "/cloud/file.dat?sig=opaque".into(),
            use_https: true,
            request_headers: vec![],
        };
        assert!(location(&directive).is_ok());
        assert_eq!(scalar(&directive.ugcid).unwrap(), "18446744073709551614");
        for host in [
            "127.0.0.1",
            "evil.test",
            "steamusercontent.com.evil.test",
            "steamugc.blob.core.windows.net@evil.test",
            "steamugc.blob.core.windows.net:80",
            "steamugc.blob.core.windows.net/evil",
        ] {
            directive.url_host = host.into();
            assert!(location(&directive).is_err(), "{host}");
        }
        directive.url_host = "steamugc.blob.core.windows.net".into();
        directive.use_https = false;
        assert!(location(&directive).is_err());
        assert!(scalar(&Value::String("1\r\nInjected: yes".into())).is_err());
    }

    #[test]
    fn authentication_cookies_are_sensitive_and_reject_header_injection() {
        let header = cookie(42, "synthetic.jwt.token", "123abc").unwrap();
        assert!(header.is_sensitive());
        assert!(!format!("{header:?}").contains("synthetic.jwt.token"));
        assert!(cookie(42, "bad\r\nCookie: x", "123abc").is_err());
    }
    fn begin_fixture(flag: Value) -> Value {
        serde_json::json!({
            "success": 1, "timestamp": 1770000000, "hmac": "synthetic-secret-signature",
            "result": { "success": 1, "storage_system": 2, "rwgrsn": 0,
                "ugcid": "18446744073709551614", "timestamp": 1770000000,
                "url_host": "steamugc.blob.core.windows.net", "url_path": "/cloud/file.dat?sig=synthetic-secret",
                "use_https": flag, "request_headers": [{ "name": "Content-Type", "value": "image/png" }] }
        })
    }

    #[test]
    fn real_web_response_numeric_https_flag_is_accepted_without_weakening_transport_validation() {
        for flag in [Value::Bool(true), Value::Number(1.into())] {
            let response = parse_begin(begin_fixture(flag)).unwrap();
            assert!(response.result.use_https);
            assert_eq!(location(&response.result).unwrap().scheme(), "https");
            assert_eq!(
                scalar(&response.result.ugcid).unwrap(),
                "18446744073709551614"
            );
        }
        for flag in [Value::Bool(false), Value::Number(0.into())] {
            let response = parse_begin(begin_fixture(flag)).unwrap();
            assert!(!response.result.use_https);
            assert!(location(&response.result).is_err());
        }
        for flag in [
            serde_json::json!(2),
            serde_json::json!(-1),
            Value::String("https".into()),
            Value::Null,
        ] {
            assert!(parse_begin(begin_fixture(flag)).is_err());
        }
    }

    #[test]
    fn rejected_and_malformed_upload_responses_are_distinguished_without_echoing_secrets() {
        let response = serde_json::json!({ "success": 15, "hmac": "secret-signature", "message": "secret-access-token" });
        let error = parse_begin(response).err().unwrap();
        assert!(error.contains("AccessDenied (15)"));
        assert!(!error.contains("secret"));
        let mut response = begin_fixture(serde_json::json!(1));
        response.as_object_mut().unwrap().remove("hmac");
        let error = parse_begin(response).err().unwrap();
        assert!(error.contains("missing or invalid fields"));
        assert!(!error.contains("rejected"));
        assert!(!error.contains("synthetic-secret"));
    }
}
