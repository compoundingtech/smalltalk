//! The Codex app-server protocol admission gate.
//!
//! Moved verbatim out of the parent module: the version probe and the JSON-schema walk that
//! decides whether the installed app-server speaks the protocol this build was written for.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{Context as _, Result};
use serde_json::Value;

use super::{
    CODEX_PROVIDER_AUTH_REJECTED, REQUIRED_CODEX_CLIENT_NOTIFICATIONS,
    REQUIRED_CODEX_CLIENT_REQUESTS, REQUIRED_CODEX_SERVER_NOTIFICATIONS,
};

pub(super) struct CodexProtocolSchemas {
    pub(super) protocol: Value,
    pub(super) client_requests: Value,
    pub(super) client_notifications: Value,
    pub(super) server_requests: Value,
    pub(super) server_notifications: Value,
}

/// Admit the installed Codex app-server protocol, returning the version that passed.
pub(super) fn ensure_supported_protocol(codex: &str) -> Result<String> {
    let version = codex_version(codex)?;
    let generated = tempfile::Builder::new()
        .prefix("st2-codex-protocol-")
        .tempdir()
        .context("creating a temporary Codex protocol schema directory")?;
    let output = Command::new(codex)
        .args([
            "app-server",
            "generate-json-schema",
            "--experimental",
            "--out",
        ])
        .arg(generated.path())
        .output()
        .with_context(|| format!("generating the Codex app-server schema from {codex}"))?;
    anyhow::ensure!(
        output.status.success(),
        "{codex} app-server schema generation failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let read_schema = |name: &str| -> Result<Value> {
        let path = generated.path().join(name);
        let bytes =
            fs::read(&path).with_context(|| format!("reading generated Codex schema {name}"))?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing generated Codex schema {name}"))
    };
    let schemas = CodexProtocolSchemas {
        protocol: read_schema("codex_app_server_protocol.v2.schemas.json")?,
        client_requests: read_schema("ClientRequest.json")?,
        client_notifications: read_schema("ClientNotification.json")?,
        server_requests: read_schema("ServerRequest.json")?,
        server_notifications: read_schema("ServerNotification.json")?,
    };
    verify_codex_protocol_schemas(&schemas)
        .with_context(|| format!("Codex app-server schema from {version} is incompatible"))?;
    Ok(version)
}

fn codex_version(codex: &str) -> Result<String> {
    let mut attempt_index = 0;
    let output = loop {
        let attempt = Command::new(codex).arg("--version").output();
        match attempt {
            Ok(output) => break output,
            Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) && attempt_index + 1 < 5 => {
                // Some Linux filesystems briefly retain writer exclusion after a binary install.
                // Retry only this transient error and keep every other launch error immediate.
                attempt_index += 1;
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                return Err(error).with_context(|| format!("reading Codex version from {codex}"));
            }
        }
    };
    anyhow::ensure!(
        output.status.success(),
        "{codex} --version failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let actual = String::from_utf8(output.stdout)
        .context("Codex version output is not UTF-8")?
        .trim()
        .to_string();
    anyhow::ensure!(!actual.is_empty(), "{codex} --version printed nothing");
    Ok(actual)
}

