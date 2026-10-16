use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

use tokio::io::unix::AsyncFd;
use zeroize::{Zeroize, Zeroizing};

use super::{AccountError, PROMPT_TIMEOUT, Result};

/// A real nonblocking tty, not Tokio stdin/spawn_blocking: cancelling its future
/// cannot strand a blocking read or keep runtime shutdown waiting for input.
struct HiddenTty {
    file: AsyncFd<File>,
    original: libc::termios,
}

impl HiddenTty {
    fn open() -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open("/dev/tty")
            .map_err(|_| {
                AccountError::Message("Interactive login needs a controlling /dev/tty terminal")
            })?;
        // SAFETY: the owned File keeps a stable descriptor open throughout
        // AsyncFd's lifetime; no code closes or replaces its descriptor.
        let file = unsafe { AsyncFd::register(file) }.map_err(|_| {
            AccountError::Message("Cannot register the login terminal for asynchronous input")
        })?;
        let mut original = MaybeUninit::<libc::termios>::uninit();
        // SAFETY: tcgetattr initializes the entire termios value on success and
        // the descriptor remains owned by `file` for this operation.
        if unsafe { libc::tcgetattr(file.get_ref().as_raw_fd(), original.as_mut_ptr()) } != 0 {
            return Err(AccountError::Message("Cannot read login terminal settings"));
        }
        // SAFETY: successful tcgetattr initialized this value above.
        let original = unsafe { original.assume_init() };
        let mut hidden = original;
        hidden.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG | libc::IEXTEN);
        hidden.c_cc[libc::VMIN] = 1;
        hidden.c_cc[libc::VTIME] = 0;
        // SAFETY: valid owned tty fd and initialized termios. TCSAFLUSH also
        // discards input typed before hidden mode was enabled.
        if unsafe { libc::tcsetattr(file.get_ref().as_raw_fd(), libc::TCSAFLUSH, &hidden) } != 0 {
            return Err(AccountError::Message(
                "Cannot disable terminal echo; refusing to read a secret",
            ));
        }
        Ok(Self { file, original })
    }

    async fn read_line(&self) -> Result<Zeroizing<String>> {
        // Fixed maximum capacity avoids leaving old secret copies in freed
        // allocations during buffer growth.
        let mut bytes = Zeroizing::new(Vec::with_capacity(4096));
        loop {
            let mut ready = self
                .file
                .readable()
                .await
                .map_err(|_| AccountError::Message("Cannot wait for login terminal input"))?;
            let mut byte = Zeroizing::new([0_u8; 1]);
            let result = ready.try_io(|fd| fd.get_ref().read(&mut *byte));
            match result {
                Err(_) => continue,
                Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => continue,
                Ok(Err(_)) => {
                    return Err(AccountError::Message("Cannot read login terminal input"));
                }
                Ok(Ok(0)) => {
                    return Err(AccountError::Message(
                        "Login cancelled: terminal input ended",
                    ));
                }
                Ok(Ok(_)) => {}
            }
            match byte[0] {
                b'\n' | b'\r' => {
                    let raw = std::mem::take(&mut *bytes);
                    return match String::from_utf8(raw) {
                        Ok(text) => Ok(Zeroizing::new(text)),
                        Err(error) => {
                            error.into_bytes().zeroize();
                            Err(AccountError::Message("Login input was not valid UTF-8"))
                        }
                    };
                }
                3 | 4 => {
                    return Err(AccountError::Message(
                        "Login cancelled; no saved credential was removed",
                    ));
                }
                8 | 127 => {
                    // Remove an entire UTF-8 character without making copies.
                    while let Some(last) = bytes.last_mut() {
                        let continuation = *last & 0xc0 == 0x80;
                        last.zeroize();
                        bytes.pop();
                        if !continuation {
                            break;
                        }
                    }
                }
                21 => bytes.zeroize(),
                value if value >= 32 && value != 127 => {
                    if bytes.len() == 4096 {
                        return Err(AccountError::Message(
                            "Login input exceeded the terminal length limit",
                        ));
                    }
                    bytes.push(value);
                }
                _ => {}
            }
        }
    }
}

impl Drop for HiddenTty {
    fn drop(&mut self) {
        // SAFETY: this fd is still owned. Flush unread secret bytes before
        // restoring echo, including when a timeout/mobile approval cancels us.
        unsafe {
            libc::tcflush(self.file.get_ref().as_raw_fd(), libc::TCIFLUSH);
            libc::tcsetattr(
                self.file.get_ref().as_raw_fd(),
                libc::TCSANOW,
                &self.original,
            );
        }
        let _ = self.file.get_ref().write_all(b"\n");
    }
}

pub(super) async fn prompt(message: &'static str) -> Result<Zeroizing<String>> {
    let tty = HiddenTty::open()?;
    tty.file
        .get_ref()
        .write_all(message.as_bytes())
        .map_err(|_| AccountError::Message("Cannot display login prompt on the terminal"))?;
    tokio::time::timeout(PROMPT_TIMEOUT, tty.read_line())
        .await
        .map_err(|_| {
            AccountError::Message("Login prompt timed out; no saved credential was removed")
        })?
}
