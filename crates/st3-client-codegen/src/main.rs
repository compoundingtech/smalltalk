use anyhow::{Context as _, Result, bail};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const RUST_MODELS_TEMPLATE: &str = include_str!("../templates/generated.rs.in");
const RUST_CLIENT_TEMPLATE: &str = include_str!("../templates/lib.rs.in");
const SWIFT_MODELS_TEMPLATE: &str = include_str!("../templates/Models.swift.in");
const SWIFT_CLIENT_TEMPLATE: &str = include_str!("../templates/Client.swift.in");
const TYPESCRIPT_CLIENT_TEMPLATE: &str = include_str!("../templates/Client.ts.in");

fn main() -> Result<()> {
    let check = std::env::args()
        .skip(1)
        .any(|argument| argument == "--check");
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let operations_path = root.join("docs/st3/client-v0/schemas/operations.json");
    let schema_path = root.join("docs/st3/client-v0/schemas/client-v0.schema.json");
    let operations_bytes = std::fs::read(&operations_path).context("read operations manifest")?;
    let schema_bytes = std::fs::read(&schema_path).context("read client schema")?;
    let operations: Value = serde_json::from_slice(&operations_bytes)?;
    let schema: Value = serde_json::from_slice(&schema_bytes)?;
    let actions = operations["actions"]
        .as_object()
        .context("actions object")?;
    let reads = operations["reads"].as_array().context("reads array")?;
    let digest = hex_digest(&[&schema_bytes, &operations_bytes]);
    let rust = format_rust(&rust_contract(
        &digest,
        actions.keys().map(String::as_str),
        reads,
    )?)?;
    let swift = swift_contract(&digest, actions.keys().map(String::as_str), reads)?;
    let rust_models = format_rust(&render_marker(
        RUST_MODELS_TEMPLATE,
        "    // @st3-codegen:rust-action-constructors",
        &rust_action_constructors(actions)?,
    )?)?;
    let rust_client = format_rust(&render_marker(
        RUST_CLIENT_TEMPLATE,
        "    // @st3-codegen:rust-operation-methods",
        &rust_operation_methods(reads, actions)?,
    )?)?;
    let swift_models = render_marker(
        SWIFT_MODELS_TEMPLATE,
        "    // @st3-codegen:swift-action-constructors",
        &swift_action_constructors(actions)?,
    )?;
    let swift_client = render_marker(
        SWIFT_CLIENT_TEMPLATE,
        "    // @st3-codegen:swift-operation-methods",
        &swift_operation_methods(reads, actions)?,
    )?;
    let typescript_models = typescript_models(&schema, &operations, &digest)?;
    let typescript_client = render_marker(
        TYPESCRIPT_CLIENT_TEMPLATE,
        "    // @st3-codegen:typescript-operation-methods",
        &typescript_operation_methods(reads, actions)?,
    )?;
    validate_surfaces(
        &schema,
        &operations,
        &rust_models,
        &rust_client,
        &swift_models,
        &swift_client,
    )?;
    output(
        &root.join("crates/st3-client/src/contract.rs"),
        &rust,
        check,
    )?;
    output(
        &root.join("clients/swift/St3Client/Sources/St3Client/Contract.generated.swift"),
        &swift,
        check,
    )?;
    output(
        &root.join("crates/st3-client/src/generated.rs"),
        &rust_models,
        check,
    )?;
    output(
        &root.join("crates/st3-client/src/lib.rs"),
        &rust_client,
        check,
    )?;
    output(
        &root.join("clients/swift/St3Client/Sources/St3Client/Models.swift"),
        &swift_models,
        check,
    )?;
    output(
        &root.join("clients/swift/St3Client/Sources/St3Client/Client.swift"),
        &swift_client,
        check,
    )?;
    output(
        &root.join("clients/typescript/st3-client/Models.generated.ts"),
        &typescript_models,
        check,
    )?;
    output(
        &root.join("clients/typescript/st3-client/Client.generated.ts"),
        &typescript_client,
        check,
    )?;
    Ok(())
}