pub(super) fn verify_codex_protocol_schemas(schemas: &CodexProtocolSchemas) -> Result<()> {
    let definitions = schemas
        .protocol
        .get("definitions")
        .and_then(Value::as_object)
        .context("aggregate schema has no definitions object")?;

    require_methods(
        &schemas.client_requests,
        REQUIRED_CODEX_CLIENT_REQUESTS,
        "client request",
    )?;
    require_methods(
        &schemas.client_notifications,
        REQUIRED_CODEX_CLIENT_NOTIFICATIONS,
        "client notification",
    )?;
    require_methods(
        &schemas.server_notifications,
        REQUIRED_CODEX_SERVER_NOTIFICATIONS,
        "server notification",
    )?;
    schema_methods(&schemas.server_requests, "server request")?;

    let status_variants = schema_variants(definitions, "ThreadStatus", "type")?;
    for status in ["notLoaded", "idle", "systemError", "active"] {
        anyhow::ensure!(
            status_variants.contains_key(status),
            "ThreadStatus has no '{status}' variant"
        );
    }
    let active = status_variants
        .get("active")
        .context("ThreadStatus has no active variant")?;
    let active_flags =
        required_property(definitions, active, "activeFlags", "ThreadStatus.active")?;
    let active_flag = require_array(definitions, active_flags, "ThreadStatus.activeFlags")?;
    anyhow::ensure!(
        active_flag == schema_definition(definitions, "ThreadActiveFlag")?,
        "ThreadStatus.activeFlags does not contain ThreadActiveFlag"
    );
    let actual_active_flags = schema_enum(definitions, "ThreadActiveFlag")?;
    anyhow::ensure!(
        actual_active_flags == string_set(&["waitingOnApproval", "waitingOnUserInput"]),
        "ThreadActiveFlag changed: {}",
        actual_active_flags
            .into_iter()
            .collect::<Vec<_>>()
            .join(", ")
    );

    let item_variants = schema_variants(definitions, "ThreadItem", "type")?;
    for item in [
        "contextCompaction",
        "enteredReviewMode",
        "exitedReviewMode",
        "userMessage",
    ] {
        anyhow::ensure!(
            item_variants.contains_key(item),
            "ThreadItem has no '{item}' variant"
        );
    }
    let user_message = item_variants
        .get("userMessage")
        .context("ThreadItem has no userMessage variant")?;
    require_property_type(
        definitions,
        user_message,
        "clientId",
        "string",
        false,
        "ThreadItem.userMessage",
    )?;

    let user_input_variants = schema_variants(definitions, "UserInput", "type")?;
    let text_input = user_input_variants
        .get("text")
        .context("UserInput has no text variant")?;
    require_property_type(
        definitions,
        text_input,
        "text",
        "string",
        true,
        "UserInput.text",
    )?;
    let text_elements = property(definitions, text_input, "text_elements", "UserInput.text")?;
    require_array(definitions, text_elements, "UserInput.text.text_elements")?;

    for (definition, path) in [
        ("ClientInfo", &["name"][..]),
        ("ClientInfo", &["version"][..]),
        ("Thread", &["id"][..]),
        ("Turn", &["id"][..]),
        ("ThreadStatusChangedNotification", &["threadId"][..]),
        ("TurnStartedNotification", &["threadId"][..]),
        ("TurnStartedNotification", &["turn", "id"][..]),
        ("TurnCompletedNotification", &["threadId"][..]),
        ("TurnCompletedNotification", &["turn", "id"][..]),
        ("ItemStartedNotification", &["threadId"][..]),
        ("ItemStartedNotification", &["turnId"][..]),
        ("ItemCompletedNotification", &["threadId"][..]),
        ("ItemCompletedNotification", &["turnId"][..]),
        ("ThreadStartedNotification", &["thread", "id"][..]),
        ("ThreadResumeParams", &["threadId"][..]),
        ("ThreadResumeResponse", &["thread", "id"][..]),
        ("TurnStartParams", &["threadId"][..]),
        ("TurnStartResponse", &["turn", "id"][..]),
        ("TurnSteerParams", &["threadId"][..]),
        ("TurnSteerParams", &["expectedTurnId"][..]),
        ("TurnSteerResponse", &["turnId"][..]),
    ] {
        let schema = required_schema_path(definitions, definition, path)?;
        require_type(
            definitions,
            schema,
            "string",
            &format!("{definition}.{}", path.join(".")),
        )?;
    }

    require_property_type(
        definitions,
        schema_definition(definitions, "ClientInfo")?,
        "title",
        "string",
        false,
        "ClientInfo",
    )?;
    require_property_type(
        definitions,
        schema_definition(definitions, "InitializeCapabilities")?,
        "experimentalApi",
        "boolean",
        false,
        "InitializeCapabilities",
    )?;
    required_schema_path(definitions, "InitializeParams", &["clientInfo"])?;

    let thread_status = required_schema_path(definitions, "Thread", &["status"])?;
    anyhow::ensure!(
        thread_status == schema_definition(definitions, "ThreadStatus")?,
        "Thread.status does not use ThreadStatus"
    );
    let resume_status =
        required_schema_path(definitions, "ThreadResumeResponse", &["thread", "status"])?;
    anyhow::ensure!(
        resume_status == schema_definition(definitions, "ThreadStatus")?,
        "ThreadResumeResponse.thread.status does not use ThreadStatus"
    );
    let started_status = required_schema_path(
        definitions,
        "ThreadStartedNotification",
        &["thread", "status"],
    )?;
    anyhow::ensure!(
        started_status == schema_definition(definitions, "ThreadStatus")?,
        "ThreadStartedNotification.thread.status does not use ThreadStatus"
    );
    let changed_status =
        required_schema_path(definitions, "ThreadStatusChangedNotification", &["status"])?;
    anyhow::ensure!(
        changed_status == schema_definition(definitions, "ThreadStatus")?,
        "ThreadStatusChangedNotification.status does not use ThreadStatus"
    );

    let turns = required_schema_path(definitions, "Thread", &["turns"])?;
    let turn = require_array(definitions, turns, "Thread.turns")?;
    anyhow::ensure!(
        turn == schema_definition(definitions, "Turn")?,
        "Thread.turns does not contain Turn"
    );
    let items = required_schema_path(definitions, "Turn", &["items"])?;
    let item = require_array(definitions, items, "Turn.items")?;
    anyhow::ensure!(
        item == schema_definition(definitions, "ThreadItem")?,
        "Turn.items does not contain ThreadItem"
    );
    // The typed turn result the provider-credential classifier reads. A release that renames the
    // status word, drops the failure's typed error, or merges the credential arm into a quota arm
    // must refuse the launch rather than let st2 silently stop classifying rejections — or, worse,
    // report an exhausted allowance as a rejected credential.
    let turn_status = required_schema_path(definitions, "Turn", &["status"])?;
    anyhow::ensure!(
        turn_status == schema_definition(definitions, "TurnStatus")?,
        "Turn.status does not use TurnStatus"
    );
    let turn_statuses = schema_enum(definitions, "TurnStatus")?;
    for status in ["completed", "failed"] {
        anyhow::ensure!(
            turn_statuses.contains(status),
            "TurnStatus has no '{status}' variant"
        );
    }
    let turn_error = nullable_schema(
        definitions,
        property(
            definitions,
            schema_definition(definitions, "Turn")?,
            "error",
            "Turn",
        )?,
        "Turn.error",
    )?;
    anyhow::ensure!(
        turn_error == schema_definition(definitions, "TurnError")?,
        "Turn.error does not use TurnError"
    );
    let error_info = nullable_schema(
        definitions,
        property(
            definitions,
            schema_definition(definitions, "TurnError")?,
            "codexErrorInfo",
            "TurnError",
        )?,
        "TurnError.codexErrorInfo",
    )?;
    anyhow::ensure!(
        error_info == schema_definition(definitions, "CodexErrorInfo")?,
        "TurnError.codexErrorInfo does not use CodexErrorInfo"
    );
    let error_words = schema_variant_words(definitions, "CodexErrorInfo")?;
    for word in [
        CODEX_PROVIDER_AUTH_REJECTED,
        "rateLimitExceeded",
        "usageLimitExceeded",
    ] {
        anyhow::ensure!(
            error_words.contains(word),
            "CodexErrorInfo has no '{word}' word"
        );
    }
    for notification in ["ItemStartedNotification", "ItemCompletedNotification"] {
        let item = required_schema_path(definitions, notification, &["item"])?;
        anyhow::ensure!(
            item == schema_definition(definitions, "ThreadItem")?,
            "{notification}.item does not use ThreadItem"
        );
    }

    for params in ["TurnStartParams", "TurnSteerParams"] {
        let input = required_schema_path(definitions, params, &["input"])?;
        let input_item = require_array(definitions, input, &format!("{params}.input"))?;
        anyhow::ensure!(
            input_item == schema_definition(definitions, "UserInput")?,
            "{params}.input does not contain UserInput"
        );
        require_property_type(
            definitions,
            schema_definition(definitions, params)?,
            "clientUserMessageId",
            "string",
            false,
            params,
        )?;
    }
    let loaded = required_schema_path(definitions, "ThreadLoadedListResponse", &["data"])?;
    let loaded_item = require_array(definitions, loaded, "ThreadLoadedListResponse.data")?;
    require_type(
        definitions,
        loaded_item,
        "string",
        "ThreadLoadedListResponse.data item",
    )?;
    let hook_cwds = property(
        definitions,
        schema_definition(definitions, "HooksListParams")?,
        "cwds",
        "HooksListParams",
    )?;
    let hook_cwd = require_array(definitions, hook_cwds, "HooksListParams.cwds")?;
    require_type(definitions, hook_cwd, "string", "HooksListParams.cwds item")?;
    verify_hook_schema(definitions)?;
    Ok(())
}

