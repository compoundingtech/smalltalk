//! Effect 4 rich schemas derived from the inline client-v0 semantic contract.
//! Only Struct declarations are emitted; JSON Schema remains the wire authority.

use anyhow::{Context as _, Result, bail};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

const KEYWORDS: &[&str] = &[
    "$ref",
    "$comment",
    "description",
    "title",
    "examples",
    "type",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "const",
    "oneOf",
    "anyOf",
    "allOf",
    "if",
    "then",
    "not",
    "minimum",
    "maximum",
    "minLength",
    "maxLength",
    "minItems",
    "pattern",
    "format",
    "contains",
    "propertyNames",
    "maxItems",
    "uniqueItems",
    "x-st-ref",
    "x-st-brand",
    "x-st-codec",
];
const DATE_TIME: &str =
    r"^\d{4}-\d{2}-\d{2}[Tt]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:[Zz]|[+-]\d{2}:\d{2})$";
const ID_PATTERN: &str = r"^[a-z][a-z0-9-]*/[^\s]+$";

type Defs = Map<String, Value>;

fn js(value: &Value) -> String {
    value.to_string()
}

// ------------------------------------------------------------------ normalization

fn resolve<'a>(value: &'a Value, defs: &'a Defs) -> Result<&'a Value> {
    match value.get("$ref").and_then(Value::as_str) {
        Some(reference) => {
            let name = reference
                .strip_prefix("#/$defs/")
                .context("only #/$defs/ refs")?;
            defs.get(name)
                .with_context(|| format!("missing definition `{name}`"))
        }
        None => Ok(value),
    }
}

fn merge_objects(parts: &[Value]) -> Result<Value> {
    let mut properties = Map::new();
    let mut required = BTreeSet::new();
    let mut out = Map::new();
    let mut closed: Vec<BTreeSet<String>> = vec![];
    for part in parts {
        let object = part
            .as_object()
            .context("allOf part must be an object schema")?;
        if object.get("additionalProperties") == Some(&Value::Bool(false)) {
            closed.push(
                object
                    .get("properties")
                    .and_then(Value::as_object)
                    .map(|p| p.keys().cloned().collect())
                    .unwrap_or_default(),
            );
        }
        for (key, value) in object {
            match key.as_str() {
                "properties" => {
                    for (name, schema) in value.as_object().context("properties")? {
                        properties.insert(name.clone(), schema.clone());
                    }
                }
                "required" => {
                    for name in value.as_array().context("required")? {
                        required.insert(name.as_str().context("required name")?.to_owned());
                    }
                }
                _ => {
                    out.insert(key.clone(), value.clone());
                }
            }
        }
    }
    for allowed in &closed {
        if let Some(extra) = properties.keys().find(|name| !allowed.contains(*name)) {
            bail!("allOf adds `{extra}` to a closed part; not translated");
        }
    }
    if !properties.is_empty() {
        out.insert("properties".into(), Value::Object(properties));
    }
    if !required.is_empty() {
        out.insert(
            "required".into(),
            json!(required.into_iter().collect::<Vec<_>>()),
        );
    }
    Ok(Value::Object(out))
}

/// Known values of an open enum: exactly `anyOf: [{ enum }, { type: "string" }]` (+ docs).
fn open_enum(value: &Value) -> Option<Vec<Value>> {
    let object = value.as_object()?;
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), "anyOf" | "description" | "title"))
    {
        return None;
    }
    let branches = object.get("anyOf")?.as_array()?;
    if branches.len() != 2 {
        return None;
    }
    let known = branches
        .iter()
        .find(|branch| branch.get("enum").is_some())?;
    let string = branches
        .iter()
        .find(|branch| **branch == json!({ "type": "string" }))?;
    if known == string || known.as_object()?.len() != 1 {
        return None;
    }
    known.get("enum")?.as_array().cloned()
}

fn literal_values(test: &Value) -> Option<Vec<Value>> {
    test.get("const")
        .map(|value| vec![value.clone()])
        .or_else(|| test.get("enum").and_then(Value::as_array).cloned())
}