fn format_rust(source: &str) -> Result<String> {
    let mut child = Command::new("rustfmt")
        .args(["--edition", "2024", "--emit", "stdout"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start rustfmt for generated Rust client artifact")?;
    child
        .stdin
        .take()
        .context("open rustfmt stdin")?
        .write_all(source.as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "rustfmt rejected generated Rust client artifact: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8(output.stdout).context("rustfmt emitted non-UTF-8")
}

fn render_marker(template: &str, marker: &str, generated: &str) -> Result<String> {
    if template.matches(marker).count() != 1 {
        bail!("template must contain exactly one `{marker}` marker");
    }
    Ok(template.replace(marker, generated))
}

fn action_parameter<'a>(action: &str, definition: &'a Value) -> Result<&'a str> {
    definition["parameters"]
        .as_str()
        .with_context(|| format!("action `{action}` has no parameter model"))
}

fn action_method(action: &str) -> String {
    action.replace(['.', '-'], "_")
}

fn rust_action_constructors(actions: &serde_json::Map<String, Value>) -> Result<String> {
    let mut out = String::new();
    for (action, definition) in actions {
        let method = action_method(action);
        let variant = pascal(action);
        let parameters = action_parameter(action, definition)?;
        writeln!(
            out,
            "    pub fn {method}(id: impl Into<String>, idempotency_key: impl Into<String>, fence: Fence, parameters: {parameters}) -> Result<Self, serde_json::Error> {{\n        Self::new(id, ActionType::{variant}, idempotency_key, fence, &parameters)\n    }}"
        )?;
    }
    Ok(out.trim_end().to_owned())
}

fn swift_action_constructors(actions: &serde_json::Map<String, Value>) -> Result<String> {
    let mut out = String::new();
    for (action, definition) in actions {
        let method = lower_camel(&pascal(action));
        let action_case = lower_camel(&pascal(action));
        let parameters = action_parameter(action, definition)?;
        writeln!(
            out,
            "    public static func {method}(id: String, idempotencyKey: String, fence: Fence, parameters: {parameters}) throws -> Self {{ try .init(id: id, type: .{action_case}, idempotencyKey: idempotencyKey, fence: fence, typedParameters: parameters) }}"
        )?;
    }
    Ok(out.trim_end().to_owned())
}

fn rust_operation_methods(
    reads: &[Value],
    actions: &serde_json::Map<String, Value>,
) -> Result<String> {
    let mut out = String::new();
    for read in reads {
        let id = read["id"].as_str().context("read id")?;
        let path = read["path"].as_str().context("read path")?;
        let method = action_method(id);
        if id == "capabilities.get" {
            writeln!(
                out,
                "    pub async fn capabilities(&self) -> Result<Envelope<Capabilities>, ClientError> {{ self.capabilities_internal().await }}"
            )?;
        } else if id == "timeline.list" {
            writeln!(
                out,
                "    pub async fn timeline(&self, session_id: &str, cursor: Option<&str>, limit: Option<usize>) -> Result<Envelope<TimelinePage>, ClientError> {{ self.timeline_page_internal(session_id, cursor, limit).await }}"
            )?;
        } else if id == "events.list" {
            writeln!(
                out,
                "    pub async fn events(&self, after: Option<&str>, limit: Option<usize>, wait_ms: Option<u64>) -> Result<Envelope<EventPage>, ClientError> {{ self.events_internal(after, limit, wait_ms).await }}"
            )?;
        } else if id == "terminal.screen" {
            writeln!(
                out,
                "    pub async fn terminal_screen(&self, terminal_id: &str) -> Result<Envelope<TerminalScreen>, ClientError> {{ self.terminal_screen_internal(terminal_id).await }}"
            )?;
        } else if id.ends_with(".get") {
            let collection = path
                .trim_start_matches("/v1/client/")
                .split('/')
                .next()
                .context("read collection")?;
            writeln!(
                out,
                "    pub async fn {method}(&self, id: &str) -> Result<Envelope<Resource>, ClientError> {{ self.resource_internal(\"{collection}\", id).await }}"
            )?;
        } else if id.starts_with("launch-") {
            let child = id.trim_start_matches("launch-").trim_end_matches(".list");
            writeln!(
                out,
                "    pub async fn {method}(&self, launch_id: &str, cursor: Option<&str>, limit: Option<usize>) -> Result<Envelope<Page>, ClientError> {{ self.launch_children(launch_id, \"{child}\", cursor, limit).await }}"
            )?;
        } else if id.ends_with(".list") {
            let collection = path
                .trim_start_matches("/v1/client/")
                .split('/')
                .next()
                .context("read collection")?;
            writeln!(
                out,
                "    pub async fn {method}(&self, cursor: Option<&str>, limit: Option<usize>, history: bool) -> Result<Envelope<Page>, ClientError> {{ self.list_internal(\"{collection}\", cursor, limit, history).await }}"
            )?;
        } else {
            bail!("no Rust read surface renderer for `{id}`");
        }
    }
    for (action, definition) in actions {
        let method = action_method(action);
        let parameters = action_parameter(action, definition)?;
        writeln!(
            out,
            "    pub async fn {method}(&self, id: impl Into<String>, idempotency_key: impl Into<String>, fence: Fence, parameters: {parameters}) -> Result<Envelope<ActionResult>, ClientError> {{ let request = ActionRequest::{method}(id, idempotency_key, fence, parameters).map_err(|error| ClientError::Protocol(error.to_string()))?; self.action_internal(&request).await }}"
        )?;
    }
    Ok(out.trim_end().to_owned())
}

fn swift_operation_methods(
    reads: &[Value],
    actions: &serde_json::Map<String, Value>,
) -> Result<String> {
    let mut out = String::new();
    for read in reads {
        let id = read["id"].as_str().context("read id")?;
        let path = read["path"].as_str().context("read path")?;
        let method = lower_camel(&pascal(id));
        if matches!(
            id,
            "capabilities.get" | "timeline.list" | "events.list" | "terminal.screen"
        ) {
            continue;
        } else if id.ends_with(".get") {
            let collection = path
                .trim_start_matches("/v1/client/")
                .split('/')
                .next()
                .context("read collection")?;
            writeln!(
                out,
                "    public func {method}(id: String) async throws -> Envelope<Resource> {{ try await resource(\"{collection}\", id: id) }}"
            )?;
        } else if id.starts_with("launch-") {
            let child = id.trim_start_matches("launch-").trim_end_matches(".list");
            writeln!(
                out,
                "    public func {method}(launchID: String, cursor: String? = nil, limit: Int? = nil) async throws -> Envelope<ResourcePage> {{ var query: [URLQueryItem] = []; if let cursor {{ query.append(.init(name: \"cursor\", value: cursor)) }}; if let limit {{ query.append(.init(name: \"limit\", value: String(limit))) }}; return try await get(\"v1/client/launches/\\(launchID.replacingOccurrences(of: \"launch/\", with: \"\"))/{child}\", query: query) }}"
            )?;
        } else if id.ends_with(".list") {
            let collection = path
                .trim_start_matches("/v1/client/")
                .split('/')
                .next()
                .context("read collection")?;
            writeln!(
                out,
                "    public func {method}(cursor: String? = nil, limit: Int? = nil, history: Bool = false) async throws -> Envelope<ResourcePage> {{ try await list(\"{collection}\", cursor: cursor, limit: limit, history: history) }}"
            )?;
        } else {
            bail!("no Swift read surface renderer for `{id}`");
        }
    }
    for (action, definition) in actions {
        let method = lower_camel(&pascal(action));
        let parameters = action_parameter(action, definition)?;
        writeln!(
            out,
            "    public func {method}(id: String, idempotencyKey: String, fence: Fence, parameters: {parameters}) async throws -> Envelope<ActionResult> {{ try await submit(try .{method}(id: id, idempotencyKey: idempotencyKey, fence: fence, parameters: parameters)) }}"
        )?;
    }
    Ok(out.trim_end().to_owned())
}

fn struct_block<'a>(source: &'a str, declaration: &str) -> Result<&'a str> {
    let start = source
        .find(declaration)
        .with_context(|| format!("client surface has no `{declaration}`"))?;
    let tail = &source[start..];
    let end = tail
        .find("\n}")
        .map(|offset| offset + 2)
        .unwrap_or(tail.len());
    Ok(&tail[..end])
}