fn verify_hook_schema(definitions: &serde_json::Map<String, Value>) -> Result<()> {
    let data = required_schema_path(definitions, "HooksListResponse", &["data"])?;
    let entry = require_array(definitions, data, "HooksListResponse.data")?;
    anyhow::ensure!(
        entry == schema_definition(definitions, "HooksListEntry")?,
        "HooksListResponse.data does not contain HooksListEntry"
    );
    let hooks = required_schema_path(definitions, "HooksListEntry", &["hooks"])?;
    let hook = require_array(definitions, hooks, "HooksListEntry.hooks")?;
    anyhow::ensure!(
        hook == schema_definition(definitions, "HookMetadata")?,
        "HooksListEntry.hooks does not contain HookMetadata"
    );
    for (property, expected_type) in [
        ("currentHash", "string"),
        ("isManaged", "boolean"),
        ("key", "string"),
    ] {
        require_property_type(
            definitions,
            schema_definition(definitions, "HookMetadata")?,
            property,
            expected_type,
            true,
            "HookMetadata",
        )?;
    }
    let trust_status = required_schema_path(definitions, "HookMetadata", &["trustStatus"])?;
    anyhow::ensure!(
        trust_status == schema_definition(definitions, "HookTrustStatus")?,
        "HookMetadata.trustStatus does not use HookTrustStatus"
    );
    let statuses = schema_enum(definitions, "HookTrustStatus")?;
    anyhow::ensure!(
        statuses == string_set(&["managed", "modified", "trusted", "untrusted"]),
        "HookTrustStatus changed: {}",
        statuses.into_iter().collect::<Vec<_>>().join(", ")
    );
    Ok(())
}

