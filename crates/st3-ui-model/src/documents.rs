//! What a document view shows, read through the client's `document.get` and `documents.list`.
//!
//! A document is shown as its author wrote it: the text is never filtered or rewritten. This
//! model only names it, finds the `doc/…` links in it, and says why it cannot be shown when it
//! cannot. `fixtures/clients/documents-v1.json` pins the same behaviour for the TypeScript views.

use st3_client::{DocumentContent, DocumentItem, DocumentsPage};

/// A `doc/…` reference found in text, as written.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DocumentLink {
    /// `doc/NAME@HASH`, or `doc/NAME` for the name's latest version.
    pub reference: String,
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
}

/// Splits `doc/NAME@HASH` or `doc/NAME` into its name and version, or `None` when it is not a
/// document reference.
pub fn parse_reference(reference: &str) -> Option<(&str, Option<&str>)> {
    let (name, version) = match reference.rsplit_once('@') {
        Some((name, version)) => (name, Some(version)),
        None => (reference, None),
    };
    let valid_name = name.starts_with("doc/")
        && name.len() > 4
        && !name.contains("..")
        && !name.contains('@')
        && !name.ends_with('/');
    let valid_version = version.is_none_or(|version| {
        version.len() == 64 && version.bytes().all(|byte| byte.is_ascii_hexdigit())
    });
    (valid_name && valid_version).then_some((name, version))
}

fn name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-' | b'~' | b'+')
}

/// Every `doc/…` reference in `text`, in order of first appearance, each once. A reference
/// starts where `doc/` follows the start of the text or a character that cannot be part of a
/// word or path, so `see doc/a`, `[a](doc/a)` and `` `doc/a` `` all count and `mydoc/a` does
/// not. Trailing `.` and `/` end the sentence, not the name.
pub fn document_links(text: &str) -> Vec<DocumentLink> {
    let bytes = text.as_bytes();
    let mut links: Vec<DocumentLink> = Vec::new();
    let mut at = 0;
    while let Some(offset) = text[at..].find("doc/") {
        let start = at + offset;
        at = start + 4;
        if start > 0 && (name_byte(bytes[start - 1]) || bytes[start - 1] == b'@') {
            continue;
        }
        let mut end = start + 4;
        while end < bytes.len() && name_byte(bytes[end]) {
            end += 1;
        }
        while end > start + 4 && matches!(bytes[end - 1], b'.' | b'/') {
            end -= 1;
        }
        let mut reference_end = end;
        if bytes.get(end) == Some(&b'@') {
            let hash_end = end + 1 + 64;
            if hash_end <= bytes.len()
                && bytes[end + 1..hash_end].iter().all(u8::is_ascii_hexdigit)
                && !bytes.get(hash_end).is_some_and(|byte| byte.is_ascii_alphanumeric())
            {
                reference_end = hash_end;
            }
        }
        at = reference_end.max(at);
        let reference = &text[start..reference_end];
        let Some((name, version)) = parse_reference(reference) else {
            continue;
        };
        if links.iter().all(|link| link.reference != reference) {
            links.push(DocumentLink {
                reference: reference.to_owned(),
                name: name.to_owned(),
                version: version.map(str::to_owned),
            });
        }
    }
    links
}

/// Why a document cannot be shown. Each says so in words; none is a blank page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Unavailable {
    /// No document has this name on the host.
    NotFound,
    /// The reference names an exact version this host has not received yet.
    NotSynced,
    /// The host could not be reached, or did not answer.
    RemoteUnavailable,
    /// The bytes are not UTF-8 text, so there is nothing to render.
    NotText,
}

impl Unavailable {
    /// Which state a failed `document.get` means, from the HTTP status (none when the request
    /// never got an answer) and the error envelope's code.
    pub fn from_error(status: Option<u16>, code: &str) -> Self {
        match (status, code) {
            (_, "document-not-synced") => Unavailable::NotSynced,
            (Some(404), _) | (_, "not-found") => Unavailable::NotFound,
            _ => Unavailable::RemoteUnavailable,
        }
    }

    /// One sentence for the page, naming the reference.
    pub fn explain(self, reference: &str) -> String {
        match self {
            Unavailable::NotFound => format!("No document named {reference} is on this host."),
            Unavailable::NotSynced => {
                format!("{reference} has not reached this host yet. It shows once it syncs.")
            }
            Unavailable::RemoteUnavailable => {
                format!("The host did not answer, so {reference} cannot be read now.")
            }
            Unavailable::NotText => format!("{reference} is not text, so it cannot be shown."),
        }
    }
}

/// An open document, ready to render.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Document {
    /// The exact version shown, `doc/NAME@HASH`.
    pub reference: String,
    pub name: String,
    pub version: String,
    /// The first twelve characters of the version, for a header.
    pub short_version: String,
    /// When this version was bound to its name; `None` from a daemon that does not say.
    pub changed_at_unix_ms: Option<u64>,
    /// Whether a newer version exists. `None` when the daemon does not say.
    pub latest: Option<bool>,
    pub owner: Option<String>,
    pub size: u64,
    /// The Markdown exactly as stored.
    pub text: String,
    /// The `doc/…` links in `text`, in order.
    pub links: Vec<DocumentLink>,
}

