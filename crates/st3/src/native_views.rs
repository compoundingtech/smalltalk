//! Deterministic, owner-local projections of OMP/Pi native records.
use serde_json::{Value, json};

fn fields(out: &mut Value, source: &Value, names: &[(&str, &str)]) {
    for &(target, native) in names {
        if let Some(value) = source.get(native).filter(|value| match target {
            "fallback" | "timed_out" | "case" | "hidden" | "gitignore" | "reset" | "truncated" => value.is_boolean(),
            "timeout_s" | "exit_code" | "wall_ms" | "first_changed_line" | "total_ms"
            | "duration_ms" | "tokens" | "cost_usd" | "requests" | "tool_count"
            | "output_bytes" | "tokens_before" | "tokens_after" | "recommended" | "input"
            | "output" | "cache_read" | "cache_write" | "total" | "context_tokens" | "ttft_ms"
            | "limit" | "skip" | "match_count" | "file_count" | "file_limit_reached" | "per_file_limit_reached" => {
                value.is_number()
            }
            "started_at" | "ended_at" => value.is_string() || value.is_number(),
            _ => value.is_string(),
        }) {
            out[target] = value.clone();
        }
    }
}

fn rows(source: &Value, key: &str, project: impl Fn(&Value) -> Value) -> Value {
    Value::Array(
        source
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|v| v.is_object())
            .map(project)
            .collect(),
    )
}

fn items(source: &Value) -> Value {
    Value::Array(
        source
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| {
                if let Some(content) = item.as_str() {
                    return Some(json!({"content":content,"status":"pending"}));
                }
                if !item.is_object() {
                    return None;
                }
                let mut out = json!({});
                fields(
                    &mut out,
                    item,
                    &[("content", "content"), ("status", "status")],
                );
                Some(out)
            })
            .collect(),
    )
}

pub(crate) fn tool_call_view(tool: &str, args: &Value) -> Value {
    let mut out = json!({"type":tool,"tool":tool});
    fields(&mut out, args, &[("intent", "i")]);
    match tool {
        "bash" => {
            fields(
                &mut out,
                args,
                &[
                    ("command", "command"),
                    ("cwd", "cwd"),
                    ("timeout_s", "timeout"),
                ],
            );
            out["background"] = json!(args.get("async").and_then(Value::as_bool).unwrap_or(false));
            if let Some(env) = args.get("env").and_then(Value::as_object) {
                let mut keys: Vec<_> = env.keys().collect();
                keys.sort_unstable();
                out["env_keys"] = json!(keys);
            }
        }
        "edit" => {
            if let Some(input) = args
                .get("input")
                .and_then(Value::as_str)
                .or_else(|| args.as_str())
            {
                out["input_bytes"] = json!(input.len());
                out["ops"] = json!(
                    input
                        .lines()
                        .filter(|line| line.starts_with("PUT ")
                            || line.starts_with("CUT ")
                            || line.starts_with("REM")
                            || line.starts_with("MV "))
                        .count()
                );
                if let Some(path) = input.lines().find_map(|line| {
                    line.strip_prefix('[')
                        .and_then(|line| line.split_once('#'))
                        .map(|(path, _)| path)
                }) {
                    out["path"] = json!(path);
                }
            }
        }
        "write" => {
            fields(&mut out, args, &[("path", "path"), ("content", "content")]);
            if let Some(content) = args.get("content").and_then(Value::as_str) {
                out["bytes"] = json!(content.len());
                out["line_count"] = json!(content.lines().count());
            }
        }
        "read" => {
            fields(&mut out, args, &[("path", "path")]);
            if let Some(path) = args.get("path").and_then(Value::as_str) {
                // URI scheme/authority colons are not file range selectors.
                let start = path.find("://").map_or(0, |n| n + 3);
                let start = path[start..].find('/').map_or(start, |n| start + n);
                if let Some((base, range)) = path[start..].split_once(':') {
                    out["path"] = json!(format!("{}{}", &path[..start], base));
                    out["range"] = json!(range);
                }
            }
        }
        "grep" | "glob" | "web_search" => {
            out["type"] = json!("search");
            out["engine"] = json!(match tool {
                "grep" => "grep",
                "glob" => "glob",
                _ => "web",
            });
            fields(
                &mut out,
                args,
                &[
                    ("pattern", "pattern"), ("path", "path"), ("query", "query"),
                    ("case", "case"), ("hidden", "hidden"), ("gitignore", "gitignore"),
                    ("limit", "limit"), ("skip", "skip"),
                ],
            );
        }
        "todo" => {
            fields(
                &mut out,
                args,
                &[("op", "op"), ("phase", "phase"), ("task", "task")],
            );
            if let Some(value) = args.get("items").filter(|v| v.is_array()) {
                out["items"] = items(value);
            }
        }
        "ask" => {
            out["questions"] = rows(args, "questions", |question| {
                let mut q = json!({"multi":question.get("multi").and_then(Value::as_bool).unwrap_or(false)});
                fields(
                    &mut q,
                    question,
                    &[
                        ("id", "id"),
                        ("question", "question"),
                        ("recommended", "recommended"),
                    ],
                );
                q["options"] = rows(question, "options", |option| {
                    let mut o = json!({});
                    fields(&mut o, option, &[("label", "label"), ("description", "description")]);
                    o
                });
                q
            });
        }
        "task" => {
            fields(&mut out, args, &[("context", "context")]);
            out["tasks"] = rows(args, "tasks", |task| {
                let mut t = json!({});
                fields(
                    &mut t,
                    task,
                    &[("name", "name"), ("agent", "agent"), ("task", "task")],
                );
                t
            });
        }
        "hub" => fields(
            &mut out,
            args,
            &[
                ("op", "op"),
                ("name", "name"),
                ("target", "to"),
                ("target", "target"),
                ("timeout_s", "timeout"),
                ("message", "message"),
            ],
        ),
        "eval" => {
            fields(
                &mut out,
                args,
                &[("language", "language"), ("title", "title"), ("code", "code"), ("timeout_s", "timeout"), ("reset", "reset")],
            );
            if let Some(code) = args.get("code").and_then(Value::as_str) {
                out["code_bytes"] = json!(code.len());
            }
        }
        _ => {
            out["type"] = json!("generic");
            out["name"] = json!(tool);
        }
    }
    out
}

