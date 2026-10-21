//! Clipboard reads happen only in response to Ctrl+V, never on startup.
use std::{process::Stdio, time::Duration};
use steam_core::chat::upload::{Attachment, MAX_IMAGE_BYTES};
use tokio::{io::AsyncReadExt, process::Command, time::timeout};

pub(super) enum Content {
    Image(Attachment),
    Text(String),
}

fn preferred(types: &str) -> Option<(&str, bool)> {
    let types: Vec<_> = types.lines().map(str::trim).collect();
    for mime in [
        "image/png",
        "image/jpeg",
        "image/webp",
        "image/gif",
        "image/bmp",
    ] {
        if types.contains(&mime) {
            return Some((mime, true));
        }
    }
    for mime in ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"] {
        if types.contains(&mime) {
            return Some((mime, false));
        }
    }
    None
}

async fn output(program: &str, args: &[&str], limit: usize) -> Result<Vec<u8>, String> {
    let task = async {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| format!("Clipboard unavailable: install {program}"))?;
        let stdout = child.stdout.take().ok_or("Cannot read clipboard")?;
        let mut bytes = Vec::new();
        stdout
            .take((limit + 1) as u64)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| "Clipboard read failed")?;
        if bytes.len() > limit {
            return Err("Clipboard data exceeds the size limit".into());
        }
        if !child
            .wait()
            .await
            .map_err(|_| "Clipboard read failed")?
            .success()
        {
            return Err("Clipboard is empty or unavailable".into());
        }
        Ok(bytes)
    };
    timeout(Duration::from_secs(5), task)
        .await
        .map_err(|_| "Clipboard read timed out".to_string())?
}

pub(super) async fn read() -> Result<Content, String> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let (program, targets): (&str, &[&str]) = if wayland {
        ("wl-paste", &["--list-types"])
    } else if std::env::var_os("DISPLAY").is_some() {
        ("xclip", &["-selection", "clipboard", "-o", "-t", "TARGETS"])
    } else {
        return Err("Clipboard needs a Wayland or X11 session".into());
    };
    let types = String::from_utf8(output(program, targets, 16 * 1024).await?)
        .map_err(|_| "Invalid clipboard types")?;
    let (mime, image) = preferred(&types).ok_or("Clipboard contains no supported image or text")?;
    let args: Vec<&str> = if wayland {
        vec!["--no-newline", "--type", mime]
    } else {
        vec!["-selection", "clipboard", "-o", "-t", mime]
    };
    let bytes = output(
        program,
        &args,
        if image { MAX_IMAGE_BYTES } else { 64 * 1024 },
    )
    .await?;
    if image {
        let attachment = tokio::task::spawn_blocking(move || Attachment::decode(bytes))
            .await
            .map_err(|_| "Clipboard image decoding failed")??;
        Ok(Content::Image(attachment))
    } else {
        Ok(Content::Text(
            String::from_utf8(bytes).map_err(|_| "Clipboard text is not UTF-8")?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn images_take_precedence_over_text_and_file_lists_are_not_opened() {
        assert_eq!(
            preferred("text/plain\nimage/png\n"),
            Some(("image/png", true))
        );
        assert_eq!(
            preferred("TARGETS\nUTF8_STRING\n"),
            Some(("UTF8_STRING", false))
        );
        assert_eq!(preferred("text/uri-list\napplication/octet-stream"), None);
    }
    #[tokio::test]
    async fn bounded_process_reads_kill_oversized_or_stalled_producers() {
        assert!(output("printf", &["abcdef"], 3).await.is_err());
        assert_eq!(
            output("printf", &["image/png\\n"], 100).await.unwrap(),
            b"image/png\n"
        );
    }
}