fn string_set(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

fn schema_methods(schema: &Value, label: &str) -> Result<BTreeSet<String>> {
    let arms = schema
        .get("oneOf")
        .and_then(Value::as_array)
        .with_context(|| format!("{label} schema has no oneOf array"))?;
    let mut methods = BTreeSet::new();
    for arm in arms {
        let required = arm
            .get("required")
            .and_then(Value::as_array)
            .with_context(|| format!("{label} arm has no required array"))?;
        anyhow::ensure!(
            required
                .iter()
                .any(|value| value.as_str() == Some("method")),
            "{label} arm does not require method"
        );
        let values = arm
            .pointer("/properties/method/enum")
            .and_then(Value::as_array)
            .with_context(|| format!("{label} arm has no method enum"))?;
        anyhow::ensure!(values.len() == 1, "{label} arm method enum is not exact");
        let method = values[0]
            .as_str()
            .with_context(|| format!("{label} arm method is not a string"))?;
        anyhow::ensure!(
            methods.insert(method.to_string()),
            "{label} method '{method}' is duplicated"
        );
    }
    Ok(methods)
}

fn require_methods(schema: &Value, required: &[&str], label: &str) -> Result<()> {
    let methods = schema_methods(schema, label)?;
    let missing = string_set(required)
        .difference(&methods)
        .cloned()
        .collect::<Vec<_>>();
    anyhow::ensure!(
        missing.is_empty(),
        "missing {label} methods: {}",
        missing.join(", ")
    );
    Ok(())
}

fn schema_definition<'a>(
    definitions: &'a serde_json::Map<String, Value>,
    name: &str,
) -> Result<&'a Value> {
    definitions
        .get(name)
        .with_context(|| format!("aggregate schema has no {name} definition"))
}

