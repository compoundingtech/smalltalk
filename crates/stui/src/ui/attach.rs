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
    describe(&path)
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

/// The message text an agent reads for its attachments, until st carries them itself.
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