fn schema_properties(definition: &Value) -> Option<&serde_json::Map<String, Value>> {
    definition
        .get("properties")
        .and_then(Value::as_object)
        .or_else(|| {
            definition
                .get("allOf")?
                .as_array()?
                .iter()
                .find_map(|part| part.get("properties").and_then(Value::as_object))
        })
}

fn rust_field(name: &str) -> String {
    match name {
        "type" => "entry_type".into(),
        "final" => "is_final".into(),
        "loop" => "loop_spec".into(),
        other => other.into(),
    }
}

fn lower_camel_snake(value: &str) -> String {
    let mut parts = value.split('_');
    let mut out = parts.next().unwrap_or_default().to_owned();
    for part in parts {
        if matches!(part, "id" | "url" | "ms") {
            out.push_str(&part.to_ascii_uppercase());
        } else if part == "ids" {
            out.push_str("IDs");
        } else {
            out.push_str(&pascal(part));
        }
    }
    out
}

fn validate_model(
    schema: &Value,
    definition: &str,
    rust_name: &str,
    swift_name: &str,
    rust: &str,
    swift: &str,
) -> Result<()> {
    let model = &schema["$defs"][definition];
    if model.is_null() {
        bail!("client schema has no `{definition}` definition");
    }
    let properties = schema_properties(model)
        .with_context(|| format!("client schema `{definition}` has no properties"))?;
    let rust_block = struct_block(rust, &format!("pub struct {rust_name} {{"))?;
    let swift_block = struct_block(swift, &format!("public struct {swift_name}:"))?;
    for property in properties.keys().filter(|name| name.as_str() != "kind") {
        let rust_field = rust_field(property);
        let rust_discriminated_timeline = definition == "TimelineEntry"
            && property == "type"
            && rust_block.contains("pub body: TimelineBody")
            && (rust.contains("#[serde(tag = \"type\", content = \"body\"")
                || (rust.contains("struct TaggedBody")
                    && rust.contains("#[serde(rename = \"type\")]")));
        if !rust_block.contains(&format!("pub {rust_field}:")) && !rust_discriminated_timeline {
            bail!("Rust `{rust_name}` does not model schema field `{property}`");
        }
        let swift_field = match property.as_str() {
            "final" => "isFinal".into(),
            other => lower_camel_snake(other),
        };
        if !swift_block.contains(&swift_field) {
            bail!("Swift `{swift_name}` does not model schema field `{property}`");
        }
    }
    Ok(())
}