fn resolve_schema<'a>(
    definitions: &'a serde_json::Map<String, Value>,
    mut schema: &'a Value,
) -> Result<&'a Value> {
    for _ in 0..16 {
        if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
            let name = reference
                .strip_prefix("#/definitions/")
                .with_context(|| format!("unsupported schema reference '{reference}'"))?;
            schema = schema_definition(definitions, name)?;
            continue;
        }
        if let Some(all_of) = schema.get("allOf").and_then(Value::as_array) {
            anyhow::ensure!(all_of.len() == 1, "schema allOf is not a single reference");
            schema = &all_of[0];
            continue;
        }
        return Ok(schema);
    }
    anyhow::bail!("schema reference depth exceeds 16")
}

/// Resolve `anyOf: [T, null]` — the shape the Codex generator emits for an optional typed field —
/// to `T`. A field that is not exactly one typed arm beside `null` is refused rather than guessed.
fn nullable_schema<'a>(
    definitions: &'a serde_json::Map<String, Value>,
    schema: &'a Value,
    label: &str,
) -> Result<&'a Value> {
    let schema = resolve_schema(definitions, schema)?;
    let arms = schema
        .get("anyOf")
        .and_then(Value::as_array)
        .with_context(|| format!("{label} is not a nullable schema"))?;
    let mut typed = arms
        .iter()
        .filter(|arm| arm.get("type").and_then(Value::as_str) != Some("null"));
    let only = typed
        .next()
        .with_context(|| format!("{label} has no typed arm"))?;
    anyhow::ensure!(
        typed.next().is_none(),
        "{label} has more than one typed arm"
    );
    resolve_schema(definitions, only)
}

/// Every unit word of a `oneOf` union that mixes a string enum with data-carrying object arms —
/// the shape `CodexErrorInfo` has. Only the enum arms carry words st2 can match on.
fn schema_variant_words(
    definitions: &serde_json::Map<String, Value>,
    definition: &str,
) -> Result<BTreeSet<String>> {
    let arms = schema_definition(definitions, definition)?
        .get("oneOf")
        .and_then(Value::as_array)
        .with_context(|| format!("{definition} has no oneOf variants"))?;
    let mut words = BTreeSet::new();
    for arm in arms {
        let Some(values) = resolve_schema(definitions, arm)?
            .get("enum")
            .and_then(Value::as_array)
        else {
            continue;
        };
        for value in values {
            let word = value
                .as_str()
                .with_context(|| format!("{definition} has a non-string enum value"))?;
            words.insert(word.to_string());
        }
    }
    anyhow::ensure!(!words.is_empty(), "{definition} has no enum words");
    Ok(words)
}