fn job(value: &Value, completed: bool) -> Value {
    let mut out = json!({});
    fields(
        &mut out,
        value,
        &[
            ("id", "jobId"),
            ("id", "id"),
            ("name", "label"),
            ("name", "name"),
            ("type", "type"),
            ("state", "state"),
            ("exit_code", "exitCode"),
            ("started_at", "startedAt"),
            ("ended_at", "exitedAt"),
            ("ended_at", "endedAt"),
            ("duration_ms", "durationMs"),
            ("output_bytes", "outputBytes"),
        ],
    );
    if completed && out.get("state").is_none() {
        out["state"] = json!("completed");
    }
    out
}

fn answer(value: &Value) -> Value {
    let mut out = json!({"selected":[]});
    fields(
        &mut out,
        value,
        &[
            ("question", "question"),
            ("custom", "custom"),
            ("note", "note"),
        ],
    );
    if let Some(selected) = value
        .get("selectedOptions")
        .or_else(|| value.get("selected"))
        .and_then(Value::as_array)
    {
        out["selected"] =
            Value::Array(selected.iter().filter(|v| v.is_string()).cloned().collect());
    }
    out
}

fn search_output(out: &mut Value, tool: &str, details: &Value) {
    out["type"] = json!("search");
    out["engine"] = json!(tool);
    fields(out, details, &[
        ("match_count", "matchCount"), ("file_count", "fileCount"),
        ("truncated", "truncated"), ("file_limit_reached", "fileLimitReached"),
        ("per_file_limit_reached", "perFileLimitReached"), ("warning", "warning"),
    ]);
}