/// Inline `allOf`, distribute an `allOf` base over a sibling `oneOf`, and expand `if`/`then` on one
/// enum property into a `oneOf` of complete branches. An open discriminator adds one fallback
/// branch whose tag is `{ type: string, not: { enum: known } }`.
fn normalize(value: &Value, defs: &Defs) -> Result<Value> {
    let Some(object) = value.as_object() else {
        return Ok(value.clone());
    };
    let mut node = Map::new();
    for (key, child) in object {
        let child = match key.as_str() {
            "properties" => Value::Object(
                child
                    .as_object()
                    .context("properties")?
                    .iter()
                    .map(|(name, schema)| Ok((name.clone(), normalize(schema, defs)?)))
                    .collect::<Result<Map<_, _>>>()?,
            ),
            "items" | "additionalProperties" | "contains" | "then" => normalize(child, defs)?,
            "oneOf" | "anyOf" | "allOf" => Value::Array(
                child
                    .as_array()
                    .context("combinator")?
                    .iter()
                    .map(|schema| normalize(schema, defs))
                    .collect::<Result<_>>()?,
            ),
            _ => child.clone(),
        };
        node.insert(key.clone(), child);
    }
    let Some(all_of) = node.remove("allOf") else {
        return Ok(Value::Object(node));
    };
    let parts = all_of.as_array().context("allOf")?;
    let (conditionals, plain): (Vec<&Value>, Vec<&Value>) =
        parts.iter().partition(|part| part.get("if").is_some());
    let mut fragments = vec![];
    for part in plain {
        fragments.push(normalize(resolve(part, defs)?, defs)?);
    }
    let one_of = node.remove("oneOf");
    let docs: Map<String, Value> = node
        .iter()
        .filter(|(key, _)| matches!(key.as_str(), "description" | "title" | "examples"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    fragments.push(Value::Object(node));
    let mut base = merge_objects(&fragments)?;
    if let Some(one_of) = one_of {
        if !conditionals.is_empty() {
            bail!("allOf conditionals next to oneOf are not translated");
        }
        let branches = one_of
            .as_array()
            .context("oneOf")?
            .iter()
            .map(|branch| merge_objects(&[base.clone(), branch.clone()]))
            .collect::<Result<Vec<_>>>()?;
        return Ok(json!({ "oneOf": branches }));
    }
    if conditionals.is_empty() {
        return Ok(base);
    }
    let mut discriminators = BTreeSet::new();
    for part in &conditionals {
        discriminators.extend(
            part["if"]["properties"]
                .as_object()
                .context("if.properties")?
                .keys()
                .cloned(),
        );
    }
    if discriminators.len() != 1 {
        bail!("conditionals must all test one property");
    }
    let key = discriminators.into_iter().next().unwrap();
    if !base["required"]
        .as_array()
        .is_some_and(|names| names.contains(&json!(key)))
    {
        bail!("conditional discriminator `{key}` must be required");
    }
    let tag = resolve(&base["properties"][&key], defs)?.clone();
    let (domain, open) = match open_enum(&tag) {
        Some(known) => (known, true),
        None => (
            literal_values(&tag)
                .with_context(|| format!("conditional discriminator `{key}` is not an enum"))?,
            false,
        ),
    };
    let mut branches = domain
        .iter()
        .map(|value| {
            let mut fragments = vec![
                base.clone(),
                json!({ "properties": { key.clone(): { "const": value } } }),
            ];
            for part in &conditionals {
                if literal_values(&part["if"]["properties"][&key])
                    .is_some_and(|values| values.contains(value))
                {
                    fragments.push(part["then"].clone());
                }
            }
            merge_objects(&fragments)
        })
        .collect::<Result<Vec<_>>>()?;
    if open {
        base["properties"][&key] = json!({ "type": "string", "not": { "enum": domain } });
        branches.push(base);
    }
    let mut out = Map::new();
    out.insert("oneOf".into(), Value::Array(branches));
    out.extend(docs);
    Ok(Value::Object(out))
}

fn refs(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                out.insert(reference.trim_start_matches("#/$defs/").to_owned());
            }
            object.values().for_each(|child| refs(child, out));
        }
        Value::Array(items) => items.iter().for_each(|child| refs(child, out)),
        _ => {}
    }
}

// ------------------------------------------------------------------ emission

struct Cx<'a> {
    defs: &'a Defs,
    normalized: &'a BTreeMap<String, Value>,
    recursive: &'a BTreeSet<String>,
}

fn with_checks(base: String, checks: Vec<String>) -> String {
    checks
        .into_iter()
        .fold(base, |schema, check| format!("{schema}.check({check})"))
}

fn number_checks(value: &Value) -> Vec<String> {
    let mut checks = vec![];
    if let Some(minimum) = value.get("minimum") {
        checks.push(format!("Schema.isGreaterThanOrEqualTo({minimum})"));
    }
    if let Some(maximum) = value.get("maximum") {
        checks.push(format!("Schema.isLessThanOrEqualTo({maximum})"));
    }
    checks
}

/// The non-null member of a nullable schema, if `value` is one.
fn nullable_inner(value: &Value) -> Option<Value> {
    let object = value.as_object()?;
    let docs = |mut inner: Map<String, Value>| {
        for key in ["description", "title"] {
            if let Some(doc) = object.get(key) {
                inner.entry(key).or_insert(doc.clone());
            }
        }
        Value::Object(inner)
    };
    for combinator in ["oneOf", "anyOf"] {
        if let Some([a, b]) = object
            .get(combinator)
            .and_then(Value::as_array)
            .map(Vec::as_slice)
        {
            let null = json!({ "type": "null" });
            let other = if b == &null {
                a
            } else if a == &null {
                b
            } else {
                continue;
            };
            return Some(docs(other.as_object()?.clone()));
        }
    }
    if let Some([a, b]) = object
        .get("type")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
    {
        let other = if b == "null" {
            a
        } else if a == "null" {
            b
        } else {
            return None;
        };
        let mut inner = object.clone();
        inner.insert("type".into(), other.clone());
        return Some(Value::Object(inner));
    }
    let values = object.get("enum")?.as_array()?;
    if values.iter().any(Value::is_null) {
        let mut inner = object.clone();
        inner.insert(
            "enum".into(),
            Value::Array(values.iter().filter(|v| !v.is_null()).cloned().collect()),
        );
        return Some(Value::Object(inner));
    }
    None
}

