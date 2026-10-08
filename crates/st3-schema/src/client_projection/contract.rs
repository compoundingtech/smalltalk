//! Shared effective native discovery authority; both gateway and generator derive this contract.
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use super as policy;

pub struct Contract {
    pub artifact: Value,
    pub discovery: Value,
    pub definitions: Map<String, Value>,
    pub families: BTreeMap<String, String>,
    pub claims: BTreeMap<String, String>,
    pub custom_effective: Value,
}

fn reference(name: &str) -> Value { json!({"$ref": format!("#/$defs/{name}")}) }
fn object(properties: Value, required: &[&str]) -> Value {
    json!({"type":"object", "additionalProperties":false, "properties":properties, "required":required})
}
fn optional_nullable(schema: Value) -> Value {
    json!({"anyOf":[schema,{"type":"null"}], "x-st-preserve-null":true})
}
fn union(names: impl IntoIterator<Item = String>) -> Value {
    json!({"oneOf":names.into_iter().map(|name| reference(&name)).collect::<Vec<_>>()})
}
fn literal_schema(value: &Value) -> Value {
    match value {
        Value::Object(values) => {
            let mut required = values.keys().collect::<Vec<_>>();
            required.sort_unstable();
            json!({"type":"object", "additionalProperties":false,
                "required":required,
                "properties":values.iter().map(|(key,value)| (key.clone(),literal_schema(value))).collect::<Map<_,_>>() })
        }
        Value::Array(values) => json!({"type":"array", "minItems":values.len(), "maxItems":values.len(),
            "items": if values.is_empty() { json!({"type":"string"}) } else { json!({"anyOf":values.iter().map(literal_schema).collect::<Vec<_>>()}) },
            "const": value, "x-st-exact-json":value}),
        other => json!({"const":other}),
    }
}
fn exact_json_object(value: &Value) -> Value {
    json!({"type":"object", "additionalProperties":reference("NativeJsonValue"),
        "const":value, "x-st-exact-json":value})
}
// Share reference constraints so validators compile each canonical-reference
// pattern once, rather than once per field and per claim in the native union.
fn subject_reference(families: &[String], definitions: &mut Map<String, Value>) -> Value {
    let schema = crate::subject_reference_schema(families);
    let registered = schema["x-st-native-ref-families"].as_array().expect("reference families");
    let name = if registered.len() == crate::registry().subjects.len() {
        "NativeSubjectRef".to_owned()
    } else {
        format!("Native{}SubjectRef", registered.iter().map(|family| pascal(family.as_str().expect("reference family"))).collect::<String>())
    };
    definitions.entry(name.clone()).or_insert(schema);
    reference(&name)
}
fn field_schema(field: &Value, definitions: &mut Map<String, Value>) -> Result<Value, String> {
    let schema = if let Some(values) = field["values"].as_array().filter(|values| !values.is_empty()) {
        json!({"enum":values})
    } else {
        match field["value_type"].as_str().ok_or("native field value_type")? {
            "any" => reference("NativeJsonValue"),
            "array" => json!({"type":"array", "items":reference("NativeJsonValue")}),
            "object" => reference("NativeJsonObject"),
            "subject-reference" => subject_reference(&field["reference_families"].as_array()
                .map(|values| values.iter().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>()).unwrap_or_default(), definitions),
            "integer" => json!({"type":"integer", "minimum":-9007199254740991_i64,"maximum":9007199254740991_u64}),
            kind @ ("boolean" | "number" | "string") => json!({"type":kind}),
            other => return Err(format!("unsupported native value type {other}")),
        }
    };
    Ok(optional_nullable(schema))
}
fn projected_fields(claim: &Value, definitions: &mut Map<String, Value>) -> Result<Value, String> {
    let mut properties = Map::new();
    for (name, field) in claim["fields"].as_object().ok_or("effective native fields")? {
        let mut schema = field_schema(field, definitions)?;
        if name == "desired" && matches!(claim["special"].as_str(), Some("agent_desired" | "account_desired")) {
            schema = optional_nullable(reference("CanonicalNode"));
        } else if name == "body" && claim["special"] == "glass_body" {
            schema = optional_nullable(reference("GlassBody"));
        }
        properties.insert(name.clone(), schema);
    }
    Ok(json!({"type":"object", "additionalProperties":false, "properties":properties}))
}

