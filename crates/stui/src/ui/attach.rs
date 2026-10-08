//! Images a person attaches to a message: pasted from the clipboard (Ctrl+V while typing) or
//! named by a pasted path. They are kept as files on this machine; until st can carry a blob,
//! the message names the file, which an agent on this machine can read.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The largest image stui attaches.
const MAX_BYTES: u64 = 10 << 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    pub path: PathBuf,
    pub bytes: u64,
    /// Width and height, when the format says them cheaply (PNG).
    pub size: Option<(u32, u32)>,
}

impl Attachment {
    /// `[image 1280×720 · 84 KB]`
    pub fn label(&self) -> String {
        let kb = self.bytes.div_ceil(1024);
        match self.size {
            Some((width, height)) => format!("image {width}×{height} · {kb} KB"),
            None => format!("image · {kb} KB"),
        }
    }
}

/// A terminal that draws images (kitty, Ghostty, WezTerm, iTerm2) and answers stui's query
/// for how.
pub fn graphics_terminal() -> bool {
    let term = std::env::var("TERM").unwrap_or_default();
    let program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    ["kitty", "ghostty", "wezterm"]
        .iter()
        .any(|name| term.contains(name))
        || ["iTerm.app", "WezTerm", "ghostty"].contains(&program.as_str())
        || std::env::var_os("KITTY_WINDOW_ID").is_some()
}

/// Where pasted images are kept: `$XDG_STATE_HOME/st3/stui/attachments`.
pub fn dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
        })?;
    base.is_absolute()
        .then(|| base.join("st3").join("stui").join("attachments"))
}

/// A pasted path that names an image file on this machine.
pub fn from_path(text: &str) -> Option<Attachment> {
    let text = text.trim().trim_matches(|c| c == '\'' || c == '"');
    // Terminals escape spaces in dropped paths.
    let path = PathBuf::from(text.replace("\\ ", " "));
    let image = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp"
            )
        });
    if !image || !path.is_absolute() || text.contains('\n') {
        return None;
    }
    describe(&path)?;
    // Tests keep their files where they made them, not in the person's state.
    let dir = if cfg!(test) { None } else { dir() };
    describe(&kept(&path, dir.as_deref()))
}