fn subject_ref(object: &Map<String, Value>, families: &Value) -> Result<String> {
    let list: Vec<&str> = match families {
        Value::String(one) => vec![one.as_str()],
        Value::Array(many) => many
            .iter()
            .map(|f| f.as_str().context("x-st-ref family"))
            .collect::<Result<_>>()?,
        other => bail!("x-st-ref must be a family or a list, got {other}"),
    };
    let pattern = object
        .get("pattern")
        .and_then(Value::as_str)
        .context("x-st-ref needs a native `pattern`")?;
    let regex = format!("new RegExp({}, \"u\")", js(&json!(pattern)));
    if list == ["*"] {
        if pattern != ID_PATTERN {
            bail!("x-st-ref `*` needs the Id pattern, got `{pattern}`");
        }
        return Ok(format!("subjectRef<string>({regex})"));
    }
    let expected = format!("^(?:{})/[^\\s]+$", list.join("|"));
    if pattern != expected {
        bail!("x-st-ref {list:?} disagrees with pattern `{pattern}` (expected `{expected}`)");
    }
    Ok(format!(
        "subjectRef({regex}, {})",
        list.iter()
            .map(|family| js(&json!(family)))
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

fn emit_string(object: &Map<String, Value>) -> Result<String> {
    let mut excluded = None;
    if let Some(not) = object.get("not") {
        let only = not.as_object().filter(|n| n.len() == 1);
        if let Some(known) = only.and_then(|n| n.get("enum")).and_then(Value::as_array) {
            return Ok(format!("unknownCase({})", js(&Value::Array(known.clone()))));
        }
        // `not: { const }` excludes one value from an otherwise plain string.
        excluded = Some(
            only.and_then(|n| n.get("const"))
                .context("only `not: { enum }` and `not: { const }` are translated")?,
        );
    }
    let mut schema = match object.get("x-st-ref") {
        Some(families) => subject_ref(object, families)?,
        None => {
            let mut checks = vec![];
            if let Some(pattern) = object.get("pattern") {
                checks.push(format!(
                    "Schema.isPattern(new RegExp({}, \"u\"))",
                    js(pattern)
                ));
            }
            if let Some(min) = object.get("minLength") {
                checks.push(format!("Schema.isMinLength({min})"));
            }
            if let Some(max) = object.get("maxLength") {
                checks.push(format!("Schema.isMaxLength({max})"));
            }
            match object.get("format").and_then(Value::as_str) {
                Some("date-time") => checks.push(
                    "Schema.isPattern(DATE_TIME, { expected: \"an RFC 3339 date-time\" })".into(),
                ),
                Some("uri-reference") | None => {}
                Some(other) => bail!("unsupported string format `{other}`"),
            }
            with_checks("Schema.String".into(), checks)
        }
    };
    if let Some(value) = excluded {
        schema = format!(
            "{schema}.check(Schema.makeFilter((value: string) => value !== {}, {{ expected: {} }}))",
            js(value),
            js(&json!(format!("a string other than {value}")))
        );
    }
    if let Some(brand) = object.get("x-st-brand") {
        schema = format!(
            "{schema}.pipe(Schema.brand({}))",
            js(&json!(format!(
                "st3/{}",
                brand.as_str().context("x-st-brand")?
            )))
        );
    }
    Ok(
        match object
            .get("x-st-codec")
            .map(|c| c.as_str().context("x-st-codec"))
            .transpose()?
        {
            None => schema,
            Some("timestamp") if object.get("format") == Some(&json!("date-time")) => format!(
                "{schema}.check(isInstant).pipe(Schema.decodeTo(Schema.DateTimeUtc.check(rfc3339Range), utcFromIso))"
            ),
            Some("redacted") => format!("Schema.RedactedFromValue({schema})"),
            Some(other) => bail!("string codec `{other}` not translated here"),
        },
    )
}

fn emit_integer(object: &Map<String, Value>, value: &Value) -> Result<String> {
    let schema = with_checks("Schema.Int".into(), number_checks(value));
    Ok(
        match object
            .get("x-st-codec")
            .map(|c| c.as_str().context("x-st-codec"))
            .transpose()?
        {
            None => schema,
            Some("duration-ms") => format!(
                "{schema}.pipe(Schema.decodeTo(Schema.Duration.check(durationMillisRange), wholeUnits(1)))"
            ),
            Some("duration-s") => format!(
                "{schema}.pipe(Schema.decodeTo(Schema.Duration.check(durationSecondsRange), wholeUnits(1000)))"
            ),
            Some("epoch-ms") => format!(
                "{schema}.pipe(Schema.decodeTo(Schema.DateTimeUtcFromMillis.check(epochRange)))"
            ),
            Some(other) => bail!("integer codec `{other}` not translated"),
        },
    )
}

fn validate_keywords(object: &Map<String, Value>) -> Result<()> {
    for key in object.keys() {
        if !KEYWORDS.contains(&key.as_str()) {
            bail!("unsupported JSON Schema keyword `{key}`");
        }
        if matches!(key.as_str(), "allOf" | "if" | "then") {
            bail!("untranslated JSON Schema keyword `{key}`");
        }
    }
    Ok(())
}

/// A schema in value position (array item, record value, union member).
fn emit(value: &Value, cx: &mut Cx) -> Result<String> {
    let Some(object) = value.as_object() else {
        return match value {
            Value::Bool(true) => Ok("Schema.Unknown".into()),
            other => bail!("unsupported schema {other}"),
        };
    };
    validate_keywords(object)?;
    if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
        let name = reference
            .strip_prefix("#/$defs/")
            .context("only #/$defs/ refs")?;
        if !cx.defs.contains_key(name) {
            bail!("missing definition `{name}`");
        }
        if let Some(extra) = object
            .keys()
            .find(|key| !matches!(key.as_str(), "$ref" | "description" | "title"))
        {
            bail!("`$ref` with sibling `{extra}` is not translated");
        }
        return Ok(if cx.recursive.contains(name) {
            format!("Schema.suspend(() => {name})")
        } else {
            name.to_owned()
        });
    }
    if let Some(inner) = nullable_inner(value) {
        return Ok(format!("Schema.NullOr({})", emit(&inner, cx)?));
    }
    for key in ["x-st-ref", "x-st-brand", "not"] {
        if object.contains_key(key) && object.get("type").and_then(Value::as_str) != Some("string")
        {
            bail!("`{key}` is only translated on strings");
        }
    }
    if let Some(codec) = object.get("x-st-codec") {
        let codec = codec.as_str().context("x-st-codec must be a string")?;
        let kind = object.get("type").and_then(Value::as_str);
        if !matches!(
            (kind, codec),
            (Some("string"), "timestamp" | "redacted")
                | (Some("integer"), "duration-ms" | "duration-s" | "epoch-ms")
        ) {
            bail!("codec `{codec}` is not supported for type {kind:?}");
        }
        if object.contains_key("const") || object.contains_key("enum") || object.contains_key("not")
        {
            bail!("codec `{codec}` on a literal or excluded enum is not translated");
        }
    }
    if let Some(known) = open_enum(value) {
        if known.is_empty() || known.iter().any(|value| !value.is_string()) {
            bail!("open enum must contain known string cases");
        }
        return Ok(format!("openEnum({})", js(&Value::Array(known))));
    }
    if let Some(constant) = object.get("const") {
        return Ok(format!("Schema.Literal({})", js(constant)));
    }
    if let Some(values) = object.get("enum").and_then(Value::as_array) {
        return Ok(format!(
            "Schema.Literals({})",
            js(&Value::Array(values.clone()))
        ));
    }
    for combinator in ["oneOf", "anyOf"] {
        let Some(branches) = object.get(combinator).and_then(Value::as_array) else {
            continue;
        };
        if branches.iter().all(|branch| {
            branch
                .as_object()
                .is_some_and(|b| b.len() == 1 && b.contains_key("required"))
        }) {
            break;
        }
        let members = branches
            .iter()
            .map(|branch| emit(branch, cx))
            .collect::<Result<Vec<_>>>()?;
        let mode = if combinator == "oneOf" {
            ", { mode: \"oneOf\" }"
        } else {
            ""
        };
        return Ok(format!("Schema.Union([{}]{mode})", members.join(", ")));
    }
    if let Some(types) = object.get("type").and_then(Value::as_array) {
        let members = types
            .iter()
            .map(|kind| {
                let mut single = object.clone();
                single.insert("type".into(), kind.clone());
                emit(&Value::Object(single), cx)
            })
            .collect::<Result<Vec<_>>>()?;
        return Ok(format!("Schema.Union([{}])", members.join(", ")));
    }
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| object.contains_key("properties").then_some("object"));
    Ok(match kind {
        Some("string") => emit_string(object)?,
        Some("integer") => emit_integer(object, value)?,
        Some("number") => with_checks("Schema.Number".into(), number_checks(value)),
        Some("boolean") => "Schema.Boolean".into(),
        Some("null") => "Schema.Null".into(),
        Some("array") => {
            let items = object
                .get("items")
                .map(|items| emit(items, cx))
                .transpose()?
                .unwrap_or("Schema.Unknown".into());
            let mut checks = vec![];
            if let Some(min) = object.get("minItems") {
                checks.push(format!("Schema.isMinLength({min})"));
            }
            if let Some(max) = object.get("maxItems") {
                checks.push(format!("Schema.isMaxLength({max})"));
            }
            if object.get("uniqueItems") == Some(&Value::Bool(true)) {
                checks.push("Schema.isUnique()".into());
            }
            if let Some(contains) = object.get("contains") {
                checks.push(format!(
                    "Schema.makeFilter((items: ReadonlyArray<unknown>) => items.some(Schema.is({})), {{ expected: \"an array containing a match\" }})",
                    emit(contains, cx)?
                ));
            }
            with_checks(format!("Schema.Array({items})"), checks)
        }
        Some("object") => {
            let object = emit_object(object, cx, "")?;
            object.expression()
        }
        Some(other) => bail!("unsupported type `{other}`"),
        None => "Schema.Unknown".into(),
    })
}

fn doc_comment(indent: &str, value: &Value) -> String {
    match value.get("description").and_then(Value::as_str) {
        Some(text) => format!("{indent}/** {} */\n", text.replace("*/", "*\\/")),
        None => String::new(),
    }
}

fn annotations(value: &Value, identifier: Option<&str>) -> String {
    let mut parts = vec![];
    if let Some(identifier) = identifier {
        parts.push(format!("identifier: {}", js(&json!(identifier))));
    }
    for key in ["title", "description"] {
        if let Some(text) = value.get(key) {
            parts.push(format!("{key}: {}", js(text)));
        }
    }
    parts.join(", ")
}

/// One struct field: nullable -> `Option`, absent -> `optionalKey`, docs -> JSDoc + annotation.
fn emit_field(
    name: &str,
    value: &Value,
    required: bool,
    cx: &mut Cx,
    indent: &str,
) -> Result<String> {
    let (schema, nullable) = match nullable_inner(value) {
        Some(inner) => (emit(&inner, cx)?, true),
        None => (emit(value, cx)?, false),
    };
    let schema = match (nullable, required) {
        (true, true) => format!("Schema.OptionFromNullOr({schema})"),
        (true, false) => format!("Schema.OptionFromOptionalNullOr({schema}, NULL_NONE)"),
        (false, true) => schema,
        (false, false) => format!("optionalKey({schema})"),
    };
    let docs = annotations(value, None);
    let schema = if docs.is_empty() || value.get("$ref").is_some() {
        schema
    } else {
        format!("{schema}.annotate({{ {docs} }})")
    };
    Ok(format!(
        "{}{indent}{}: {schema}",
        doc_comment(indent, value),
        js(&json!(name))
    ))
}

struct Object {
    fields: Vec<String>,
    rest: Option<String>,
    checks: Vec<String>,
    multiline: bool,
}

impl Object {
    fn base(&self) -> String {
        let (open, sep, close) = if self.multiline {
            ("{\n", ",\n", "\n}")
        } else {
            ("{ ", ", ", " }")
        };
        let fields = || format!("{open}{}{close}", self.fields.join(sep));
        match (self.fields.is_empty(), &self.rest) {
            (true, Some(rest)) => format!("Schema.Record(Schema.String, {rest})"),
            (true, None) => "Schema.Record(Schema.String, Schema.Never)".into(),
            (false, None) => format!("Schema.Struct({})", fields()),
            (false, Some(rest)) => format!(
                "Schema.StructWithRest(Schema.Struct({}), [Schema.Record(Schema.String, {rest})])",
                fields()
            ),
        }
    }
    fn expression(self) -> String {
        let base = self.base();
        with_checks(base, self.checks)
    }
}

/// Open objects (no `additionalProperties`) with declared fields decode as plain structs: excess
/// keys are ignored, not preserved, so typos stay type errors. Bags without fields stay records.
fn emit_object(object: &Map<String, Value>, cx: &mut Cx, indent: &str) -> Result<Object> {
    validate_keywords(object)?;
    if object.contains_key("x-st-codec")
        || object.contains_key("x-st-ref")
        || object.contains_key("x-st-brand")
    {
        bail!("string/integer semantics are not supported on objects");
    }
    let empty = Map::new();
    let properties = object
        .get("properties")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let required: BTreeSet<&str> = object
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let multiline = !indent.is_empty();
    let mut fields = vec![];
    for (name, schema) in properties {
        fields.push(emit_field(
            name,
            schema,
            required.contains(name.as_str()),
            cx,
            if multiline { indent } else { "" },
        )?);
    }
    for name in &required {
        if !properties.contains_key(*name) {
            fields.push(format!(
                "{}{}: Schema.Unknown",
                if multiline { indent } else { "" },
                js(&json!(name))
            ));
        }
    }
    let rest = match object.get("additionalProperties") {
        Some(Value::Bool(false)) => None,
        Some(Value::Bool(true)) | None if !properties.is_empty() => None,
        Some(Value::Bool(true)) | None => Some("Schema.Unknown".to_owned()),
        Some(schema) if properties.is_empty() => Some(emit(schema, cx)?),
        Some(schema) => Some(emit(schema, cx)?),
    };
    let mut checks = vec![];
    if let Some(names) = object.get("propertyNames") {
        let mut key = names.as_object().context("propertyNames")?.clone();
        // Property names are always strings; references already name a string schema.
        if !key.contains_key("$ref") {
            key.entry("type").or_insert(json!("string"));
        }
        checks.push(format!("Schema.makeFilter((o: object) => Object.keys(o).every(Schema.is({})), {{ expected: \"property names matching the schema\" }})", emit(&Value::Object(key), cx)?));
    }
    if let Some(alternatives) = ["anyOf", "oneOf"]
        .iter()
        .filter_map(|key| object.get(*key).and_then(Value::as_array))
        .next()
    {
        let groups = alternatives
            .iter()
            .map(|alt| {
                let names = alt["required"]
                    .as_array()
                    .context("required-only alternative")?;
                Ok(names
                    .iter()
                    .map(|name| format!("{} in o", js(name)))
                    .collect::<Vec<_>>()
                    .join(" && "))
            })
            .collect::<Result<Vec<_>>>()?;
        let expected = alternatives
            .iter()
            .map(|alt| alt["required"].to_string())
            .collect::<Vec<_>>()
            .join(" or ");
        checks.push(format!(
            "Schema.makeFilter((o: object) => {}, {{ expected: {} }})",
            groups
                .iter()
                .map(|group| format!("({group})"))
                .collect::<Vec<_>>()
                .join(" || "),
            js(&json!(format!("fields {expected}")))
        ));
    }
    Ok(Object {
        fields,
        rest,
        checks,
        multiline,
    })
}

/// The single-const discriminator shared by every branch, if all values are unique.
fn tagged_by(branches: &[Value], cx: &Cx) -> Option<String> {
    let resolved: Vec<&Value> = branches
        .iter()
        .map(|branch| match branch.get("$ref").and_then(Value::as_str) {
            Some(reference) => cx.normalized.get(reference.trim_start_matches("#/$defs/")),
            None => Some(branch),
        })
        .collect::<Option<_>>()?;
    let first = resolved.first()?.get("properties")?.as_object()?;
    first
        .keys()
        .find(|key| {
            let values: Option<Vec<&Value>> = resolved
                .iter()
                .map(|branch| branch.get("properties")?.get(key.as_str())?.get("const"))
                .collect();
            values.is_some_and(|values| {
                values
                    .iter()
                    .map(|value| value.to_string())
                    .collect::<BTreeSet<_>>()
                    .len()
                    == values.len()
            })
        })
        .cloned()
}

/// Named object schemas preserve their field accessors and plain-data Type.
fn object_decl(name: &str, value: &Value, cx: &mut Cx) -> Result<String> {
    let object = value.as_object().context("object definition")?;
    let built = emit_object(object, cx, "  ")?;
    Ok(format!(
        "{}export const {name} = {}\nexport type {name} = typeof {name}.Type\nexport type {name}Encoded = typeof {name}.Encoded\n",
        doc_comment("", value),
        pure(&format!(
            "{}.annotate({{ {} }})",
            built.expression(),
            annotations(value, Some(name))
        ))
    ))
}

fn is_object(value: &Value) -> bool {
    value.get("properties").is_some()
        && value.get("oneOf").is_none()
        && value.get("anyOf").is_none_or(|alts| {
            alts.as_array().is_some_and(|alts| {
                alts.iter().all(|alt| {
                    alt.as_object()
                        .is_some_and(|a| a.len() == 1 && a.contains_key("required"))
                })
            })
        })
}

/// Recursive definitions need explicit decoded/wire types to break TypeScript's
/// initializer inference cycle. Non-recursive declarations still infer directly.
fn recursive_type(value: &Value, encoded: bool) -> Result<String> {
    if value == &Value::Bool(true) {
        return Ok("unknown".into());
    }
    let object = value.as_object().context("recursive schema")?;
    if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
        let name = reference
            .strip_prefix("#/$defs/")
            .context("only #/$defs/ refs")?;
        return Ok(format!("{name}{}", if encoded { "Encoded" } else { "" }));
    }
    if let Some(inner) = nullable_inner(value) {
        return Ok(format!("({}) | null", recursive_type(&inner, encoded)?));
    }
    if let Some(known) = open_enum(value) {
        let known = known.iter().map(js).collect::<Vec<_>>().join(" | ");
        return Ok(if encoded {
            "string".into()
        } else {
            format!("{known} | UnknownCase")
        });
    }
    if let Some(constant) = object.get("const") {
        return Ok(js(constant));
    }
    if let Some(values) = object.get("enum").and_then(Value::as_array) {
        return Ok(values.iter().map(js).collect::<Vec<_>>().join(" | "));
    }
    if !is_object(value) {
        for combinator in ["oneOf", "anyOf"] {
            if let Some(branches) = object.get(combinator).and_then(Value::as_array) {
                return Ok(branches
                    .iter()
                    .map(|branch| recursive_type(branch, encoded).map(|ty| format!("({ty})")))
                    .collect::<Result<Vec<_>>>()?
                    .join(" | "));
            }
        }
    }
    if let Some(kinds) = object.get("type").and_then(Value::as_array) {
        return Ok(kinds
            .iter()
            .map(|kind| {
                let mut single = object.clone();
                single.insert("type".into(), kind.clone());
                recursive_type(&Value::Object(single), encoded)
            })
            .collect::<Result<Vec<_>>>()?
            .join(" | "));
    }
    let codec = object.get("x-st-codec").and_then(Value::as_str);
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| object.contains_key("properties").then_some("object"));
    Ok(match kind {
        Some("string") if !encoded && codec == Some("timestamp") => "DateTime.Utc".into(),
        Some("string") if !encoded && codec == Some("redacted") => {
            "Redacted.Redacted<string>".into()
        }
        Some("string") if !encoded && object.contains_key("not") => "UnknownCase".into(),
        Some("string") => {
            let mut ty = "string".to_owned();
            if !encoded {
                if let Some(families) = object.get("x-st-ref") {
                    let families = match families {
                        Value::String(family) if family == "*" => "string".into(),
                        Value::String(_) => js(families),
                        Value::Array(families) => {
                            families.iter().map(js).collect::<Vec<_>>().join(" | ")
                        }
                        _ => bail!("invalid reference families"),
                    };
                    ty = format!("SubjectRef<{families}>");
                }
                if let Some(brand) = object.get("x-st-brand").and_then(Value::as_str) {
                    ty = format!("{ty} & Brand.Brand<{}>", js(&json!(format!("st3/{brand}"))));
                }
            }
            ty
        }
        Some("integer") if !encoded && codec == Some("epoch-ms") => "DateTime.Utc".into(),
        Some("integer") if !encoded && matches!(codec, Some("duration-ms" | "duration-s")) => {
            "Duration.Duration".into()
        }
        Some("integer" | "number") => "number".into(),
        Some("boolean") => "boolean".into(),
        Some("null") => "null".into(),
        Some("array") => format!(
            "ReadonlyArray<{}>",
            object
                .get("items")
                .map(|item| recursive_type(item, encoded))
                .transpose()?
                .unwrap_or("unknown".into())
        ),
        Some("object") => {
            let empty = Map::new();
            let properties = object
                .get("properties")
                .and_then(Value::as_object)
                .unwrap_or(&empty);
            let required = object.get("required").and_then(Value::as_array);
            let mut fields = vec![];
            for (name, field) in properties {
                let required = required.is_some_and(|names| names.contains(&json!(name)));
                let nullable = nullable_inner(field);
                let mut ty = if !encoded {
                    match &nullable {
                        Some(inner) => format!("Option.Option<{}>", recursive_type(inner, false)?),
                        None => recursive_type(field, false)?,
                    }
                } else {
                    recursive_type(field, true)?
                };
                if encoded && !required && nullable.is_none() {
                    ty = format!("({ty}) | null");
                }
                if encoded && !required && nullable.is_some() {
                    ty = format!("({ty}) | undefined");
                }
                let optional = !required && (encoded || nullable.is_none());
                fields.push(format!(
                    "readonly {}{}: {ty}",
                    js(&json!(name)),
                    if optional { "?" } else { "" }
                ));
            }
            if let Some(required) = required {
                for name in required {
                    let name = name.as_str().context("required property")?;
                    if !properties.contains_key(name) {
                        fields.push(format!("readonly {}: unknown", js(&json!(name))));
                    }
                }
            }
            let fields = format!("{{ {} }}", fields.join("; "));
            match object.get("additionalProperties") {
                Some(Value::Bool(false)) => fields,
                Some(Value::Bool(true)) | None if !properties.is_empty() => fields,
                Some(Value::Bool(true)) | None => "{ readonly [key: string]: unknown }".into(),
                Some(rest) => format!(
                    "{fields} & {{ readonly [key: string]: {} }}",
                    recursive_type(rest, encoded)?
                ),
            }
        }
        _ => "unknown".into(),
    })
}

