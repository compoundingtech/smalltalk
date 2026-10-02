//! Security metadata is derived from the installed build, never from a source-tree bundle.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use axum::body::Bytes;
use axum::http::HeaderValue;
use axum::http::uri::Authority;
use base64::Engine as _;
use lol_html::html_content::ContentType;
use lol_html::{RewriteStrSettings, element, rewrite_str};
use sha2::{Digest as _, Sha256, Sha384};

pub(super) struct BundleSecurity {
    pub html: HashMap<PathBuf, Bytes>,
    style_sources: String,
}

impl BundleSecurity {
    pub fn load(root: &Path, mount: &str) -> Result<Self> {
        let mut html = HashMap::new();
        let mut styles = BTreeSet::new();
        let integrity = RefCell::new(HashMap::new());
        let mut documents = Vec::new();
        let mut modules = BTreeSet::new();
        for entry in walkdir::WalkDir::new(root) {
            let entry = entry.context("inspect client bundle")?;
            if !entry.file_type().is_file() && !entry.file_type().is_symlink() {
                continue;
            }
            let extension = entry.path().extension().and_then(|value| value.to_str());
            if !matches!(extension, Some("html" | "js" | "mjs")) {
                continue;
            }
            let path = std::fs::canonicalize(entry.path()).context("resolve bundle asset")?;
            anyhow::ensure!(path.starts_with(root), "bundle asset escapes static_dir");
            if !path.is_file() {
                continue;
            }
            let source = std::fs::read_to_string(&path).context("read UTF-8 bundle asset")?;
            if extension == Some("html") {
                documents.push((path, source));
            } else {
                let hash = format!("sha384-{}",
                    base64::engine::general_purpose::STANDARD.encode(Sha384::digest(source.as_bytes())));
                integrity.borrow_mut().insert(path.clone(), hash.clone());
                let relative = entry.path().strip_prefix(root)?.to_str().context("UTF-8 module path")?;
                let url = bundle_url(mount, relative);
                modules.insert(format!(
                    "<link rel=\"modulepreload\" href=\"{}\" integrity=\"{hash}\" crossorigin=\"anonymous\">",
                    html_escape::encode_double_quoted_attribute(url.path()),
                ));
                if let Some(style) = pressable_style(&source)? {
                    styles.insert(format!(" 'sha256-{}'",
                        base64::engine::general_purpose::STANDARD.encode(Sha256::digest(style))));
                }
            }
        }
        // Eagerly populate the module map with integrity-checked built chunks before
        // entry execution. Native modulepreload support and ordinary .js/.mjs chunk
        // URLs (without runtime-added queries) are part of the supported build contract.
        let modules: String = modules.into_iter().collect();
        for (path, source) in documents {
                let inserted = Cell::new(false);
                let document_path = path.strip_prefix(root)?.to_str().context("UTF-8 HTML path")?;
                let base = bundle_url(mount, document_path);
                let mut rewritten = rewrite_str(
                    &source,
                    RewriteStrSettings {
                        element_content_handlers: vec![
                            element!("base", |_| Err("bundle HTML must not override its base URL".into())),
                            element!("script[src], link[href]", |element| {
                                (|| -> Result<()> {
                                let script = element.tag_name() == "script";
                                if script && !inserted.replace(true) {
                                    element.before(&modules, ContentType::Html);
                                }
                                let raw_rel = element.get_attribute("rel").unwrap_or_default();
                                let rel = html_escape::decode_html_entities(&raw_rel);
                                let raw_kind = element.get_attribute("as").unwrap_or_default();
                                let kind = html_escape::decode_html_entities(&raw_kind);
                                anyhow::ensure!(!rel.contains('&') && !kind.contains('&'),
                                    "ambiguous HTML entities in asset link attributes");
                                let asset_link = rel.split_ascii_whitespace().any(|rel| {
                                    rel.eq_ignore_ascii_case("stylesheet")
                                        || rel.eq_ignore_ascii_case("modulepreload")
                                        || (rel.eq_ignore_ascii_case("preload")
                                            && (kind.eq_ignore_ascii_case("script")
                                                || kind.eq_ignore_ascii_case("style")))
                                });
                                if !script && !asset_link {
                                    return Ok(());
                                }
                                let attribute = if script { "src" } else { "href" };
                                let raw_source = element.get_attribute(attribute).unwrap_or_default();
                                let source = html_escape::decode_html_entities(&raw_source);
                                // Require path references, rather than accepting an absolute URL
                                // which happens to match the sentinel used to resolve relative paths.
                                anyhow::ensure!(
                                    !source.is_empty() && !source.starts_with("//")
                                        && !source.contains([':', '\\'])
                                        && !source.chars().any(char::is_control),
                                    "script/style URLs must be same-gateway paths"
                                );
                                let url = base.join(&source)?;
                                anyhow::ensure!(url.origin() == base.origin()
                                    && url.username().is_empty() && url.password().is_none(),
                                    "script/style URLs must be same-gateway paths");
                                let relative = url.path().strip_prefix(&format!("{mount}/"))
                                    .context("script/style URL must stay inside the bundle mount")?;
                                let decoded = urlencoding::decode(relative)?;
                                anyhow::ensure!(
                                    decoded.split('/').all(|part| part != ".." && part != ".")
                                        && !decoded.contains(['\\', '\0']),
                                    "invalid bundle asset path"
                                );
                                let file = std::fs::canonicalize(root.join(decoded.as_ref()))
                                    .context("resolve script/style asset")?;
                                anyhow::ensure!(file.starts_with(root) && file.is_file(),
                                    "script/style asset must stay inside static_dir");
                                let mut hashes = integrity.borrow_mut();
                                if !hashes.contains_key(&file) {
                                    let bytes = std::fs::read(&file).context("read script/style asset")?;
                                    hashes.insert(file.clone(), format!("sha384-{}",
                                        base64::engine::general_purpose::STANDARD.encode(Sha384::digest(&bytes))));
                                }
                                element.set_attribute("integrity", &hashes[&file])?;
                                element.set_attribute("crossorigin", "anonymous")?;
                                // Absolute mounted paths also keep SPA fallback and symlink aliases
                                // from changing the browser's resolution of relative asset URLs.
                                let mut path = url.path().to_owned();
                                if let Some(query) = url.query() {
                                    path.push('?');
                                    path.push_str(query);
                                }
                                element.set_attribute(attribute, &html_escape::encode_double_quoted_attribute(&path))?;
                                Ok(())
                                })().map_err(Into::into)
                            }),
                        ],
                        ..RewriteStrSettings::default()
                    },
                ).with_context(|| format!("secure bundle HTML {}", path.display()))?;
                if !inserted.get() {
                    rewritten.push_str(&modules);
                }
                html.insert(path, Bytes::from(rewritten));
        }
        Ok(Self { html, style_sources: styles.into_iter().collect() })
    }