/// What the document view shows for one reference.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum DocumentView {
    Loading { reference: String },
    Ready(Document),
    Unavailable { reference: String, why: Unavailable, explanation: String },
}

impl DocumentView {
    pub fn loading(reference: &str) -> Self {
        DocumentView::Loading { reference: reference.to_owned() }
    }

    pub fn unavailable(reference: &str, why: Unavailable) -> Self {
        DocumentView::Unavailable {
            reference: reference.to_owned(),
            why,
            explanation: why.explain(reference),
        }
    }

    /// The view of a `document.get` answer for `requested`.
    pub fn from_content(requested: &str, content: &DocumentContent) -> Self {
        let Ok(text) = String::from_utf8(content.bytes.clone()) else {
            return Self::unavailable(requested, Unavailable::NotText);
        };
        let (reference_name, reference_version) = content
            .reference
            .rsplit_once('@')
            .map_or((content.reference.as_str(), ""), |(name, version)| (name, version));
        let name = content.name.clone().unwrap_or_else(|| reference_name.to_owned());
        let version = content.version.clone().unwrap_or_else(|| reference_version.to_owned());
        DocumentView::Ready(Document {
            reference: content.reference.clone(),
            short_version: version.chars().take(12).collect(),
            name,
            version,
            changed_at_unix_ms: content.changed_at_unix_ms,
            latest: content.latest,
            owner: content.owner.clone(),
            size: content.size.unwrap_or(content.bytes.len() as u64),
            links: document_links(&text),
            text,
        })
    }
}

/// One row of the documents list.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct DocumentRow {
    pub reference: String,
    pub name: String,
    pub version: String,
    pub short_version: String,
    pub changed_at_unix_ms: u64,
    pub size: u64,
    pub latest: bool,
    pub owner: Option<String>,
}

impl From<&DocumentItem> for DocumentRow {
    fn from(item: &DocumentItem) -> Self {
        DocumentRow {
            reference: item.reference.clone(),
            name: item.name.clone(),
            version: item.version.clone(),
            short_version: item.version.chars().take(12).collect(),
            changed_at_unix_ms: item.changed_at_unix_ms,
            size: item.size,
            latest: item.latest,
            owner: item.owner.clone(),
        }
    }
}

/// The documents list under one prefix, a page at a time. Loading, empty and failed are three
/// states: a list that has not loaded never says it is empty.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct DocumentList {
    pub prefix: Option<String>,
    pub rows: Vec<DocumentRow>,
    /// Whether a page has arrived.
    pub loaded: bool,
    /// Asks for the next page; `None` once the last page has arrived.
    pub next_cursor: Option<String>,
    /// Why the last page could not be read, if it could not.
    pub failed: Option<Unavailable>,
}

impl DocumentList {
    pub fn new(prefix: Option<&str>) -> Self {
        DocumentList {
            prefix: prefix.map(str::to_owned),
            rows: Vec::new(),
            loaded: false,
            next_cursor: None,
            failed: None,
        }
    }

    /// Adds a page. A row already listed keeps its place.
    pub fn push_page(&mut self, page: &DocumentsPage) {
        for item in &page.items {
            if self.rows.iter().all(|row| row.reference != item.reference) {
                self.rows.push(DocumentRow::from(item));
            }
        }
        self.loaded = true;
        self.failed = None;
        self.next_cursor = if page.has_more { page.next_cursor.clone() } else { None };
    }

    /// Records a failed page read; rows already shown stay.
    pub fn fail(&mut self, why: Unavailable) {
        self.failed = Some(why);
    }

    /// The words for the list when it has no rows to show.
    pub fn placeholder(&self) -> Option<String> {
        if !self.rows.is_empty() {
            return None;
        }
        let under = self.prefix.as_deref().unwrap_or("doc/");
        Some(match (self.loaded, self.failed) {
            (_, Some(Unavailable::RemoteUnavailable)) => {
                "The host did not answer, so the documents cannot be listed now.".to_owned()
            }
            (_, Some(why)) => why.explain(under),
            (false, None) => "Loading…".to_owned(),
            (true, None) => format!("No documents under {under}."),
        })
    }
}

/// The documents opened in one view, newest last. Following a link to a document already on
/// the trail goes back to it, so a cycle of links never grows the trail.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct DocumentTrail {
    pub references: Vec<String>,
}

impl DocumentTrail {
    pub fn open(&mut self, reference: &str) {
        match self.references.iter().position(|open| open == reference) {
            Some(index) => self.references.truncate(index + 1),
            None => self.references.push(reference.to_owned()),
        }
    }

    /// Goes back one document; the first stays.
    pub fn back(&mut self) -> Option<&str> {
        if self.references.len() > 1 {
            self.references.pop();
        }
        self.current()
    }

    pub fn current(&self) -> Option<&str> {
        self.references.last().map(String::as_str)
    }
}

#[cfg(test)]
mod tests;