fn def_decl(name: &str, value: &Value, cx: &mut Cx) -> Result<String> {
    if let Some(object) = value.as_object() {
        validate_keywords(object)?;
    }
    if cx.recursive.contains(name) {
        let expression = emit(value, cx)?;
        return Ok(format!(
            "{}export type {name} = {}\nexport type {name}Encoded = {}\nexport const {name}: Schema.Codec<{name}, {name}Encoded> = {}\n",
            doc_comment("", value),
            recursive_type(value, false)?,
            recursive_type(value, true)?,
            pure(&format!(
                "{expression}.annotate({{ {} }})",
                annotations(value, Some(name))
            ))
        ));
    }
    if is_object(value) {
        return object_decl(name, value, cx);
    }
    let branches = value
        .get("oneOf")
        .and_then(Value::as_array)
        .filter(|b| b.len() > 2 || b.iter().all(|x| x.get("type") != Some(&json!("null"))));
    if let Some(branches) = branches.filter(|_| open_enum(value).is_none()) {
        let members = branches
            .iter()
            .map(|branch| emit(branch, cx))
            .collect::<Result<Vec<_>>>()?;
        let tag = tagged_by(branches, cx);
        let mut union = format!(
            "Schema.Union([\n  {}\n], {{ mode: \"oneOf\" }})",
            members.join(",\n  ")
        );
        if let Some(tag) = tag {
            union = format!("{union}.pipe(Schema.toTaggedUnion({}))", js(&json!(tag)));
        }
        return Ok(format!(
            "{}export const {name} = {}\nexport type {name} = typeof {name}.Type\nexport type {name}Encoded = typeof {name}.Encoded\n",
            doc_comment("", value),
            pure(&format!(
                "{union}.annotate({{ {} }})",
                annotations(value, Some(name))
            ))
        ));
    }
    Ok(format!(
        "{}export const {name} = {}\nexport type {name} = typeof {name}.Type\nexport type {name}Encoded = typeof {name}.Encoded\n",
        doc_comment("", value),
        pure(&format!(
            "{}.annotate({{ {} }})",
            emit(value, cx)?,
            annotations(value, Some(name))
        ))
    ))
}