fn schema_variants<'a>(
    definitions: &'a serde_json::Map<String, Value>,
    definition: &str,
    discriminator: &str,
) -> Result<BTreeMap<String, &'a Value>> {
    let schema = schema_definition(definitions, definition)?;
    let variants = schema
        .get("oneOf")
        .and_then(Value::as_array)
        .with_context(|| format!("{definition} has no oneOf variants"))?;
    let mut found = BTreeMap::new();
    for variant in variants {
        let variant = resolve_schema(definitions, variant)?;
        let required = variant
            .get("required")
            .and_then(Value::as_array)
            .with_context(|| format!("{definition} variant has no required array"))?;
        anyhow::ensure!(
            required
                .iter()
                .any(|value| value.as_str() == Some(discriminator)),
            "{definition} variant does not require {discriminator}"
        );
        let values = variant
            .pointer(&format!("/properties/{discriminator}/enum"))
            .and_then(Value::as_array)
            .with_context(|| format!("{definition} variant has no {discriminator} enum"))?;
        anyhow::ensure!(
            values.len() == 1,
            "{definition} variant discriminator is not exact"
        );
        let value = values[0]
            .as_str()
            .with_context(|| format!("{definition} discriminator is not a string"))?;
        anyhow::ensure!(
            found.insert(value.to_string(), variant).is_none(),
            "{definition} discriminator '{value}' is duplicated"
        );
    }
    Ok(found)
}

fn schema_enum(
    definitions: &serde_json::Map<String, Value>,
    definition: &str,
) -> Result<BTreeSet<String>> {
    let values = schema_definition(definitions, definition)?
        .get("enum")
        .and_then(Value::as_array)
        .with_context(|| format!("{definition} has no enum"))?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .with_context(|| format!("{definition} has a non-string enum value"))
        })
        .collect()
}

fn property<'a>(
    definitions: &'a serde_json::Map<String, Value>,
    schema: &'a Value,
    name: &str,
    label: &str,
) -> Result<&'a Value> {
    let schema = resolve_schema(definitions, schema)?;
    let property = schema
        .get("properties")
        .and_then(Value::as_object)
        .and_then(|properties| properties.get(name))
        .with_context(|| format!("{label} has no {name} property"))?;
    resolve_schema(definitions, property)
}

fn required_property<'a>(
    definitions: &'a serde_json::Map<String, Value>,
    schema: &'a Value,
    name: &str,
    label: &str,
) -> Result<&'a Value> {
    let schema = resolve_schema(definitions, schema)?;
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .with_context(|| format!("{label} has no required array"))?;
    anyhow::ensure!(
        required.iter().any(|value| value.as_str() == Some(name)),
        "{label} does not require {name}"
    );
    property(definitions, schema, name, label)
}

fn required_schema_path<'a>(
    definitions: &'a serde_json::Map<String, Value>,
    definition: &str,
    path: &[&str],
) -> Result<&'a Value> {
    let mut schema = schema_definition(definitions, definition)?;
    let mut label = definition.to_string();
    for component in path {
        schema = required_property(definitions, schema, component, &label)?;
        label.push('.');
        label.push_str(component);
    }
    Ok(schema)
}

fn require_property_type(
    definitions: &serde_json::Map<String, Value>,
    schema: &Value,
    property_name: &str,
    expected_type: &str,
    required: bool,
    label: &str,
) -> Result<()> {
    let property = if required {
        required_property(definitions, schema, property_name, label)?
    } else {
        property(definitions, schema, property_name, label)?
    };
    require_type(
        definitions,
        property,
        expected_type,
        &format!("{label}.{property_name}"),
    )
}

fn require_type(
    definitions: &serde_json::Map<String, Value>,
    schema: &Value,
    expected: &str,
    label: &str,
) -> Result<()> {
    let schema = resolve_schema(definitions, schema)?;
    let matches = match schema.get("type") {
        Some(Value::String(actual)) => actual == expected,
        Some(Value::Array(actual)) => actual.iter().any(|value| value.as_str() == Some(expected)),
        _ => false,
    };
    anyhow::ensure!(matches, "{label} does not accept {expected}");
    Ok(())
}

fn require_array<'a>(
    definitions: &'a serde_json::Map<String, Value>,
    schema: &'a Value,
    label: &str,
) -> Result<&'a Value> {
    let schema = resolve_schema(definitions, schema)?;
    require_type(definitions, schema, "array", label)?;
    let items = schema
        .get("items")
        .with_context(|| format!("{label} has no item schema"))?;
    resolve_schema(definitions, items)
}