pub(crate) fn tool_output_view(
    tool: &str,
    call_id: &str,
    is_error: bool,
    details: Option<&Value>,
) -> Value {
    let details = details.unwrap_or(&Value::Null);
    let mut out = json!({"type":tool,"tool":tool,"call_id":call_id,"is_error":is_error});
    match tool {
        "bash" => fields(
            &mut out,
            details,
            &[
                ("exit_code", "exitCode"),
                ("wall_ms", "wallTimeMs"),
                ("timeout_s", "timeoutSeconds"),
                ("timed_out", "timedOut"),
            ],
        ),
        "grep" | "glob" => search_output(&mut out, tool, details),
        "edit" => fields(
            &mut out,
            details,
            &[
                ("path", "path"),
                ("first_changed_line", "firstChangedLine"),
                ("diff", "diff"),
            ],
        ),
        "todo" => {
            out["phases"] = rows(details, "phases", |phase| {
                let mut p = json!({});
                fields(&mut p, phase, &[("name", "name")]);
                p["items"] = items(
                    phase
                        .get("tasks")
                        .or_else(|| phase.get("items"))
                        .unwrap_or(&Value::Null),
                );
                p
            })
        }
        "ask" => {
            out["answers"] = if details.get("answers").is_some_and(Value::is_array) {
                rows(details, "answers", answer)
            } else if details.get("question").is_some() {
                json!([answer(details)])
            } else {
                json!([])
            }
        }
        "task" => {
            out["async"] = json!(
                details
                    .get("async")
                    .is_some_and(|v| v.as_bool().unwrap_or(v.is_object()))
            );
            fields(&mut out, details, &[("total_ms", "totalDurationMs")]);
            let key = if details
                .get("progress")
                .and_then(Value::as_array)
                .is_some_and(|v| !v.is_empty())
            {
                "progress"
            } else {
                "results"
            };
            out["agents"] = rows(details, key, |agent| {
                let mut a = json!({});
                fields(
                    &mut a,
                    agent,
                    &[
                        ("id", "id"),
                        ("agent", "agent"),
                        ("name", "id"),
                        ("name", "name"),
                        ("status", "status"),
                        ("task", "task"),
                        ("task", "assignment"),
                        ("duration_ms", "durationMs"),
                        ("tokens", "tokens"),
                        ("cost_usd", "cost"),
                        ("requests", "requests"),
                        ("tool_count", "toolCount"),
                    ],
                );
                a
            });
        }
        "hub" => {
            fields(
                &mut out,
                details,
                &[("op", "op"), ("timed_out", "timedOut"), ("state", "state")],
            );
            if details.get("jobs").is_some_and(Value::is_array) {
                out["jobs"] = rows(details, "jobs", |v| job(v, false));
            }
        }
        _ => {
            out["type"] = json!("generic");
            fields(&mut out, details, &[("wall_ms", "wallTimeMs")]);
        }
    }
    out
}

pub(crate) fn omp_record_view(
    record: &Value,
) -> Option<(&'static str, Value, Option<&'static str>)> {
    let record_type = record.get("type")?.as_str()?;
    let custom = record.get("customType").and_then(Value::as_str);
    let mut out = json!({});
    let mut visibility = None;
    let kind = match (record_type, custom) {
        ("custom_message", _) if record.get("display") == Some(&Value::Bool(false)) => return None,
        ("custom_message", Some("irc:incoming")) => {
            out["type"] = json!("irc");
            fields(
                &mut out,
                &record["details"],
                &[
                    ("from", "from"),
                    ("message", "message"),
                    ("reply_to", "replyTo"),
                    ("message_id", "id"),
                ],
            );
            "irc"
        }
        ("custom_message", Some("launch-completion" | "async-result")) => {
            out["type"] = json!("job");
            let key = if custom == Some("launch-completion") {
                "daemons"
            } else {
                "jobs"
            };
            out["jobs"] = rows(&record["details"], key, |v| job(v, true));
            "job"
        }
        ("custom_message", Some("skill-prompt")) => {
            out["type"] = json!("skill");
            fields(
                &mut out,
                &record["details"],
                &[("name", "name"), ("path", "path"), ("args", "args")],
            );
            "status"
        }
        ("compaction" | "branch_summary", _) => {
            out["type"] = json!("compaction");
            fields(
                &mut out,
                record,
                &[
                    ("method", "method"),
                    ("tokens_before", "tokensBefore"),
                    ("tokens_after", "tokensAfter"),
                    ("short_summary", "shortSummary"),
                    ("summary", "summary"),
                ],
            );
            "status"
        }
        ("model_change", _) => {
            out["type"] = json!("model_change");
            fields(
                &mut out,
                record,
                &[
                    ("model", "model"),
                    ("role", "role"),
                    ("fallback", "resolvedModelIsFallback"),
                ],
            );
            "status"
        }
        ("thinking_level_change", _) => {
            out["type"] = json!("thinking_level");
            fields(
                &mut out,
                record,
                &[("level", "thinkingLevel"), ("configured", "configured")],
            );
            "status"
        }
        ("reset_boundary", _) => {
            out["type"] = json!("reset_boundary");
            "status"
        }
        ("credential_pin", _) => {
            out["type"] = json!("credential_pin");
            fields(&mut out, record, &[("provider", "provider")]);
            visibility = Some("internal");
            "status"
        }
        ("title_change" | "title", _) => {
            out["type"] = json!("title");
            fields(
                &mut out,
                record,
                &[
                    ("title", "title"),
                    ("previous", "previousTitle"),
                    ("source", "source"),
                ],
            );
            "status"
        }
        ("custom", Some("session_exit")) => {
            out["type"] = json!("session_exit");
            fields(
                &mut out,
                &record["data"],
                &[("kind", "kind"), ("reason", "reason")],
            );
            "status"
        }
        ("custom", Some("tool_execution_start")) => {
            out["type"] = json!("tool_start");
            fields(
                &mut out,
                &record["data"],
                &[
                    ("call_id", "toolCallId"),
                    ("tool", "toolName"),
                    ("started_at", "startedAt"),
                ],
            );
            visibility = Some("internal");
            "status"
        }
        _ => return None,
    };
    Some((kind, out, visibility))
}