/// Mark a top-level initializer side-effect free so bundlers drop unused definitions.
fn pure(expression: &str) -> String {
    format!("/*#__PURE__*/ (() => {expression})()")
}

const PRELUDE: &str = r#"
import { Option } from "effect"
import type { Brand, Redacted } from "effect"

/** Optional nonnullable fields accept null as absence only in tolerant decoding. */
const optionalKey = <T extends Schema.Top>(schema: T) =>
  Schema.optionalKey(Schema.NullOr(schema).check(Schema.makeFilter(
    (value: T["Type"] | null, _ast, options) => value !== null || options.onExcessProperty !== "error",
    { expected: "a non-null optional value in strict mode" }
  ))).pipe(Schema.decodeTo(
    Schema.optionalKey(Schema.toType(schema)),
    SchemaTransformation.transformOptional<T["Type"], T["Type"] | null>({
      decode: (value) => Option.filter(value, (value): value is T["Type"] => value !== null),
      encode: (value) => value
    })
  ))

/** A subject reference `family/rest`; `SubjectRef<"mission">` is `` `mission/${string}` ``. */
export type SubjectRef<F extends string = string> = `${F}/${string}`
const subjectRef = <const F extends string>(pattern: RegExp, ...families: ReadonlyArray<F>) =>
  Schema.String.check(Schema.isPattern(pattern)).pipe(
    Schema.refine((s: string): s is SubjectRef<F> => families.length === 0 || families.some((family) => s.startsWith(`${family}/`)))
  )