impl Contract {
    pub fn derive(base: &Value) -> Result<Self, String> {
        let mut definitions = Map::new();
        definitions.insert("NativeJsonValue".into(), json!({"anyOf":[{"type":"null"},{"type":"boolean"},{"type":"number"},{"type":"string"},{"type":"array","items":reference("NativeJsonValue")},reference("NativeJsonObject")]}));
        definitions.insert("NativeJsonObject".into(), json!({"type":"object","additionalProperties":reference("NativeJsonValue")}));
        definitions.insert("SubjectLocalFence".into(), object(json!({"node":{"type":"string"},"position":{"type":"integer","minimum":0}}), &["node","position"]));
        definitions.insert("SubjectHistoryCoverage".into(), object(json!({
            "source":{"const":"answering-host-retained"},
            "replicated_through":{"type":"integer","minimum":0},
            "local_through":{"type":"integer","minimum":0},
            "earlier_history":{"const":"not-guaranteed"},
            "local_retention":object(json!({"window_ms":{"type":"integer","minimum":0},"max_per_subject_kind":{"type":"integer","minimum":0},"keeps_latest_per_subject_kind":{"const":true}}), &["window_ms","max_per_subject_kind","keeps_latest_per_subject_kind"]),
            "replicated_retention":object(json!({"mode":{"const":"checkpoint-prunable"},"checkpoint_enabled":{"type":"boolean"}}), &["mode","checkpoint_enabled"]),
            "projection_limits":object(json!({"max_heads":{"const":64},"max_claim_payload_bytes":{"const":65536},"max_response_bytes":{"const":1048576}}), &["max_heads","max_claim_payload_bytes","max_response_bytes"])
        }), &["source","replicated_through","local_through","earlier_history","local_retention","replicated_retention","projection_limits"]));
        definitions.insert("SubjectProvenance".into(), json!({"oneOf":[
            object(json!({"source":{"const":"replicated"},"claim_id":{"type":"string"},"origin":{"type":"string"},"actor":{"type":"string"},"accepted_at":reference("Timestamp"),"store_index":{"type":"integer","minimum":0}}), &["source","claim_id","origin","accepted_at","store_index"]),
            object(json!({"source":{"const":"local"},"observation_id":{"type":"string"},"node":{"type":"string"},"actor":{"type":"string"},"observed_at":reference("Timestamp"),"after_store_index":{"type":"integer","minimum":0},"position":{"type":"integer","minimum":1}}), &["source","observation_id","node","observed_at","after_store_index","position"])
        ]}));
        let mut families = BTreeMap::new();
        let mut claims = BTreeMap::new();
        let mut entries = Vec::new();
        let mut custom_effective = Value::Null;
        let mut descriptor_names = Vec::new();
        for family in crate::registry().subjects.keys() {
            let descriptor = policy::family_descriptor(family).ok_or_else(|| format!("missing effective descriptor for native family {family}"))?;
            let schema_id = policy::family_schema_id(family).ok_or("effective family schema ID")?;
            let family_name = format!("Native{}Subject", pascal(family));
            families.insert(family_name.clone(), schema_id.clone());
            let mut family_claims = Vec::new();
            let mut claim_ids = Map::new();
            for (kind, claim) in descriptor["claims"].as_object().ok_or("effective claims")? {
                let name = format!("Native{}{}Claim", pascal(family), pascal(kind));
                let mut fields = projected_fields(claim, &mut definitions)?;
                let claim_id = if kind == "custom.*" {
                    fields = json!({"type":"object", "additionalProperties":reference("NativeJsonValue")});
                    custom_effective = json!({"wire_version":policy::WIRE_VERSION,"family":family,"kind":"",
                        "identity":descriptor["identity"],"claim":claim,"value_semantics":descriptor["value_semantics"],"custom_payload":descriptor["custom_payload"]});
                    json!({"type":"string","pattern":"^subject-claim-schema/[a-f0-9]{64}/custom/custom\\.[a-zA-Z0-9_.-]+$"})
                } else {
                    let id = policy::claim_schema_id(family,kind).ok_or("effective claim schema ID")?;
                    claims.insert(name.clone(),id.clone());
                    claim_ids.insert(kind.clone(),json!(id));
                    json!({"const":id})
                };
                // Resource observation's facts are selected by the native ResourceSpec policy,
                // never an unrestricted object. Each branch binds the kind to its closed facts.
                let field_name = format!("{name}Fields");
                if let Some(resources) = claim["special_output"]["by_kind"].as_object() {
                    let mut alternatives = Vec::new();
                    for (resource_kind, resource) in resources {
                        let mut branch = fields.clone();
                        branch["properties"]["kind"] = json!({"const":resource_kind});
                        branch["required"] = json!(["kind"]);
                        branch["properties"]["facts"] = optional_nullable(projected_fields(resource, &mut definitions)?);
                        alternatives.push(branch);
                    }
                    fields["properties"]["facts"] = optional_nullable(object(json!({}), &[]));
                    alternatives.push(fields);
                    fields = json!({"anyOf":alternatives});
                }
                definitions.insert(field_name.clone(), fields);
                let properties = json!({"id":{"type":"string"},"ref":subject_reference(std::slice::from_ref(family), &mut definitions),
                    "kind":if kind == "custom.*" { json!({"type":"string","pattern":"^custom\\.[a-zA-Z0-9_-]+(?:\\.[a-zA-Z0-9_-]+)+$"}) } else { json!({"const":kind}) },
                    "schema_id":claim_id,"retention":if kind == "custom.*" { json!({"const":"durable"}) } else { json!({"const":claim["retention"]}) },
                    "provenance":reference("SubjectProvenance"),"payload_availability":{"enum":["available","withheld","unavailable"]},
                    "reason":{"type":"string"},"fields":reference(&field_name),"omitted_fields":{"type":"array","items":{"type":"string"}}});
                definitions.insert(name.clone(),object(properties,&["id","ref","kind","schema_id","retention","provenance","payload_availability","fields","omitted_fields"]));
                if kind == "custom.*" {
                    definitions[&name]["x-st-custom-claim-schema"] = json!(true);
                }
                family_claims.push(name);
            }
            let heads_name = format!("Native{}Claim",pascal(family));
            definitions.insert(heads_name.clone(),union(family_claims));
            let family_ref = subject_reference(std::slice::from_ref(family), &mut definitions);
            definitions.insert(family_name.clone(),object(json!({"kind":{"const":"subject"},"id":{"type":"string"},"ref":family_ref,
                "family":{"const":family},"schema_id":{"const":schema_id},"heads":{"type":"array","maxItems":64,"items":reference(&heads_name)},"heads_complete":{"type":"boolean"},"local_fence":reference("SubjectLocalFence")}),&["kind","id","ref","family","schema_id","heads","heads_complete","local_fence"]));
            let descriptor_name = format!("Native{}Descriptor", pascal(family));
            definitions.insert(descriptor_name.clone(),object(json!({"family":{"const":family},"schema_id":{"const":schema_id},"descriptor":exact_json_object(&descriptor),"claim_schema_ids":exact_json_object(&Value::Object(claim_ids.clone()))}),&["family","schema_id","descriptor","claim_schema_ids"]));
            descriptor_names.push(descriptor_name);
            entries.push(json!({"family":family,"schema_id":schema_id,"descriptor":descriptor,"claim_schema_ids":claim_ids}));
        }
        definitions.insert("SubjectProjection".into(),union(families.keys().cloned()));
        definitions.insert("SubjectClaim".into(),union(claims.keys().cloned().chain(std::iter::once("NativeCustomCustomClaim".to_owned()))));
        definitions.insert("SubjectFamilyDescriptor".into(),union(descriptor_names));
        for (name, collection, item) in [("SubjectsPage","subjects","SubjectProjection"),("SubjectClaimsPage","subject-claims","SubjectClaim"),("SubjectHistoryPage","subject-history","SubjectClaim")] {
            let mut page = base["$defs"]["Page"].clone();
            page["properties"]["collection"] = json!({"const":collection});
            page["properties"]["items"]["items"] = reference(item);
            page["properties"]["local_fence"] = reference("SubjectLocalFence");
            page["required"].as_array_mut().ok_or("Page.required")?.push(json!("local_fence"));
            if collection != "subjects" {
                page["properties"]["coverage"] = reference("SubjectHistoryCoverage");
                page["required"].as_array_mut().ok_or("Page.required")?.push(json!("coverage"));
            }
            definitions.insert(name.into(),page);
        }
        let mut subscription_branches = Vec::new();
        for selector in [json!({"family":{"enum":crate::registry().subjects.keys().collect::<Vec<_>>()},"ref_prefix":{"type":"string"}}),json!({"ref":subject_reference(&[], &mut definitions)})] {
            let required_selector = if selector.get("family").is_some() { "family" } else { "ref" };
            let mut properties = selector.as_object().ok_or("selector")?.clone();
            properties.extend(json!({"kind":{"const":"subscribe"},"id":{"type":"string","minLength":1,"maxLength":128},"collection":{"const":"subjects"},"limit":{"type":"integer","minimum":1,"maximum":200}}).as_object().ok_or("subscription properties")?.clone());
            subscription_branches.push(object(Value::Object(properties),&["kind","id","collection",required_selector]));
        }
        definitions.insert("SubjectSubscription".into(),json!({"oneOf":subscription_branches}));
        let mut subject_frames = Vec::new();
        for kind in ["snapshot","changes"] {
            let mut frame = base["$defs"]["CollectionFrame"]["oneOf"].as_array().ok_or("collection frames")?.iter().find(|frame| frame["properties"]["kind"]["const"] == kind).ok_or("operational frame")?.clone();
            frame["properties"]["collection"] = json!({"const":"subjects"});
            let items = if kind == "snapshot" { "items" } else { "upserts" };
            frame["properties"][items]["items"] = reference("SubjectProjection");
            subject_frames.push(frame);
        }
        definitions.insert("SubjectCollectionFrame".into(),json!({"oneOf":subject_frames}));
        // Hash only the effective projection and its transitive base dependencies.
        // Discovery definitions are added afterwards, so its digest never hashes itself.
        let mut projected_defs = definitions.clone();
        collect_dependencies(base, &mut projected_defs)?;
        let projected_schema_sha256 = crate::canonical_json_sha256(&Value::Object(projected_defs.clone()));
        let projected_json_schema = json!({"$schema":"https://json-schema.org/draft/2020-12/schema",
            "$id":"https://st3.local/schemas/subject-projection.schema.json", "$defs":projected_defs});
        let resources = policy::resource_facts_descriptor();
        let discovery = json!({"kind":"subject-schemas", "wire_version":policy::WIRE_VERSION,
            "native_registry_digest":crate::registry().digest(), "projected_schema_sha256":projected_schema_sha256,
            "projected_digest_convention":"canonical-effective-definitions-with-transitive-dependencies-excluding-discovery-v1",
            "families":entries, "resources":resources, "projected_json_schema":projected_json_schema});
        let mut discovery_schema = literal_schema(&discovery);
        discovery_schema["properties"]["projected_json_schema"] = exact_json_object(&projected_json_schema);
        discovery_schema["properties"]["resources"] = exact_json_object(&resources);
        for (branch, entry) in discovery_schema["properties"]["families"]["items"]["anyOf"].as_array_mut().ok_or("discovery families")?.iter_mut().zip(&entries) {
            branch["properties"]["descriptor"] = exact_json_object(&entry["descriptor"]);
            branch["properties"]["claim_schema_ids"] = exact_json_object(&entry["claim_schema_ids"]);
        }
        definitions.insert("SubjectSchemas".into(), discovery_schema);
        let mut artifact_defs = base["$defs"].as_object().ok_or("base definitions")?.clone();
        artifact_defs.extend(definitions.clone());
        let artifact = json!({"$schema":"https://json-schema.org/draft/2020-12/schema",
            "$id":"https://st3.local/schemas/subject-projection.schema.json",
            "title":"Registry-derived effective native client subject projection",
            "x-st-generated-by":"cargo run -p st3-client-codegen", "wire_version":policy::WIRE_VERSION,
            "native_registry_digest":crate::registry().digest(),
            "projected_schema_sha256":projected_schema_sha256,
            "projected_digest_convention":discovery["projected_digest_convention"],
            "families":entries,"resources":resources,"$defs":artifact_defs});
        Ok(Self { artifact, discovery, definitions, families, claims, custom_effective })
    }