fn validate_surfaces(
    schema: &Value,
    operations: &Value,
    rust: &str,
    rust_client: &str,
    swift: &str,
    swift_client: &str,
) -> Result<()> {
    let refs = schema["$defs"]["Resource"]["oneOf"]
        .as_array()
        .context("Resource.oneOf")?;
    for reference in refs {
        let definition = reference["$ref"]
            .as_str()
            .and_then(|value| value.rsplit('/').next())
            .context("Resource reference")?;
        validate_model(
            schema,
            definition,
            definition,
            &format!("{definition}Resource"),
            rust,
            swift,
        )?;
        let swift_case = lower_camel(definition);
        if !rust.contains(&format!("    {definition}({definition}),"))
            || !swift.contains(&format!(".{swift_case}("))
        {
            bail!("Resource discriminator `{definition}` is absent from a generated client");
        }
    }
    for definition in [
        "TimelineEntry",
        "TimelinePage",
        "PairingBegin",
        "PairingChallenge",
        "PairingComplete",
        "PairedSession",
        "TerminalAttachment",
        "MachineCapacity",
        "MachineOccupancy",
        "MachineTransport",
        "StructuredDiff",
        "Visualization",
        "VisualizationNode",
        "VisualizationEdge",
        "VisualizationGroup",
    ] {
        validate_model(schema, definition, definition, definition, rust, swift)?;
    }
    for token in [
        "pub async fn pairing_begin",
        "pub async fn pairing_complete",
        "pub async fn terminal_frames",
        "Endpoint::Unix",
        "Endpoint::FabricLoopback",
    ] {
        if !rust_client.contains(token) {
            bail!("Rust client surface is missing `{token}`");
        }
    }
    for token in [
        "func capabilities(",
        "func timeline(",
        "cursor: String?",
        "func events(",
        "func beginPairing(",
        "func completePairing(",
        "func terminalScreen(",
        "func terminalFrames(",
    ] {
        if !swift_client.contains(token) {
            bail!("Swift client surface is missing `{token}`");
        }
    }
    let reads = operations["reads"].as_array().context("reads array")?;
    for read in reads {
        let method = read["method"].as_str().context("read method")?;
        if method != "GET" {
            bail!("read operation uses unsupported method `{method}`");
        }
        let path = read["path"].as_str().context("read path")?;
        if !path.starts_with("/v1/client/") {
            bail!("read operation `{path}` escapes the client boundary");
        }
        let rust_method = match read["id"].as_str().context("read id")? {
            "capabilities.get" => "capabilities".into(),
            "timeline.list" => "timeline".into(),
            "events.list" => "events".into(),
            "terminal.screen" => "terminal_screen".into(),
            id => action_method(id),
        };
        let swift_method = lower_camel(&pascal(read["id"].as_str().unwrap()));
        let swift_method = match read["id"].as_str().unwrap() {
            "capabilities.get" => "capabilities".into(),
            "timeline.list" => "timeline".into(),
            "events.list" => "events".into(),
            "terminal.screen" => "terminalScreen".into(),
            _ => swift_method,
        };
        if !rust_client.contains(&format!("pub async fn {rust_method}("))
            || !swift_client.contains(&format!("public func {swift_method}("))
        {
            bail!(
                "read operation `{}` has no typed Rust/Swift method",
                read["id"]
            );
        }
    }
    let action_schema = schema["$defs"]["ActionRequest"]["oneOf"]
        .as_array()
        .context("ActionRequest.oneOf")?;
    for (action, definition) in operations["actions"]
        .as_object()
        .context("actions object")?
    {
        let parameters = action_parameter(action, definition)?;
        let branch = action_schema
            .iter()
            .find(|branch| {
                branch["properties"]["type"]["const"].as_str() == Some(action)
                    || branch["properties"]["type"]["enum"]
                        .as_array()
                        .is_some_and(|items| items.iter().any(|item| item.as_str() == Some(action)))
            })
            .with_context(|| format!("action `{action}` absent from ActionRequest schema"))?;
        if let Some(reference) = branch["properties"]["parameters"]["$ref"].as_str() {
            if reference.rsplit('/').next() != Some(parameters) {
                bail!("action `{action}` parameter model disagrees with schema");
            }
        } else {
            let properties = branch["properties"]["parameters"]["properties"]
                .as_object()
                .with_context(|| format!("action `{action}` has no parameter properties"))?;
            let rust_block = struct_block(rust, &format!("pub struct {parameters} {{"))?;
            let swift_block = struct_block(swift, &format!("public struct {parameters}:"))?;
            for property in properties.keys() {
                if !rust_block.contains(&format!("pub {}:", rust_field(property)))
                    || !swift_block.contains(&lower_camel_snake(property))
                {
                    bail!("action `{action}` typed parameter model omits `{property}`");
                }
            }
        }
        let rust_method = action_method(action);
        let swift_method = lower_camel(&pascal(action));
        if !rust.contains(&format!("pub fn {rust_method}("))
            || !rust_client.contains(&format!("pub async fn {rust_method}("))
            || !swift.contains(&format!("public static func {swift_method}("))
            || !swift_client.contains(&format!("public func {swift_method}("))
        {
            bail!("action `{action}` has no typed Rust/Swift constructor and client method");
        }
    }
    Ok(())
}