/** RFC 3339 spells years 0000-9999 only; bounding the decoded side keeps every encode decodable. */
const rfc3339Range = /*#__PURE__*/ Schema.makeIsBetween({ order: DateTime.Order })({
  minimum: DateTime.makeUnsafe("0000-01-01T00:00:00.000Z"),
  maximum: DateTime.makeUnsafe("9999-12-31T23:59:59.999Z")
})
/** RFC 3339 <-> DateTime.Utc, with wire and decoded range validation. */
const DAYS_IN_MONTH = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31] as const
const isInstant = /*#__PURE__*/ Schema.makeFilter((s: string) => {
  // Date.parse normalizes February 30 and 24:00; neither is an RFC 3339 instant.
  const year = Number(s.slice(0, 4))
  const month = Number(s.slice(5, 7))
  const day = Number(s.slice(8, 10))
  const leap = year % 4 === 0 && (year % 100 !== 0 || year % 400 === 0)
  const days = month === 2 && leap ? 29 : DAYS_IN_MONTH[month - 1]
  return days !== undefined && day >= 1 && day <= days &&
    Number(s.slice(11, 13)) < 24 && !Number.isNaN(Date.parse(s))
}, { expected: "a valid RFC 3339 instant" })
const utcFromIso = /*#__PURE__*/ SchemaTransformation.transform({
  decode: (s: string) => DateTime.makeUnsafe(Date.parse(s)),
  encode: (d: DateTime.Utc) => DateTime.formatIso(d)
})
/** A case this SDK version does not know (open enums, open union tags), kept verbatim in `raw`. */
export interface UnknownCase {
  readonly _tag: "Unknown"
  readonly raw: string
}
// Identity is registered after Struct parsing, never inferred from user JSON shape.
const unknownCases = new WeakSet<object>()
const unknownCase = (known: ReadonlyArray<string>) => {
  const isUnknown = (raw: string) => !known.includes(raw)
  return Schema.String.check(Schema.makeFilter(isUnknown, { expected: "a value outside the known cases" })).pipe(
    Schema.decodeTo(
      Schema.Struct({ _tag: Schema.Literal("Unknown"), raw: Schema.String })
        .check(Schema.makeFilter((u: UnknownCase) => {
          if (!isUnknown(u.raw)) return false
          unknownCases.add(u)
          return true
        }, { expected: "an unknown case" })),
      SchemaTransformation.transform({ decode: (raw: string): UnknownCase => ({ _tag: "Unknown", raw }), encode: (u: UnknownCase) => u.raw })
    )
  )
}
const openEnum = <const L extends ReadonlyArray<string>>(known: L) => Schema.Union([Schema.Literals(known), unknownCase(known)])
/**
 * Strict decoding rejects excess struct keys and unknown cases; tolerant decoding
 * retains unknown enum values and ignores excess struct keys.
 */