    pub fn apply(&self, schema: &mut Value) -> Result<(), String> {
        let defs = schema["$defs"].as_object_mut().ok_or("client definitions")?;
        defs.extend(self.definitions.clone());
        for (owner, child) in [("CollectionCommand","SubjectSubscription"),("CollectionFrame","SubjectCollectionFrame")] {
            let branches = defs[owner]["oneOf"].as_array_mut().ok_or("collection union")?;
            let child = reference(child);
            if !branches.contains(&child) { branches.push(child); }
        }
        Ok(())
    }

}

fn pascal(value: &str) -> String {
    value.split(|ch: char| !ch.is_ascii_alphanumeric()).filter(|part| !part.is_empty()).map(|part| {
        let mut chars = part.chars();
        chars.next().map(|first| first.to_ascii_uppercase().to_string() + chars.as_str()).unwrap_or_default()
    }).collect()
}

fn collect_dependencies(base: &Value, definitions: &mut Map<String, Value>) -> Result<(), String> {
    fn refs(value: &Value, found: &mut BTreeSet<String>) {
        match value {
            Value::Object(values) => {
                if let Some(name) = values.get("$ref").and_then(Value::as_str).and_then(|r| r.strip_prefix("#/$defs/")) { found.insert(name.to_owned()); }
                for value in values.values() { refs(value, found); }
            }
            Value::Array(values) => for value in values { refs(value, found); },
            _ => {}
        }
    }
    loop {
        let mut found = BTreeSet::new();
        for definition in definitions.values() { refs(definition, &mut found); }
        let missing = found.into_iter().filter(|name| !definitions.contains_key(name)).collect::<Vec<_>>();
        if missing.is_empty() { return Ok(()); }
        for name in missing {
            let value = base["$defs"].get(&name).ok_or_else(|| format!("missing native dependency {name}"))?;
            definitions.insert(name, value.clone());
        }
    }
}