pub(crate) fn assistant_metadata(message: &Value) -> Option<Value> {
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let mut out = json!({});
    fields(
        &mut out,
        message,
        &[
            ("model", "model"),
            ("provider", "provider"),
            ("stop_reason", "stopReason"),
            ("ttft_ms", "ttft"),
            ("duration_ms", "duration"),
        ],
    );
    let mut usage = json!({});
    fields(
        &mut usage,
        &message["usage"],
        &[
            ("input", "input"),
            ("output", "output"),
            ("cache_read", "cacheRead"),
            ("cache_write", "cacheWrite"),
            ("total", "totalTokens"),
        ],
    );
    fields(
        &mut usage,
        &message["usage"]["cost"],
        &[("cost_usd", "total")],
    );
    if usage.as_object().is_some_and(|v| !v.is_empty()) {
        out["usage"] = usage;
    }
    fields(
        &mut out,
        &message["contextSnapshot"],
        &[("context_tokens", "promptTokens")],
    );
    if out.as_object().is_some_and(|v| !v.is_empty()) {
        Some(out)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_views_parse_each_family_and_keep_only_env_keys() {
        let cases = [
            (
                "bash",
                json!({"command":"echo ok","env":{"TOKEN":"secret"},"async":true}),
                "bash",
            ),
            (
                "edit",
                json!({"input":"[a.rs#ABCD]\nPUT 1.=1:\n+new\n"}),
                "edit",
            ),
            ("write", json!({"path":"a","content":"é"}), "write"),
            ("read", json!({"path":"skill://guide/file:4-8"}), "read"),
            ("grep", json!({"pattern":"needle"}), "search"),
            ("glob", json!({"path":"*.rs"}), "search"),
            ("web_search", json!({"query":"docs"}), "search"),
            ("todo", json!({"op":"add","items":["check"]}), "todo"),
            (
                "ask",
                json!({"questions":[{"id":"q","question":"Which?","options":[{"label":"A"}],"multi":true}]}),
                "ask",
            ),
            (
                "task",
                json!({"tasks":[{"name":"Child","task":"inspect"}]}),
                "task",
            ),
            ("hub", json!({"op":"wait","timeout":2}), "hub"),
            ("eval", json!({"language":"py","code":"é"}), "eval"),
            ("unknown", json!(null), "generic"),
        ];
        for (tool, args, kind) in cases {
            assert_eq!(tool_call_view(tool, &args)["type"], kind);
        }
        assert_eq!(
            tool_call_view("bash", &json!({"env":{"TOKEN":"secret"}}))["env_keys"],
            json!(["TOKEN"])
        );
        assert_eq!(
            tool_call_view("edit", &json!({"input":"[a#ABCD]\nPUT 1.=1:\n+x"}))["ops"],
            1
        );
        assert_eq!(tool_call_view("write", &json!({"content":"é"}))["bytes"], 2);
        assert_eq!(
            tool_call_view("read", &json!({"path":"skill://guide/file:4-8"}))["range"],
            "4-8"
        );
        assert_eq!(
            tool_call_view("eval", &json!({"code":"é"}))["code_bytes"],
            2
        );
    }

    #[test]
    fn output_views_project_each_family() {
        assert_eq!(
            tool_output_view(
                "bash",
                "b",
                true,
                Some(&json!({"exitCode":2,"wallTimeMs":3,"timedOut":true}))
            )["exit_code"],
            2
        );
        assert_eq!(
            tool_output_view(
                "edit",
                "e",
                false,
                Some(&json!({"diff":"@@ change","firstChangedLine":4}))
            )["first_changed_line"],
            4
        );
        assert_eq!(
            tool_output_view(
                "todo",
                "t",
                false,
                Some(
                    &json!({"phases":[{"name":"work","tasks":[{"content":"check","status":"done"}]}]})
                )
            )["phases"][0]["items"][0]["status"],
            "done"
        );
        assert_eq!(
            tool_output_view(
                "ask",
                "a",
                false,
                Some(&json!({"question":"Which?","selectedOptions":["A"]}))
            )["answers"][0]["selected"],
            json!(["A"])
        );
        let task = tool_output_view(
            "task",
            "t",
            false,
            Some(
                &json!({"async":{"state":"running"},"progress":[{"id":"Child","status":"running","cost":0.5}]}),
            ),
        );
        assert_eq!(task["agents"][0]["cost_usd"], 0.5);
        assert_eq!(task["async"], true);
        assert!(task["agents"][0].get("conversation").is_none());
        assert_eq!(
            tool_output_view(
                "hub",
                "h",
                false,
                Some(&json!({"op":"list","jobs":[{"jobId":"j","state":"running"}]}))
            )["jobs"][0]["id"],
            "j"
        );
        assert_eq!(
            tool_output_view("read", "r", true, None),
            json!({"type":"generic","tool":"read","call_id":"r","is_error":true})
        );
    }

    #[test]
    fn record_views_parse_extensions_bookkeeping_and_visibility() {
        let cases = [
            (
                json!({"type":"custom_message","customType":"irc:incoming","details":{"id":"m","from":"Child","message":"hello"}}),
                "irc",
                "irc",
            ),
            (
                json!({"type":"custom_message","customType":"launch-completion","details":{"daemons":[{"id":"j","state":"exited"}]}}),
                "job",
                "job",
            ),
            (
                json!({"type":"custom_message","customType":"async-result","details":{"jobs":[{"jobId":"j"}]}}),
                "job",
                "job",
            ),
            (
                json!({"type":"custom_message","customType":"skill-prompt","details":{"name":"guide"}}),
                "status",
                "skill",
            ),
            (
                json!({"type":"compaction","tokensBefore":10}),
                "status",
                "compaction",
            ),
            (json!({"type":"branch_summary"}), "status", "compaction"),
            (
                json!({"type":"model_change","model":"model"}),
                "status",
                "model_change",
            ),
            (
                json!({"type":"thinking_level_change","thinkingLevel":"high"}),
                "status",
                "thinking_level",
            ),
            (
                json!({"type":"title_change","title":"Work"}),
                "status",
                "title",
            ),
            (
                json!({"type":"custom","customType":"session_exit","data":{"kind":"normal","reason":"done"}}),
                "status",
                "session_exit",
            ),
            (
                json!({"type":"custom","customType":"tool_execution_start","data":{"toolCallId":"a"}}),
                "status",
                "tool_start",
            ),
        ];
        for (record, kind, view_type) in cases {
            let (actual, view, visibility) = omp_record_view(&record).unwrap();
            assert_eq!(actual, kind);
            assert_eq!(view["type"], view_type);
            assert_eq!(
                visibility,
                if view_type == "tool_start" {
                    Some("internal")
                } else {
                    None
                }
            );
        }
        assert!(
            omp_record_view(
                &json!({"type":"custom_message","customType":"irc:incoming","display":false})
            )
            .is_none()
        );
        assert!(omp_record_view(&json!({"type":"custom_message","customType":"other"})).is_none());
    }

    #[test]
    fn session_records_have_typed_views_without_credential_hashes() {
        for (record, expected, visibility) in [
            (
                json!({"type":"reset_boundary","id":"reset","parentId":"previous"}),
                json!({"type":"reset_boundary"}),
                None,
            ),
            (
                json!({"type":"credential_pin","provider":"synthetic","hash":"synthetic-hash"}),
                json!({"type":"credential_pin","provider":"synthetic"}),
                Some("internal"),
            ),
            (
                json!({"type":"title","title":"Synthetic title","source":"user","pad":"padding","v":1}),
                json!({"type":"title","title":"Synthetic title","source":"user"}),
                None,
            ),
            (
                json!({"type":"title","title":"Untitled"}),
                json!({"type":"title","title":"Untitled"}),
                None,
            ),
        ] {
            let (kind, view, actual_visibility) = omp_record_view(&record).unwrap();
            assert_eq!(kind, "status");
            assert_eq!(view, expected);
            assert_eq!(actual_visibility, visibility);
        }
    }

    #[test]
    fn assistant_metadata_renames_native_usage_and_timing() {
        let metadata = assistant_metadata(&json!({"role":"assistant","model":"model","provider":"provider","usage":{"input":2,"output":3,"cacheRead":4,"cacheWrite":5,"totalTokens":14,"cost":{"total":0.25}},"contextSnapshot":{"promptTokens":20},"stopReason":"stop","ttft":10,"duration":30})).unwrap();
        assert_eq!(
            metadata,
            json!({"model":"model","provider":"provider","usage":{"input":2,"output":3,"cache_read":4,"cache_write":5,"total":14,"cost_usd":0.25},"context_tokens":20,"stop_reason":"stop","ttft_ms":10,"duration_ms":30})
        );
        assert!(assistant_metadata(&json!({"role":"user","model":"model"})).is_none());
        assert!(
            assistant_metadata(&json!({"role":"assistant","usage":[],"contextSnapshot":null}))
                .is_none()
        );
    }

    #[test]
    fn parity_invocations_keep_content_options_and_parent_contract() {
        let write = tool_call_view("write", &json!({"path":"demo","content":"first\nlast\n"}));
        assert_eq!(write["content"], "first\nlast\n");
        assert_eq!(write["line_count"], 2);
        let task = tool_call_view("task", &json!({"context":"Context\nContract","tasks":[{"name":"NamedChild","agent":"scout","task":"First\nSecond"}]}));
        assert_eq!(task["context"], "Context\nContract");
        assert_eq!(task["tasks"][0]["task"], "First\nSecond");
        let ask = tool_call_view("ask", &json!({"questions":[{"id":"q","question":"Choose?","options":[{"label":"A","description":"All details"},{"label":"B","description":"Compact"}]}]}));
        assert_eq!(ask["questions"][0]["options"][1]["description"], "Compact");
        let hub = tool_call_view("hub", &json!({"op":"send","to":"NamedChild","message":"First\nSecond"}));
        assert_eq!(hub["message"], "First\nSecond");
        let eval = tool_call_view("eval", &json!({"language":"py","code":"1 + 2","timeout":0,"reset":false}));
        assert_eq!(eval["code"], "1 + 2");
        assert_eq!(eval["timeout_s"], 0);
        assert_eq!(eval["reset"], false);
        let search = tool_call_view("grep", &json!({"pattern":"needle","case":false,"gitignore":true,"skip":2}));
        assert_eq!(search["case"], false);
        assert_eq!(search["gitignore"], true);
        assert_eq!(search["skip"], 2);
        let glob = tool_call_view("glob", &json!({"path":"*.rs","hidden":false,"gitignore":false,"limit":10}));
        assert_eq!(glob["hidden"], false);
        assert_eq!(glob["limit"], 10);
    }

    #[test]
    fn native_search_counts_and_named_full_assignments_are_not_inferred() {
        let grep = tool_output_view("grep","g",false,Some(&json!({"matchCount":7,"fileCount":3,"truncated":true,"fileLimitReached":3,"perFileLimitReached":2})));
        assert_eq!(grep["type"], "search");
        assert_eq!(grep["match_count"], 7);
        assert_eq!(grep["file_count"], 3);
        assert_eq!(grep["truncated"], true);
        assert_eq!(grep["file_limit_reached"], 3);
        assert_eq!(grep["per_file_limit_reached"], 2);
        assert!(tool_output_view("glob","g",false,None).get("file_count").is_none());
        let task = tool_output_view("task","t",false,Some(&json!({"progress":[{"id":"NamedChild","agent":"scout","status":"running","task":"First","assignment":"First\nFull assignment"}]})));
        assert_eq!(task["agents"][0]["name"], "NamedChild");
        assert_eq!(task["agents"][0]["agent"], "scout");
        assert_eq!(task["agents"][0]["task"], "First\nFull assignment");
        let (_, compact, _) = omp_record_view(&json!({"type":"compaction","summary":"First\nFull summary"})).unwrap();
        assert_eq!(compact["summary"], "First\nFull summary");
    }
}