fn output(path: &Path, contents: &str, check: bool) -> Result<()> {
    // Generated artifacts have one canonical EOF: no blank trailing lines and
    // exactly one final newline. Templates and renderers are allowed to use
    // whichever trailing whitespace is clearest for their own implementation.
    let contents = format!("{}\n", contents.trim_end());
    if check {
        let current =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        if current != contents {
            bail!(
                "{} is stale; run `cargo run -p st3-client-codegen`",
                path.display()
            );
        }
    } else {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, &contents).with_context(|| format!("write {}", path.display()))?;
    }
    Ok(())
}

fn hex_digest(parts: &[&[u8]]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update(part);
    }
    let digest = hash.finalize();
    format!("{digest:x}")
}

fn pascal(value: &str) -> String {
    value
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .map(|first| first.to_ascii_uppercase().to_string() + chars.as_str())
                .unwrap_or_default()
        })
        .collect()
}

fn rust_contract<'a>(
    digest: &str,
    actions: impl Iterator<Item = &'a str>,
    reads: &[Value],
) -> Result<String> {
    let actions = actions.collect::<Vec<_>>();
    let mut out = format!("// @generated by st3-client-codegen; contract sha256 {digest}\n\n");
    writeln!(
        out,
        "#[rustfmt::skip]\npub const CONTRACT_SHA256: &str = \"{digest}\";"
    )?;
    writeln!(
        out,
        "#[rustfmt::skip]\npub const ACTION_NAMES: &[&str] = &["
    )?;
    for action in &actions {
        writeln!(out, "    \"{action}\",")?;
    }
    writeln!(
        out,
        "];\n#[rustfmt::skip]\npub const READ_OPERATIONS: &[(&str, &str)] = &["
    )?;
    for read in reads {
        writeln!(
            out,
            "    (\"{}\", \"{}\"),",
            read["id"].as_str().context("read id")?,
            read["path"].as_str().context("read path")?
        )?;
    }
    writeln!(out, "];")?;
    Ok(out)
}

