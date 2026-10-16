//! Closed-tool MCP adapter for the paired client gateway.
//! HTTP and stdio share the same handler and input contracts; neither exposes daemon control.
use axum::{Router, http::StatusCode, routing::get};
use rmcp::{ServerHandler, RoleServer, ErrorData, model::*, service::RequestContext};
use rmcp::transport::streamable_http_server::{StreamableHttpService, StreamableHttpServerConfig, session::local::LocalSessionManager};
use serde_json::{Value, json};
use std::{io::{self, Read}, path::Path, os::unix::fs::{OpenOptionsExt, MetadataExt}};

#[derive(Clone)]
pub struct Adapter { client: reqwest::Client, bearer: String }
impl Adapter {
    pub fn new(socket: &Path, credential_file: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        if socket.file_name().and_then(|name| name.to_str()) != Some("st3-client.sock") {
            return Err("only the paired gateway st3-client.sock is permitted".into());
        }
        let bearer = read_credential(credential_file)?;
        let client = reqwest::Client::builder().unix_socket(socket).build()?;
        Ok(Self { client, bearer })
    }
}

/// Read an owner-only regular file without following a final-component symlink.
/// Check the opened descriptor, not path metadata, and bound reads even if the file grows.
fn read_credential(path: &Path) -> io::Result<String> {
    const MAX_BYTES: u64 = 4096;
    let file = std::fs::OpenOptions::new().read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC).open(path)?;
    let metadata = file.metadata()?;
    // SAFETY: geteuid takes no arguments and has no memory-safety preconditions.
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_file() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 || metadata.len() > MAX_BYTES {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied,
            "credential must be an owner-only regular file of at most 4096 bytes"));
    }
    let mut source = String::new();
    file.take(MAX_BYTES + 1).read_to_string(&mut source)?;
    let bearer = source.trim();
    if source.len() > MAX_BYTES as usize || bearer.is_empty()
        || !bearer.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "credential must contain one nonempty ASCII bearer token"));
    }
    Ok(bearer.to_owned())
}

const TOOLS: &[(&str, &str, bool)] = &[
    ("st_alerts_list", "List owner alert identifiers and counts.", true),
    ("st_alert_get", "Read one exact owner alert, including its episode and named answer options.", true),
    ("st_alert_answer", "Propose one typed person-ask answer. Requires separate owner approval in st; retry identical arguments after approval.", false),
    ("st_missions_list", "List mission metadata.", true),
    ("st_agents_list", "List agent metadata.", true),
    ("st_conversation_read", "Read one bounded conversation content range. Content may contain sensitive material.", true),
    ("st_message_send", "Send one exact message to an owner-allowlisted agent, authorized by grant scope and the ChatGPT confirmation card.", false),
];
fn fence_schema() -> Value {
    json!({"type":"object","description":"Use the snapshot returned by the preceding read. Copy subject revisions from that snapshot to fence writes against changed resources.",
        "properties":{
            "snapshot_id":{"type":"string","description":"Opaque snapshot identifier returned by the paired gateway."},
            "subject_revisions":{"type":"object","description":"Resource subject identifiers mapped to their opaque revision strings; omitted means no subject-specific revisions.","additionalProperties":{"type":"string"}}
        },"required":["snapshot_id"],"additionalProperties":false})
}
fn schema(name: &str) -> Value {
    let props = match name {
        "st_alert_answer" => json!({"action_id":{"type":"string"},"idempotency_key":{"type":"string"},"fence":fence_schema(),"alert_id":{"type":"string"},"episode":{"type":"string"},"option":{"type":"string"},"summary":{"type":"string"}}),
        "st_message_send" => json!({"action_id":{"type":"string"},"idempotency_key":{"type":"string"},"fence":fence_schema(),"to":{"type":"string"},"title":{"type":"string"},"content":{"type":"string"}}),
        "st_alert_get" => json!({"alert_id":{"type":"string"},"max_response_bytes":{"type":"integer","minimum":1,"maximum":16384}}),
        "st_conversation_read" => json!({"agent_id":{"type":"string"},"session_id":{"type":"string"},"reference":{"type":"string"},"revision":{"type":"string"},"start":{"type":"integer","minimum":0},"end":{"type":"integer","minimum":0},"max_response_bytes":{"type":"integer","minimum":1,"maximum":16384}}),
        _ => json!({"limit":{"type":"integer","minimum":1},"cursor":{"type":"string"},"max_response_bytes":{"type":"integer","minimum":1,"maximum":16384}}),
    };
    let required = match name {
        "st_alert_answer" => vec!["action_id","idempotency_key","fence","alert_id","episode","option","summary"],
        "st_message_send" => vec!["action_id","idempotency_key","fence","to","title","content"],
        "st_alert_get" => vec!["alert_id"],
        "st_conversation_read" => vec!["agent_id","session_id","reference","revision","start","end"],
        _ => vec![],
    };
    json!({"type":"object","properties":props,"required":required,"additionalProperties":false})
}