/// A copy of a pasted or dropped image in stui's own folder, named for its bytes. macOS hands
/// screenshots over from a temporary folder that it soon empties and that its privacy protection
/// guards from other programs, so an agent could not read the original even on this machine
/// (cos, 2026-10-02). Where no copy can be made, the original.
fn kept(path: &Path, dir: Option<&Path>) -> PathBuf {
    let copy = || -> Option<PathBuf> {
        let dir = dir?;
        if path.starts_with(dir) {
            return Some(path.to_owned());
        }
        let bytes = std::fs::read(path).ok()?;
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        let name = format!("{}.{extension}", &sha256_hex(&bytes)[..16]);
        std::fs::create_dir_all(dir).ok()?;
        let kept = dir.join(name);
        if !kept.exists() {
            std::fs::write(&kept, &bytes).ok()?;
        }
        Some(kept)
    };
    copy().unwrap_or_else(|| path.to_owned())
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The image on this machine's clipboard, saved as a PNG file, if there is one.
pub fn from_clipboard() -> Result<Attachment, String> {
    let dir = dir().ok_or("no place to keep the image (set HOME)")?;
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let path = dir.join(format!("{}.png", uuid::Uuid::now_v7()));
    let saved = if cfg!(target_os = "macos") {
        // AppleScript converts whatever image is on the pasteboard to PNG and writes it.
        let script = format!(
            "set f to open for access POSIX file \"{}\" with write permission\n\
             try\n write (the clipboard as «class PNGf») to f\n on error\n close access f\n error \"no image\"\n end try\n close access f",
            path.display()
        );
        Command::new("osascript")
            .args(["-e", &script])
            .output()
            .is_ok_and(|output| output.status.success())
    } else {
        // Wayland, then X11.
        [
            ("wl-paste", vec!["--no-newline", "--type", "image/png"]),
            (
                "xclip",
                vec!["-selection", "clipboard", "-t", "image/png", "-o"],
            ),
        ]
        .into_iter()
        .any(|(program, args)| {
            Command::new(program)
                .args(&args)
                .output()
                .ok()
                .filter(|output| output.status.success() && is_png(&output.stdout))
                .is_some_and(|output| std::fs::write(&path, output.stdout).is_ok())
        })
    };
    if !saved {
        let _ = std::fs::remove_file(&path);
        return Err("the clipboard holds no image".into());
    }
    describe(&path).ok_or_else(|| "the clipboard image could not be read".into())
}

/// Whether the terminal stui draws in is kitty, which can hand over the person's clipboard
/// through the terminal itself (OSC 5522), wherever stui runs.
pub fn terminal_clipboard() -> bool {
    std::env::var("TERM").is_ok_and(|term| term.contains("kitty"))
        || std::env::var_os("KITTY_WINDOW_ID").is_some()
}

/// The image on the person's clipboard, asked of their terminal (kitty's OSC 5522): it works
/// over SSH or fabric because the request and the image travel through the terminal. kitty
/// asks the person before it hands the clipboard over.
pub fn from_terminal() -> Result<Attachment, String> {
    use base64::Engine as _;
    use std::io::{Read as _, Write as _};
    use std::os::fd::AsRawFd as _;
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|error| format!("no terminal to ask ({error})"))?;
    let base64 = base64::engine::general_purpose::STANDARD;
    write!(
        tty,
        "\x1b]5522;type=read;{}\x1b\\",
        base64.encode("image/png")
    )
    .and_then(|()| tty.flush())
    .map_err(|error| error.to_string())?;
    // The person may be asked first, so the first answer may take a while; after it, the
    // data flows at once.
    let mut received = Vec::new();
    let mut chunk = [0_u8; 65536];
    let mut wait = 30_000;
    loop {
        let mut poll = libc::pollfd {
            fd: tty.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd for an open descriptor, for the given time.
        let ready = unsafe { libc::poll(&mut poll, 1, wait) };
        if ready <= 0 {
            return Err(
                "the terminal did not answer (is it kitty, with clipboard reading allowed?)".into(),
            );
        }
        let read = tty.read(&mut chunk).map_err(|error| error.to_string())?;
        if read == 0 {
            return Err("the terminal closed".into());
        }
        received.extend_from_slice(&chunk[..read]);
        wait = 5_000;
        let text = String::from_utf8_lossy(&received);
        if text.contains("status=DONE") {
            break;
        }
        for status in ["EPERM", "ENOSYS", "EBUSY"] {
            if text.contains(&format!("status={status}")) {
                return Err(match status {
                    "EPERM" => "the terminal did not allow reading the clipboard".into(),
                    "ENOSYS" => "the clipboard holds no image".into(),
                    _ => "the clipboard is busy; try again".into(),
                });
            }
        }
    }
    let encoded = osc_payload(&String::from_utf8_lossy(&received));
    let bytes = base64
        .decode(encoded.as_bytes())
        .map_err(|_| "the terminal sent an image stui could not read".to_owned())?;
    if !is_png(&bytes) {
        return Err("the clipboard holds no image".into());
    }
    let dir = dir().ok_or("no place to keep the image (set HOME)")?;
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let path = dir.join(format!("{}.png", uuid::Uuid::now_v7()));
    std::fs::write(&path, bytes).map_err(|error| error.to_string())?;
    describe(&path).ok_or_else(|| "the image is too large to attach".into())
}

/// Every OSC 5522 DATA packet's payload, in order: one base64 stream.
fn osc_payload(text: &str) -> String {
    let mut encoded = String::new();
    for packet in text.split("\x1b]5522;").skip(1) {
        let packet = packet.split(['\x1b', '\x07']).next().unwrap_or_default();
        if let Some((meta, payload)) = packet.split_once(';')
            && meta.contains("status=DATA")
        {
            encoded.push_str(payload);
        }
    }
    encoded
}

fn describe(path: &Path) -> Option<Attachment> {
    let bytes = std::fs::metadata(path).ok()?.len();
    if bytes == 0 || bytes > MAX_BYTES {
        return None;
    }
    Some(Attachment {
        path: path.to_owned(),
        bytes,
        size: png_size(path),
    })
}

fn is_png(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
}

/// A PNG's width and height, from its header.
fn png_size(path: &Path) -> Option<(u32, u32)> {
    use std::io::Read as _;
    let mut head = [0; 24];
    std::fs::File::open(path).ok()?.read_exact(&mut head).ok()?;
    if !is_png(&head) || &head[12..16] != b"IHDR" {
        return None;
    }
    let number =
        |at: usize| u32::from_be_bytes([head[at], head[at + 1], head[at + 2], head[at + 3]]);
    Some((number(16), number(20)))
}

/// Keep an image a message carried, read from st, under `dir/received`, named by its hash so a
/// second open reuses it.
pub fn received(
    dir: &Path,
    image: &st3_conversation_ui::MailImage,
    bytes: &[u8],
) -> std::io::Result<PathBuf> {
    let extension = match image.media_type.as_str() {
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "png",
    };
    let dir = dir.join("received");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.{extension}", image.sha256));
    if !path.exists() {
        let partial = dir.join(format!(".{}.partial", image.sha256));
        std::fs::write(&partial, bytes)?;
        std::fs::rename(&partial, &path)?;
    }
    Ok(path)
}

/// Show a file with this machine's own viewer, when it has one: `open` on macOS, `xdg-open` on
/// a Linux desktop. False when nothing here can show it (a server reached over SSH).
pub fn show(path: &Path) -> bool {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
    {
        "xdg-open"
    } else {
        return false;
    };
    std::process::Command::new(opener)
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

/// An image file's media type, by its extension: one st accepts (png, jpeg, gif, webp).
pub fn media_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => "image/png",
    }
}