const isObject = (value: unknown): value is { [key: string]: unknown } =>
  typeof value === "object" && value !== null
export const containsUnknownCase = (value: unknown): boolean => {
  const seen = new WeakSet<object>()
  const pending: Array<unknown> = [value]
  while (pending.length > 0) {
    const current = pending.pop()
    if (!isObject(current)) continue
    if (unknownCases.has(current)) return true
    if (seen.has(current)) continue
    seen.add(current)
    for (const key in current) {
      if (Object.hasOwn(current, key)) pending.push(current[key])
    }
  }
  return false
}
export type DecodeMode = "tolerant" | "strict"
/** Decode untrusted wire data synchronously; production defaults to tolerant mode. */
export const decodeUnknownSync = <TType, TEncoded>(schema: Schema.Codec<TType, TEncoded>, mode: DecodeMode = "tolerant") =>
  Schema.decodeUnknownSync(
    mode === "strict" ? schema.check(Schema.makeFilter((value: TType) => !containsUnknownCase(value), { expected: "only known enum cases" })) : schema,
    { onExcessProperty: mode === "strict" ? "error" : "ignore" }
  )
/** Effectful equivalent, retaining the schema's decoding service requirements. */
export const decodeUnknownEffect = <TType, TEncoded, TDecodingServices, TEncodingServices>(
  schema: Schema.Codec<TType, TEncoded, TDecodingServices, TEncodingServices>,
  mode: DecodeMode = "tolerant"
) =>
  Schema.decodeUnknownEffect(
    mode === "strict" ? schema.check(Schema.makeFilter((value: TType) => !containsUnknownCase(value), { expected: "only known enum cases" })) : schema,
    { onExcessProperty: mode === "strict" ? "error" : "ignore" }
  )