/// Validate the small JSON Schema vocabulary used by the published contracts.
/// The same contracts drive discovery and validation, including nested fence maps.
fn matches_schema(value: &Value, contract: &Value) -> bool {
    match contract["type"].as_str() {
        Some("string") => value.is_string(),
        Some("integer") => value.as_u64().is_some_and(|number| {
            contract["minimum"].as_u64().is_none_or(|minimum| number >= minimum)
                && contract["maximum"].as_u64().is_none_or(|maximum| number <= maximum)
        }),
        Some("object") => value.as_object().is_some_and(|object| {
            let required_present = contract["required"].as_array().is_none_or(|required| {
                required.iter().all(|key| key.as_str().is_some_and(|key| object.contains_key(key)))
            });
            required_present && object.iter().all(|(key, value)| {
                if let Some(property) = contract["properties"].get(key) {
                    matches_schema(value, property)
                } else {
                    match &contract["additionalProperties"] {
                        Value::Bool(false) => false,
                        Value::Object(_) => matches_schema(value, &contract["additionalProperties"]),
                        _ => false,
                    }
                }
            })
        }),
        _ => false,
    }
}
fn validate_arguments(name: &str, args: &Value) -> Result<(), Value> {
    if !TOOLS.iter().any(|(tool, _, _)| *tool == name) {
        return Err(json!({"code":"unknown-tool"}));
    }
    if !matches_schema(args, &schema(name)) {
        return Err(json!({"code":"invalid-arguments","message":"Use the exact published tool schema."}));
    }
    Ok(())
}
fn tool_definitions() -> Vec<Tool> {
    TOOLS.iter().map(|(name,description,read)| serde_json::from_value(json!({
        "name":name,"description":description,"inputSchema":schema(name),
        "annotations":{"readOnlyHint":read,"destructiveHint":*name=="st_alert_answer","idempotentHint":true,"openWorldHint":*name=="st_message_send"}
    })).expect("static tool definition")).collect()
}
async fn call(adapter: &Adapter, name: &str, args: &Value) -> Result<Value,Value> {
    validate_arguments(name, args)?;
    let object = args.as_object().expect("validated tool object");
    let write = matches!(name,"st_alert_answer"|"st_message_send");
    let route = match name {"st_alerts_list"=>"attention-rich","st_alert_get"=>"attention-content","st_missions_list"=>"missions-rich","st_agents_list"=>"agents-rich","st_conversation_read"=>"conversation-content",_=>"propose"};
    let url = format!("http://localhost/v1/client/assistant/{route}");
    let request = if write {
        let parameters = if name == "st_alert_answer" {
            json!({"target_id":args["alert_id"],"episode":args["episode"],"summary":args["summary"],"answer":{"id":args["option"]}})
        } else {json!({"to":args["to"],"title":args["title"],"content":args["content"]})};
        adapter.client.post(url).json(&json!({"api_version":"st3.client.v0","id":args["action_id"],"type":if name=="st_alert_answer" {"work.done"} else {"message.send"},"idempotency_key":args["idempotency_key"],"fence":args["fence"],"parameters":parameters}))
    } else {
        let query: Vec<_> = object.iter().map(|(k,v)| (k.as_str(),v.as_str().map(str::to_owned).unwrap_or_else(||v.to_string()))).collect();
        adapter.client.get(url).query(&query)
    };
    let response = request.bearer_auth(&adapter.bearer).send().await.map_err(|_| json!({"code":"unavailable","message":"The paired st daemon is unavailable."}))?;
    let status = response.status();
    let value = response.json::<Value>().await.map_err(|_|json!({"code":"invalid-daemon-response"}))?;
    if status.is_success() {Ok(value)} else {Err(json!({"code":if status.as_u16()==413 {"incomplete"} else {"daemon-denied"},"http_status":status.as_u16(),"daemon":value}))}
}
impl ServerHandler for Adapter {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("st-chatgpt",env!("CARGO_PKG_VERSION")))
    }
    async fn list_tools(&self, _:Option<PaginatedRequestParams>, _:RequestContext<RoleServer>) -> Result<ListToolsResult,ErrorData> {
        Ok(ListToolsResult { tools: tool_definitions(), ..Default::default() })
    }
    async fn call_tool(&self, request:CallToolRequestParams, _:RequestContext<RoleServer>) -> Result<CallToolResponse,ErrorData> {
        let args=request.arguments.map(Value::Object).unwrap_or_else(||json!({}));
        let (value,error)=match call(self,&request.name,&args).await {Ok(v)=>(v,false),Err(v)=>(v,true)};
        let result=if error {CallToolResult::error(vec![ContentBlock::text(value.to_string())])}
            else {CallToolResult::success(vec![ContentBlock::text(value.to_string())])};
        Ok(result.into())
    }
}
/// Stateless Streamable HTTP: only POST /mcp is a protocol endpoint.
pub fn http_router(adapter: Adapter) -> Router {
    let service=StreamableHttpService::new(move ||Ok(adapter.clone()),std::sync::Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default().with_legacy_session_mode(false).with_json_response(true));
    Router::new().route("/mcp",get(||async {StatusCode::METHOD_NOT_ALLOWED}).post_service(service))
        .fallback(||async {StatusCode::NOT_FOUND})
}

#[cfg(test)]
mod tests;