fn swift_contract<'a>(
    digest: &str,
    actions: impl Iterator<Item = &'a str>,
    reads: &[Value],
) -> Result<String> {
    let actions = actions.collect::<Vec<_>>();
    let mut out = format!(
        "// @generated by st3-client-codegen; contract sha256 {digest}\n\nimport Foundation\n\n"
    );
    writeln!(out, "public let st3ClientContractSHA256 = \"{digest}\"\n")?;
    writeln!(
        out,
        "public enum ActionType: String, Codable, CaseIterable, Sendable {{"
    )?;
    for action in &actions {
        writeln!(
            out,
            "    case {} = \"{}\"",
            lower_camel(&pascal(action)),
            action
        )?;
    }
    writeln!(
        out,
        "}}\n\npublic enum ReadOperation: String, CaseIterable, Sendable {{"
    )?;
    for read in reads {
        let id = read["id"].as_str().context("read id")?;
        writeln!(out, "    case {} = \"{}\"", lower_camel(&pascal(id)), id)?;
    }
    writeln!(
        out,
        "}}\n\npublic let st3ClientReadPaths: [ReadOperation: String] = ["
    )?;
    for read in reads {
        let id = read["id"].as_str().unwrap();
        writeln!(
            out,
            "    .{}: \"{}\",",
            lower_camel(&pascal(id)),
            read["path"].as_str().unwrap()
        )?;
    }
    writeln!(out, "]\n")?;
    Ok(out)
}

fn lower_camel(value: &str) -> String {
    let mut chars = value.chars();
    chars
        .next()
        .map(|first| first.to_ascii_lowercase().to_string() + chars.as_str())
        .unwrap_or_default()
}

fn ts_type(value: &Value) -> Result<String> {
    if let Some(reference) = value.get("$ref").and_then(Value::as_str) {
        return Ok(reference
            .rsplit('/')
            .next()
            .context("TypeScript reference")?
            .into());
    }
    if let Some(constant) = value.get("const") {
        return Ok(constant.to_string());
    }
    if let Some(items) = value.get("enum").and_then(Value::as_array) {
        return Ok(items
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join(" | "));
    }
    for union in ["oneOf", "anyOf"] {
        if let Some(items) = value.get(union).and_then(Value::as_array) {
            return Ok(format!(
                "({})",
                items
                    .iter()
                    .map(ts_type)
                    .collect::<Result<Vec<_>>>()?
                    .join(" | ")
            ));
        }
    }
    if let Some(types) = value.get("type").and_then(Value::as_array) {
        return Ok(types
            .iter()
            .map(|kind| ts_type(&serde_json::json!({"type": kind})))
            .collect::<Result<Vec<_>>>()?
            .join(" | "));
    }
    let base = match value.get("type").and_then(Value::as_str) {
        Some("string") => "string".into(),
        Some("integer" | "number") => "number".into(),
        Some("boolean") => "boolean".into(),
        Some("null") => "null".into(),
        Some("array") => format!("Array<{}>", ts_type(&value["items"])?),
        Some("object") => ts_object(value)?,
        Some(other) => bail!("unsupported TypeScript schema type `{other}`"),
        None if value.get("properties").is_some() => ts_object(value)?,
        None => "unknown".into(),
    };
    if let Some(parts) = value.get("allOf").and_then(Value::as_array) {
        let mut members = vec![base];
        for part in parts {
            if part.get("if").is_none() {
                members.push(ts_type(part)?);
            }
        }
        return Ok(members
            .into_iter()
            .filter(|member| member != "unknown")
            .collect::<Vec<_>>()
            .join(" & "));
    }
    Ok(base)
}

fn ts_object(value: &Value) -> Result<String> {
    let required = value["required"].as_array();
    let mut fields = Vec::new();
    if let Some(properties) = value["properties"].as_object() {
        for (name, definition) in properties {
            let optional = if required
                .is_some_and(|items| items.iter().any(|item| item.as_str() == Some(name)))
            {
                ""
            } else {
                "?"
            };
            fields.push(format!("  {name}{optional}: {};", ts_type(definition)?));
        }
    }
    if let Some(extra) = value.get("additionalProperties")
        && extra.is_object()
    {
        if fields.is_empty() {
            fields.push(format!("  [key: string]: {};", ts_type(extra)?));
        } else {
            // TypeScript requires an index value to accept every named field.
            fields.push("  [key: string]: unknown;".into());
        }
    }
    Ok(format!("{{\n{}\n}}", fields.join("\n")))
}