/** Integer durations on the wire: whole `unitMillis` units, rounded on encode, non-negative and safe. */
const wholeUnits = (unitMillis: number) => {
  const unitNanos = BigInt(unitMillis) * 1_000_000n
  return SchemaTransformation.transform({
    // Seconds can overflow precise millisecond numbers even when wire units are safe.
    decode: (units: number) => unitMillis === 1 ? Duration.millis(units) : Duration.nanos(BigInt(units) * unitNanos),
    encode: (duration: Duration.Duration) => Duration.match(duration, {
      onMillis: (millis) => Math.round(millis / unitMillis),
      onNanos: (nanos) => Number((nanos + unitNanos / 2n) / unitNanos),
      onInfinity: () => Infinity,
      onNegativeInfinity: () => -Infinity
    })
  })
}
const durationMillisRange = /*#__PURE__*/ Schema.makeIsBetween({ order: Duration.Order })({
  minimum: Duration.zero,
  maximum: Duration.millis(Number.MAX_SAFE_INTEGER)
})
const durationSecondsRange = /*#__PURE__*/ Schema.makeIsBetween({ order: Duration.Order })({
  minimum: Duration.zero,
  maximum: Duration.nanos(BigInt(Number.MAX_SAFE_INTEGER) * 1_000_000_000n)
})
/** Epoch-millisecond instants are non-negative integers on the wire. */
const epochRange = /*#__PURE__*/ Schema.makeIsBetween({ order: DateTime.Order })({
  minimum: DateTime.makeUnsafe(0),
  maximum: DateTime.makeUnsafe("9999-12-31T23:59:59.999Z")
})
/** Nullable fields treat missing and `null` alike; `None` encodes as `null`. */
const NULL_NONE = { onNoneEncoding: null } as const
"#;

/// Every `$defs` entry as a rich Effect Schema, dependency-ordered.
pub fn models(schema: &Value) -> Result<String> {
    match schema.get("x-st-integers").and_then(Value::as_str) {
        Some("json-safe") => {}
        other => {
            bail!("integer transport policy `x-st-integers` must be `json-safe`, got {other:?}")
        }
    }
    let defs = schema["$defs"].as_object().context("schema definitions")?;
    let normalized: BTreeMap<String, Value> = defs
        .iter()
        .map(|(name, def)| {
            Ok((
                name.clone(),
                normalize(def, defs).with_context(|| format!("normalize `{name}`"))?,
            ))
        })
        .collect::<Result<_>>()?;
    let mut order = vec![];
    let mut state: BTreeMap<String, bool> = BTreeMap::new();
    let mut stack = vec![];
    let mut recursive = BTreeSet::new();
    fn visit(
        name: &str,
        graph: &BTreeMap<String, Value>,
        state: &mut BTreeMap<String, bool>,
        stack: &mut Vec<String>,
        recursive: &mut BTreeSet<String>,
        order: &mut Vec<String>,
    ) -> Result<()> {
        match state.get(name) {
            Some(true) => return Ok(()),
            Some(false) => {
                let start = stack
                    .iter()
                    .position(|entry| entry == name)
                    .context("dependency stack")?;
                recursive.extend(stack[start..].iter().cloned());
                return Ok(());
            }
            None => {}
        }
        state.insert(name.to_owned(), false);
        stack.push(name.to_owned());
        let mut deps = BTreeSet::new();
        let definition = graph
            .get(name)
            .with_context(|| format!("missing definition `{name}`"))?;
        refs(definition, &mut deps);
        for dep in deps {
            visit(&dep, graph, state, stack, recursive, order)?;
        }
        stack.pop();
        state.insert(name.to_owned(), true);
        order.push(name.to_owned());
        Ok(())
    }
    for name in normalized.keys() {
        visit(
            name,
            &normalized,
            &mut state,
            &mut stack,
            &mut recursive,
            &mut order,
        )?;
    }
    let mut out = format!(
        "// @generated by st3-client-codegen; do not edit.\n// Sources: docs/st3/client-v0/schemas/client-v0.schema.json, operations.json\n// Generator: crates/st3-client-codegen/src/rich.rs\n// Regenerate: cargo run -p st3-client-codegen; verify: cargo run -p st3-client-codegen -- --check\nimport {{ DateTime, Duration, Schema, SchemaTransformation }} from \"effect\"\n\nexport const API_VERSION = \"st3.client.v0\" as const\nconst DATE_TIME = new RegExp({}, \"u\")\n{PRELUDE}\n",
        js(&json!(DATE_TIME))
    );
    let mut cx = Cx {
        defs,
        normalized: &normalized,
        recursive: &recursive,
    };
    for name in order {
        let decl = def_decl(&name, &normalized[&name], &mut cx)
            .with_context(|| format!("emit `{name}`"))?;
        writeln!(out, "{decl}")?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conditional_schema() -> Value {
        json!({
            "type": "object",
            "required": ["type", "id"],
            "properties": {
                "id": { "type": "string" },
                "type": { "$ref": "#/$defs/Kind" },
                "payload": { "type": "string" }
            },
            "allOf": [{
                "if": { "properties": { "type": { "const": "message" } } },
                "then": { "required": ["payload"] }
            }]
        })
    }

    #[test]
    fn optional_conditional_discriminators_fail_instead_of_changing_missing_semantics() {
        let mut input = conditional_schema();
        input["required"] = json!(["id"]);
        let schema = json!({
            "x-st-integers": "json-safe",
            "$defs": {
                "Kind": { "enum": ["message", "status"] },
                "Entry": input
            }
        });
        assert!(models(&schema).is_err());
    }

    #[test]
    fn unsupported_codecs_are_not_silently_erased() {
        for input in [
            json!({ "type": "string", "x-st-codec": "epoch-ms" }),
            json!({ "type": "number", "x-st-codec": "duration-s" }),
            json!({ "type": "integer", "x-st-codec": "future-codec" }),
            json!({ "type": "string", "x-st-codec": "timestamp" }),
            json!({ "type": "string", "enum": ["x"], "x-st-codec": "redacted" }),
            json!({ "type": "object", "properties": { "x": { "type": "string" } }, "x-st-codec": "future-codec" }),
        ] {
            let schema = json!({ "x-st-integers": "json-safe", "$defs": { "Value": input } });
            assert!(models(&schema).is_err(), "{schema}");
        }
    }

    #[test]
    fn missing_references_fail_without_panicking() {
        let schema = json!({
            "x-st-integers": "json-safe",
            "$defs": { "Value": { "$ref": "#/$defs/Missing" } }
        });
        assert!(models(&schema).is_err());
    }
}