/// The message text an agent reads for its attachments, where st cannot carry them (an st from
/// before message attachments).
pub fn mention(attachments: &[Attachment]) -> String {
    attachments
        .iter()
        .map(|attachment| format!("[image: {}]", attachment.path.display()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_image_read_from_st_is_kept_once_by_its_hash() {
        let root = tempfile::tempdir().unwrap();
        let image = st3_conversation_ui::MailImage {
            sha256: "cd".repeat(32),
            message: "message/picture".into(),
            media_type: "image/jpeg".into(),
            name: Some("photo.jpg".into()),
            size: 3,
        };
        let path = received(root.path(), &image, b"one").unwrap();
        assert_eq!(
            path,
            root.path()
                .join("received")
                .join(format!("{}.jpg", "cd".repeat(32)))
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"one");
        // The same hash is the same image: a second open reuses the file.
        assert_eq!(received(root.path(), &image, b"two").unwrap(), path);
        assert_eq!(std::fs::read(&path).unwrap(), b"one");
    }

    #[test]
    fn kittys_clipboard_answer_is_its_data_packets_joined() {
        let answer = "\x1b]5522;type=read:status=OK\x1b\\\x1b]5522;type=read:status=DATA:mime=aW1hZ2UvcG5n;iVBO\x1b\\\x1b]5522;type=read:status=DATA:mime=aW1hZ2UvcG5n;Rw0K\x1b\\\x1b]5522;type=read:status=DONE\x1b\\";
        assert_eq!(osc_payload(answer), "iVBORw0K");
    }

    #[test]
    fn a_pasted_image_is_kept_in_stuis_own_folder_by_its_bytes() {
        let from = tempfile::tempdir().unwrap();
        let keep = tempfile::tempdir().unwrap();
        let shot = from.path().join("Screenshot 2026-10-02 at 22.20.50.png");
        std::fs::write(&shot, b"\x89PNG\r\n\x1a\nnot really").unwrap();
        let kept = kept(&shot, Some(keep.path()));
        assert!(kept.starts_with(keep.path()), "{}", kept.display());
        assert_eq!(std::fs::read(&kept).unwrap(), std::fs::read(&shot).unwrap());
        // The temporary original can go; the kept copy stays.
        std::fs::remove_file(&shot).unwrap();
        assert!(kept.exists());
        // The same bytes are kept once; an image already kept stays where it is.
        std::fs::write(&shot, b"\x89PNG\r\n\x1a\nnot really").unwrap();
        assert_eq!(super::kept(&shot, Some(keep.path())), kept);
        assert_eq!(super::kept(&kept, Some(keep.path())), kept);
        // Nowhere to keep it: the original.
        assert_eq!(super::kept(&shot, None), shot);
    }

    #[test]
    fn a_pasted_image_path_attaches_and_other_text_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shot one.png");
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        png.extend(1280u32.to_be_bytes());
        png.extend(720u32.to_be_bytes());
        png.extend([0; 32]);
        std::fs::write(&path, &png).unwrap();
        let dropped = path.display().to_string().replace(' ', "\\ ");
        let attachment = from_path(&format!("'{dropped}' ")).unwrap();
        assert_eq!(attachment.path, path);
        assert_eq!(attachment.size, Some((1280, 720)));
        assert_eq!(attachment.label(), "image 1280×720 · 1 KB");
        assert_eq!(
            mention(&[attachment]),
            format!("[image: {}]", path.display())
        );
        assert!(from_path("hello there").is_none());
        assert!(from_path("/no/such/file.png").is_none());
        assert!(from_path(&format!("{}\nmore", path.display())).is_none());
    }
}