fn ts_conditional_body(value: &Value) -> Result<Option<String>> {
    let Some(parts) = value.get("allOf").and_then(Value::as_array) else {
        return Ok(None);
    };
    let mut variants = Vec::new();
    for part in parts {
        let Some(discriminator) = part["if"]["properties"]["type"]
            .get("const")
            .or_else(|| part["if"]["properties"]["type"].get("enum"))
        else {
            continue;
        };
        let Some(body) = part["then"]["properties"].get("body") else {
            continue;
        };
        let types = if let Some(items) = discriminator.as_array() {
            items.clone()
        } else {
            vec![discriminator.clone()]
        };
        for kind in types {
            variants.push(format!("{{ type: {}; body: {} }}", kind, ts_type(body)?));
        }
    }
    if variants.is_empty() {
        Ok(None)
    } else {
        Ok(Some(format!(
            "Omit<{}, 'type' | 'body'> & ({})",
            ts_object(value)?,
            variants.join(" | ")
        )))
    }
}

fn typescript_models(schema: &Value, operations: &Value, digest: &str) -> Result<String> {
    let defs = schema["$defs"].as_object().context("schema definitions")?;
    let mut out = format!(
        "// @generated by st3-client-codegen; contract sha256 {digest}\n// Source: docs/st3/client-v0/schemas/client-v0.schema.json\n\nexport const CONTRACT_SHA256 = '{digest}' as const;\nexport const API_VERSION = 'st3.client.v0' as const;\n\n"
    );
    for (name, definition) in defs {
        if name == "ActionRequest" {
            continue;
        }
        let shape = ts_conditional_body(definition)?.unwrap_or(ts_type(definition)?);
        writeln!(out, "export type {name} = {shape};\n")?;
    }
    let actions = operations["actions"]
        .as_object()
        .context("actions object")?;
    let branches = schema["$defs"]["ActionRequest"]["oneOf"]
        .as_array()
        .context("ActionRequest.oneOf")?;
    let mut variants = Vec::new();
    for (action, definition) in actions {
        let branch = branches
            .iter()
            .find(|branch| {
                let kind = &branch["properties"]["type"];
                kind["const"].as_str() == Some(action)
                    || kind["enum"]
                        .as_array()
                        .is_some_and(|items| items.iter().any(|item| item.as_str() == Some(action)))
            })
            .with_context(|| format!("ActionRequest branch for `{action}`"))?;
        let parameter_name = action_parameter(action, definition)?;
        let parameter = if defs.contains_key(parameter_name) {
            parameter_name.to_owned()
        } else {
            ts_type(&branch["properties"]["parameters"])?
        };
        let mut fence = "Fence".to_owned();
        if let Some(required) = branch["properties"]["fence"]["allOf"]
            .as_array()
            .and_then(|parts| parts.iter().find_map(|part| part["required"].as_array()))
        {
            let keys = required
                .iter()
                .filter_map(Value::as_str)
                .map(|key| format!("'{key}'"))
                .collect::<Vec<_>>();
            if !keys.is_empty() {
                fence = format!("Fence & Required<Pick<Fence, {}>>", keys.join(" | "));
            }
        }
        variants.push(format!("  (Omit<ActionCommon, 'type' | 'parameters' | 'fence'> & {{ type: '{action}'; parameters: {parameter}; fence: {fence} }})"));
    }
    writeln!(
        out,
        "export type ActionRequest =\n{};",
        variants.join(" |\n")
    )?;
    writeln!(
        out,
        "export type ActionOf<T extends ActionRequest['type']> = Extract<ActionRequest, {{ type: T }}>;"
    )?;
    writeln!(
        out,
        "export type EnvelopeOf<T> = Omit<Envelope, 'value'> & {{ value: T }};"
    )?;
    Ok(out)
}