    pub fn csp(&self, authority: Option<&str>) -> HeaderValue {
        // 'self' alone is not interoperable for WebSockets. Explicit scheme sources
        // preserve the exact gateway authority (including port), never a wildcard.
        let sockets = authority
            .filter(|value| value.bytes().all(|byte| byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'-' | b':' | b'[' | b']')))
            .and_then(|value| value.parse::<Authority>().ok())
            .map(|authority| format!(" ws://{authority} wss://{authority}"))
            .unwrap_or_default();
        HeaderValue::from_str(&format!(
            "default-src 'none'; script-src 'self'; style-src 'self'{}; style-src-attr 'none'; connect-src 'self'{}; img-src 'self' data:; font-src 'self'; manifest-src 'self'; worker-src 'self'; base-uri 'none'; object-src 'none'; frame-ancestors 'none'; form-action 'self'",
            self.style_sources, sockets,
        )).expect("validated authority and base64 hashes form a valid CSP header")
    }
}

fn bundle_url(mount: &str, relative: &str) -> reqwest::Url {
    let mut url = reqwest::Url::parse("http://bundle.invalid/").expect("constant URL");
    url.path_segments_mut().expect("HTTP URL supports path segments")
        .clear()
        .extend(mount[1..].split('/'))
        .extend(relative.split('/'));
    url
}

// React Aria injects exactly this template, trimmed. Only this known CSS is
// authorized; recognizing a new upstream representation requires an explicit update.
// See react-aria/private/interactions/usePress.mjs (react-aria 3.52.1).
fn pressable_style(source: &str) -> Result<Option<String>> {
    if !source.contains("react-aria-pressable-style") {
        return Ok(None);
    }
    let prefix = "@layer {\n  [";
    let suffix = "] {\n    touch-action: pan-x pan-y pinch-zoom;\n  }\n}";
    for (start, _) in source.match_indices('`') {
        let tail = &source[start + 1..];
        let Some((template, after)) = tail.split_once('`') else { continue };
        if !after.starts_with(".trim()") {
            continue;
        }
        // Minifiers can escape newlines inside a template without changing its value.
        let template = template.replace("\\n", "\n");
        let Some(selector) = template.trim().strip_prefix(prefix)
            .and_then(|value| value.strip_suffix(suffix)) else { continue };
        let known_selector = selector == "data-react-aria-pressable"
            || (source.contains("data-react-aria-pressable")
                && selector.strip_prefix("${").and_then(|value| value.strip_suffix('}'))
                    .is_some_and(|name| !name.is_empty()
                        && name.bytes().enumerate().all(|(index, byte)| {
                            byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'$')
                                || (index > 0 && byte.is_ascii_digit())
                        })));
        if known_selector {
            return Ok(Some(format!("{prefix}data-react-aria-pressable{suffix}")));
        }
    }
    anyhow::bail!("unsupported React Aria pressable style in built JS; externalize that style or use the supported literal template (no unsafe-inline fallback)")
}
