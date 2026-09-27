use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use crate::model::MessageView;

/// Export graph messages into a deterministic mailbox tree for translated eval tools.
///
/// The claims store remains authoritative. This tree is a disposable read projection.
pub fn export_messages(root: &Path, messages: &[MessageView]) -> Result<()> {
    let mut export = MessageExport::new(root)?;
    for message in messages {
        export.write(message)?;
    }
    export.finish()
}

pub struct MessageExport<'a> {
    root: &'a Path,
    expected: BTreeSet<PathBuf>,
}

impl<'a> MessageExport<'a> {
    pub fn new(root: &'a Path) -> Result<Self> {
        fs::create_dir_all(root)
            .with_context(|| format!("create message projection {}", root.display()))?;
        Ok(Self {
            root,
            expected: BTreeSet::new(),
        })
    }

    pub fn write(&mut self, message: &MessageView) -> Result<()> {
        let recipient = message.to.strip_prefix("agent/").unwrap_or(&message.to);
        let folder = if message.status == "closed" {
            "archive"
        } else {
            "inbox"
        };
        let id = message
            .subject
            .strip_prefix("message/")
            .unwrap_or(&message.subject);
        let filename = format!(
            "{:020}-{}.md",
            message.created_index,
            urlencoding::encode(id)
        );
        let path = self.root.join(recipient).join(folder).join(filename);
        self.expected.insert(path.clone());
        write_message(&path, message)?;
        Ok(())
    }

    pub fn finish(self) -> Result<()> {
        for path in projected_files(self.root)? {
            if !self.expected.contains(&path) {
                fs::remove_file(&path).with_context(|| {
                    format!("remove stale message projection {}", path.display())
                })?;
            }
        }
        Ok(())
    }
}

fn write_message(path: &Path, message: &MessageView) -> Result<()> {
    let mut text = String::new();
    text.push_str(&format!("from: {}\n", message.from));
    text.push_str(&format!("to: {}\n", message.to));
    if let Some(title) = &message.title {
        text.push_str(&format!("subject: {title}\n"));
    }
    if let Some(parent) = &message.in_reply_to {
        text.push_str(&format!("in-reply-to: {parent}\n"));
    }
    if !message.tags.is_empty() {
        text.push_str(&format!("tags: {}\n", message.tags.join(",")));
    }
    text.push_str(&format!("status: {}\n\n", message.status));
    text.push_str(&message.content);
    if !text.ends_with('\n') {
        text.push('\n');
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let write = match fs::read(path) {
        Ok(current) => current != text.as_bytes(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => return Err(error.into()),
    };
    if write {
        fs::write(path, text)
            .with_context(|| format!("write message projection {}", path.display()))?;
    }
    Ok(())
}

fn projected_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut output = Vec::new();
    if !root.is_dir() {
        return Ok(output);
    }
    for recipient in fs::read_dir(root)? {
        let recipient = recipient?;
        if !recipient.file_type()?.is_dir() {
            continue;
        }
        for folder in ["inbox", "archive"] {
            let path = recipient.path().join(folder);
            if !path.is_dir() {
                continue;
            }
            for entry in fs::read_dir(path)? {
                let entry = entry?;
                if entry.file_type()?.is_file()
                    && entry.path().extension().and_then(|value| value.to_str()) == Some("md")
                {
                    output.push(entry.path());
                }
            }
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn projection_moves_a_closed_message_to_the_archive() {
        let directory = tempdir().unwrap();
        let mut message = MessageView {
            subject: "message/abc".into(),
            from: "agent/sender".into(),
            to: "agent/receiver".into(),
            content: "hello".into(),
            status: "sent".into(),
            title: Some("work".into()),
            in_reply_to: None,
            tags: Vec::new(),
            created_index: 7,
        };
        export_messages(directory.path(), &[message.clone()]).unwrap();
        assert!(
            directory
                .path()
                .join("receiver/inbox/00000000000000000007-abc.md")
                .is_file()
        );

        message.status = "closed".into();
        export_messages(directory.path(), &[message]).unwrap();
        assert!(
            !directory
                .path()
                .join("receiver/inbox/00000000000000000007-abc.md")
                .exists()
        );
        assert!(
            directory
                .path()
                .join("receiver/archive/00000000000000000007-abc.md")
                .is_file()
        );
    }

    #[test]
    fn projection_encodes_a_nested_message_id_as_one_file() {
        let directory = tempdir().unwrap();
        let message = MessageView {
            subject: "message/kickoff/run-1".into(),
            from: "agent/sender".into(),
            to: "agent/receiver".into(),
            content: "hello".into(),
            status: "delivered".into(),
            title: None,
            in_reply_to: None,
            tags: Vec::new(),
            created_index: 8,
        };
        export_messages(directory.path(), &[message]).unwrap();
        assert!(
            directory
                .path()
                .join("receiver/inbox/00000000000000000008-kickoff%2Frun-1.md")
                .is_file()
        );
    }
}