fn typescript_operation_methods(
    reads: &[Value],
    actions: &serde_json::Map<String, Value>,
) -> Result<String> {
    let mut out = String::new();
    for read in reads {
        let id = read["id"].as_str().context("read id")?;
        let path = read["path"].as_str().context("read path")?;
        let response = read["response"].as_str().context("read response")?;
        let method = lower_camel(&pascal(id));
        if id == "capabilities.get" {
            continue;
        }
        let route = if path.contains("{id}") {
            path.replace("{id}", "${encodeURIComponent(routedId(id))}")
        } else {
            path.to_owned()
        };
        if id == "events.list" {
            writeln!(
                out,
                "    async {method}(options: EventOptions = {{}}): Promise<EnvelopeOf<{response}>> {{ return this.get('{route}' + query(options), 'events'); }}"
            )?;
        } else if id == "timeline.list" || id == "terminal.screen" || id.ends_with(".get") {
            let query_suffix = if id == "timeline.list" {
                " + query(options)"
            } else {
                ""
            };
            let option_arg = if id == "timeline.list" {
                ", options: PageOptions = {}"
            } else {
                ""
            };
            writeln!(
                out,
                "    async {method}(id: string{option_arg}): Promise<EnvelopeOf<{response}>> {{ return this.get(`{route}`{query_suffix}); }}"
            )?;
        } else if id.starts_with("launch-") {
            writeln!(
                out,
                "    async {method}(id: string, options: PageOptions = {{}}): Promise<EnvelopeOf<{response}>> {{ return this.get(`{route}` + query(options)); }}"
            )?;
        } else {
            writeln!(
                out,
                "    async {method}(options: ListOptions = {{}}): Promise<EnvelopeOf<{response}>> {{ return this.get('{route}' + query(options)); }}"
            )?;
        }
    }
    for action in actions.keys() {
        let method = lower_camel(&pascal(action));
        writeln!(
            out,
            "    async {method}(input: Omit<ActionOf<'{action}'>, 'api_version' | 'type'>): Promise<EnvelopeOf<ActionResult>> {{ return this.submitAction({{ ...input, api_version: API_VERSION, type: '{action}' }} as ActionOf<'{action}'>); }}"
        )?;
    }
    Ok(out.trim_end().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_output(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "st3-client-codegen-{}-{nonce}-{name}",
            std::process::id()
        ))
    }

    #[test]
    fn check_rejects_model_and_route_drift_and_generation_restores_stable_bytes() -> Result<()> {
        let operations: Value = serde_json::from_str(include_str!(
            "../../../docs/st3/client-v0/schemas/operations.json"
        ))?;
        let actions = operations["actions"].as_object().context("actions")?;
        let reads = operations["reads"].as_array().context("reads")?;
        let schema: Value = serde_json::from_str(include_str!(
            "../../../docs/st3/client-v0/schemas/client-v0.schema.json"
        ))?;
        let digest = hex_digest(&[
            include_bytes!("../../../docs/st3/client-v0/schemas/client-v0.schema.json"),
            include_bytes!("../../../docs/st3/client-v0/schemas/operations.json"),
        ]);
        let ts_models = typescript_models(&schema, &operations, &digest)?;
        let ts_client = render_marker(
            TYPESCRIPT_CLIENT_TEMPLATE,
            "    // @st3-codegen:typescript-operation-methods",
            &typescript_operation_methods(reads, actions)?,
        )?;
        let models = format_rust(&render_marker(
            RUST_MODELS_TEMPLATE,
            "    // @st3-codegen:rust-action-constructors",
            &rust_action_constructors(actions)?,
        )?)?;
        let client = format_rust(&render_marker(
            RUST_CLIENT_TEMPLATE,
            "    // @st3-codegen:rust-operation-methods",
            &rust_operation_methods(reads, actions)?,
        )?)?;
        let cases = [
            (
                "generated.rs",
                models.as_str(),
                models.replacen("pub attention_id: String,", "", 1),
            ),
            (
                "lib.rs",
                client.as_str(),
                client.replacen(
                    "pub async fn attention_list(",
                    "async fn removed_attention_list(",
                    1,
                ),
            ),
            (
                "Models.generated.ts",
                ts_models.as_str(),
                ts_models.replacen(
                    "export type ActionRequest =",
                    "export type MissingActionRequest =",
                    1,
                ),
            ),
            (
                "Client.generated.ts",
                ts_client.as_str(),
                ts_client.replacen("async eventsList(", "async removedEventsList(", 1),
            ),
        ];
        for (name, expected, drifted) in cases {
            assert_ne!(
                expected, drifted,
                "test mutation must change generated output"
            );
            let path = temporary_output(name);
            output(&path, &drifted, false)?;
            assert!(output(&path, expected, true).is_err());
            output(&path, expected, false)?;
            let first = std::fs::read(&path)?;
            output(&path, expected, false)?;
            let second = std::fs::read(&path)?;
            assert_eq!(first, second, "second generation must be byte-identical");
            output(&path, expected, true)?;
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }
}
