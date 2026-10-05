//! Closed native models for Rust and Swift, from the same projected JSON Schemas as TypeScript.
use anyhow::{Context as _, Result, bail};
use serde_json::{Map,Value,json};
use std::collections::{BTreeMap,BTreeSet};
use std::fmt::Write as _;
use super::subjects::Contract;

fn nullable(schema:&Value)->Option<Value> {
    for key in ["anyOf","oneOf"] {
        if let Some(branches)=schema[key].as_array() {
            if branches.len()==2 && branches[1]==json!({"type":"null"}) { return Some(branches[0].clone()); }
        }
    }
    None
}
struct Models { definitions:Map<String,Value>, pending:BTreeSet<String>, emitted:BTreeSet<String>, rust:String, swift:String }
impl Models {
    fn type_name(&mut self,schema:&Value,name:&str,swift:bool)->Result<String> {
        if let Some(reference)=schema["$ref"].as_str() {
            let reference=reference.strip_prefix("#/$defs/").context("native model reference")?;
            return Ok(match reference { "Timestamp"|"Id"|"Cursor"|"Revision"=>"String".into(),other=>other.into() });
        }
        if let Some(inner)=nullable(schema) { return self.type_name(&inner,name,swift); }
        if let Some(literal)=schema.get("const").filter(|value| !value.is_array() && !value.is_object()) {
            return Ok(match literal { Value::String(_)=>"String".into(),Value::Bool(_)=>if swift { "Bool" } else { "bool" }.into(),Value::Number(_)=>if swift { "UInt64" } else { "u64" }.into(),Value::Null=>if swift { "JSONValue" } else { "Value" }.into(),_=>bail!("non-scalar native literal") });
        }
        if schema["enum"].is_array() || schema["oneOf"].is_array() || schema["anyOf"].is_array() || schema.get("properties").is_some() {
            self.definitions.entry(name.to_owned()).or_insert_with(||schema.clone());
            if !self.emitted.contains(name) { self.pending.insert(name.to_owned()); }
            return Ok(name.to_owned());
        }
        Ok(match schema["type"].as_str() {
            Some("string")=>"String".into(),Some("boolean")=>if swift {"Bool"} else {"bool"}.into(),
            Some("integer")=>if schema["minimum"].as_f64().is_some_and(|n|n>=0.0) { if swift {"UInt64"} else {"u64"} } else if swift {"Int64"} else {"i64"}.into(),
            Some("number")=>if swift {"Double"} else {"f64"}.into(),
            Some("null")=>if swift {"JSONValue"} else {"Value"}.into(),
            Some("array")=>{ let child=self.type_name(&schema["items"],&format!("{name}Item"),swift)?; if swift {format!("[{child}]")} else {format!("Vec<{child}>")} },
            Some("object")=>{
                let extra=schema.get("additionalProperties");
                let child=if let Some(extra)=extra.filter(|extra|extra.is_object()) { self.type_name(extra,&format!("{name}Value"),swift)? } else if extra==Some(&Value::Bool(false)) {
                    self.definitions.entry(name.to_owned()).or_insert_with(||schema.clone()); if !self.emitted.contains(name) {self.pending.insert(name.to_owned());} return Ok(name.to_owned());
                } else { if swift {"JSONValue"} else {"Value"}.into() };
                if swift { format!("[String: {child}]") } else {format!("BTreeMap<String,{child}>")}
            },
            None=>if swift {"JSONValue"} else {"Value"}.into(),Some(other)=>bail!("unsupported native model type {other}"),
        })
    }
    fn model(&mut self,name:&str,schema:&Value)->Result<()> {
        self.emitted.insert(name.to_owned());
        if matches!(name,"NativeJsonValue"|"NativeJsonObject") { return Ok(()); }
        if let Some(values)=schema["enum"].as_array() {
            writeln!(self.rust,"#[derive(Clone,Debug,Deserialize,Serialize,PartialEq)]\npub enum {name} {{")?;
            writeln!(self.swift,"public enum {name}: String, Codable, Sendable {{")?;
            for value in values {
                let raw=value.as_str().context("native enum strings")?;
                let variant=super::pascal(raw);
                let variant=if variant=="Self" { "SelfValue".to_owned() } else { variant };
                writeln!(self.rust,"#[serde(rename={value})] {variant},")?;
                writeln!(self.swift,"case {} = {value}",super::lower_camel(&variant))?;
            }
            writeln!(self.rust,"}}")?; writeln!(self.swift,"}}")?; return Ok(());
        }
        if let Some(branches)=schema["oneOf"].as_array().or_else(||schema["anyOf"].as_array()) {
            writeln!(self.rust,"#[derive(Clone,Debug,Serialize,PartialEq)]\n#[serde(untagged)]\npub enum {name} {{")?;
            writeln!(self.swift,"public indirect enum {name}: Codable, Sendable {{")?;
            let boundary=matches!(name,"SubjectClaim"|"SubjectProjection");
            let mut variants=Vec::new();
            for (index,branch) in branches.iter().enumerate() {
                let rust_type=self.type_name(branch,&format!("{name}Variant{index}"),false)?;
                let swift_type=self.type_name(branch,&format!("{name}Variant{index}"),true)?;
                let variant=format!("Variant{index}");
                let schema_name=branch["$ref"].as_str().and_then(|s|s.strip_prefix("#/$defs/")).unwrap_or(&rust_type).to_owned();
                // Primitive union members also need a named validation entry.
                let schema_name=if self.definitions.contains_key(&schema_name) {schema_name} else {let key=format!("{name}Variant{index}Schema");self.definitions.insert(key.clone(),branch.clone());key};
                writeln!(self.rust,"{variant}(Box<{rust_type}>),")?;
                writeln!(self.swift,"case variant{index}({swift_type})")?;
                variants.push((variant,rust_type,swift_type,schema_name));
            }
            if boundary { writeln!(self.rust,"Unsupported(UnsupportedSubjectSchema),")?;writeln!(self.swift,"case unsupported(UnsupportedSubjectSchema)")?; }
            writeln!(self.rust,"}}\nimpl<'de> Deserialize<'de> for {name} {{ fn deserialize<D:serde::Deserializer<'de>>(decoder:D)->Result<Self,D::Error> {{ let value=Value::deserialize(decoder)?;")?;
            writeln!(self.swift,"public init(from decoder: Decoder) throws {{ let box = try decoder.singleValueContainer(); let value = try box.decode(JSONValue.self)")?;
            if boundary {
                let supported=if name=="SubjectClaim" {"native_claim_supported"} else {"native_subject_supported"};
                let swift_supported=if name=="SubjectClaim" {"nativeClaimSupported"} else {"nativeSubjectSupported"};
                let header=if name=="SubjectClaim" {"NativeClaimHeader"} else {"NativeSubjectHeader"};
                writeln!(self.rust,"if !native_valid(&native_definitions()[{header:?}],&value) || {} {{ return Err(serde::de::Error::custom(\"malformed native subject header\")); }}",if name=="SubjectProjection" {"value[\"id\"] != value[\"ref\"]"} else {"false"})?;
                writeln!(self.swift,"guard nativeModelValid({header:?}, value){} else {{ throw DecodingError.dataCorruptedError(in: box, debugDescription: \"Malformed native subject header\") }}",if name=="SubjectProjection" {" && value.nativeObject?[\"id\"] == value.nativeObject?[\"ref\"]"} else {""})?;
                writeln!(self.rust,"if !{supported}(&value) {{ return UnsupportedSubjectSchema::from_value(&value).map(Self::Unsupported).map_err(serde::de::Error::custom); }}")?;
                writeln!(self.swift,"if !{swift_supported}(value) {{ self = .unsupported(try .init(value)); return }}")?;
            }
            for (index,(variant,rust_type,swift_type,schema_name)) in variants.iter().enumerate() {
                let frame=name=="SubjectCollectionFrame";
                writeln!(self.rust,"if {}(&native_definitions()[{schema_name:?}],&value) {{ return serde_json::from_value::<{rust_type}>(value).map(|value|Self::{variant}(Box::new(value))).map_err(serde::de::Error::custom); }}",if frame {"native_frame_header_valid"} else {"native_valid"})?;
                writeln!(self.swift,"if {}({schema_name:?}, value) {{ self = .variant{index}(try box.decode({swift_type}.self)); return }}",if frame {"nativeFrameHeaderValid"} else {"nativeModelValid"})?;
            }
            writeln!(self.rust,"Err(serde::de::Error::custom(\"malformed native subject payload\")) }} }}")?;
            writeln!(self.swift,"throw DecodingError.dataCorruptedError(in: box, debugDescription: \"Malformed native subject payload\") }}\npublic func encode(to encoder: Encoder) throws {{ var box = encoder.singleValueContainer(); switch self {{")?;
            for (index,_) in variants.iter().enumerate() {writeln!(self.swift,"case .variant{index}(let value): try box.encode(value)")?;}
            if boundary {writeln!(self.swift,"case .unsupported(let value): try box.encode(value)")?;}
            writeln!(self.swift,"}} }} }}")?;return Ok(());
        }
        if schema["type"]=="object" {
            let empty_properties=Map::new();
            if let Some(properties)=schema["properties"].as_object().or_else(||(schema["additionalProperties"]==false).then_some(&empty_properties)) {
                writeln!(self.rust,"#[derive(Clone,Debug,Serialize,PartialEq)]\n#[serde(deny_unknown_fields)]\npub struct {name} {{")?;
                writeln!(self.swift,"public struct {name}: Codable, Sendable {{")?;
                let mut fields=Vec::new();
                let mut rust_fields=String::new();
                let mut rust_keys=Vec::new();
                for (field,definition) in properties {
                    let required=schema["required"].as_array().is_some_and(|items|items.iter().any(|item|item==field));
                    let preserves=definition["x-st-preserve-null"]==true;
                    let rust_type=self.type_name(definition,&format!("{name}{}",super::pascal(field)),false)?;
                    let swift_type=self.type_name(definition,&format!("{name}{}",super::pascal(field)),true)?;
                    let rust_field=super::rust_field(&field.replace(['.','-'],"_"));
                    let swift_field=swift_field(field);
                    if preserves {
                        writeln!(rust_fields,"#[serde(default,skip_serializing_if=\"ProjectedField::is_absent\",rename={field:?})] pub {rust_field}:ProjectedField<{rust_type}>,")?;
                        writeln!(self.swift,"public let {swift_field}: ProjectedField<{swift_type}>")?;
                    } else if required {
                        writeln!(rust_fields,"#[serde(rename={field:?})] pub {rust_field}:{rust_type},")?;
                        writeln!(self.swift,"public let {swift_field}: {swift_type}")?;
                    } else {
                        writeln!(rust_fields,"#[serde(default,skip_serializing_if=\"Option::is_none\",rename={field:?})] pub {rust_field}:Option<{rust_type}>,")?;
                        writeln!(self.swift,"public let {swift_field}: {swift_type}?")?;
                    }
                    fields.push((field.clone(),swift_field,swift_type,required,preserves));
                    rust_keys.push(rust_field);
                }
                self.rust.push_str(&rust_fields);
                writeln!(self.rust,"}}\nimpl<'de> Deserialize<'de> for {name} {{ fn deserialize<D:serde::Deserializer<'de>>(decoder:D)->Result<Self,D::Error> {{ let value=Value::deserialize(decoder)?;")?;
                let frame=["items","upserts"].iter().any(|key|matches!(properties.get(*key).and_then(|field|field["items"]["$ref"].as_str()),Some("#/$defs/SubjectProjection"|"#/$defs/SubjectClaim")));
                let claim=properties.contains_key("schema_id") && properties.contains_key("kind") && properties.contains_key("fields");
                let subject=properties.contains_key("schema_id") && properties.contains_key("family") && properties.contains_key("heads");
                writeln!(self.rust,"if !{}(&native_definitions()[{name:?}],&value){} {{ return Err(serde::de::Error::custom(\"malformed native subject payload\")); }}",if frame {"native_frame_header_valid"} else {"native_valid"},if claim {" || !native_claim_supported(&value)"} else if subject {" || !native_subject_supported(&value) || value[\"id\"] != value[\"ref\"]"} else {""})?;
                writeln!(self.rust,"#[derive(Deserialize)] #[serde(deny_unknown_fields)] struct Wire {{ {rust_fields} }}\nlet _wire:Wire=serde_json::from_value(value).map_err(serde::de::Error::custom)?;\nOk(Self {{")?;
                for key in &rust_keys { writeln!(self.rust,"{key}:_wire.{key},")?; }
                writeln!(self.rust,"}}) }} }}")?;
                if fields.is_empty() {
                    writeln!(self.swift,"public init(from decoder: Decoder) throws {{ let box = try decoder.singleValueContainer(); let value = try box.decode(JSONValue.self); guard nativeModelValid({name:?}, value) else {{ throw DecodingError.dataCorruptedError(in: box, debugDescription: \"Malformed native subject payload\") }} }} }}")?;
                    return Ok(());
                }
                writeln!(self.swift,"enum CodingKeys: String, CodingKey {{")?;
                for (field,key,_,_,_) in &fields {writeln!(self.swift,"case {key} = {field:?}")?;}
                writeln!(self.swift,"}}\npublic init(from decoder: Decoder) throws {{ let raw = try decoder.singleValueContainer(); let value = try raw.decode(JSONValue.self)")?;
                writeln!(self.swift,"guard {}({name:?}, value){} else {{ throw DecodingError.dataCorruptedError(in: raw, debugDescription: \"Malformed native subject payload\") }}\nlet box = try decoder.container(keyedBy: CodingKeys.self)",if frame {"nativeFrameHeaderValid"} else {"nativeModelValid"},if claim {" && nativeClaimSupported(value)"} else if subject {" && nativeSubjectSupported(value) && value.nativeObject?[\"id\"] == value.nativeObject?[\"ref\"]"} else {""})?;
                for (_,key,ty,required,preserves) in &fields {
                    if *preserves { writeln!(self.swift,"{key} = !box.contains(.{key}) ? .absent : try box.decode(ProjectedField<{ty}>.self, forKey: .{key})")?; }
                    else if *required {writeln!(self.swift,"{key} = try box.decode({ty}.self, forKey: .{key})")?;}
                    else {writeln!(self.swift,"{key} = try box.decodeIfPresent({ty}.self, forKey: .{key})")?;}
                }
                writeln!(self.swift,"}}\npublic func encode(to encoder: Encoder) throws {{ var box = encoder.container(keyedBy: CodingKeys.self)")?;
                for (_,key,_,required,preserves) in &fields {
                    if *preserves {writeln!(self.swift,"if case .absent = {key} {{ }} else {{ try box.encode({key}, forKey: .{key}) }}")?;}
                    else if *required {writeln!(self.swift,"try box.encode({key}, forKey: .{key})")?;}
                    else {writeln!(self.swift,"try box.encodeIfPresent({key}, forKey: .{key})")?;}
                }
                writeln!(self.swift,"}} }}")?;return Ok(());
            }
        }
        let rust_type=self.type_name(schema,&format!("{name}Inner"),false)?;
        let swift_type=self.type_name(schema,&format!("{name}Inner"),true)?;
        writeln!(self.rust,"pub type {name}={rust_type};")?;
        writeln!(self.swift,"public typealias {name} = {swift_type}")?;
        Ok(())
    }
}
fn swift_field(name:&str)->String {
    let name=if name.contains(['.','-']) {super::lower_camel(&super::pascal(name))} else {super::lower_camel_snake(name)};
    if matches!(name.as_str(),"ref"|"type"|"default"|"class"|"struct"|"enum"|"case"|"repeat"|"operator"|"protocol"|"extension"|"internal"|"private"|"public"|"switch"|"where"|"let"|"var"|"is"|"as"|"in"|"import"|"return") {format!("`{name}`")} else {name}
}
impl Contract {
    pub fn native_models(&self,schema:&Value)->Result<(String,String)> {
        let mut models=Models {definitions:self.definitions.clone(),pending:self.definitions.keys().cloned().collect(),emitted:BTreeSet::new(),rust:String::new(),swift:String::new()};
        while let Some(name)=models.pending.pop_first() { let definition=models.definitions[&name].clone(); models.model(&name,&definition)?; }
        // Existing semantic base types stay owned by the original operational client contract.
        let mut runtime=schema["$defs"].as_object().context("client schema definitions")?.clone();
        runtime.extend(models.definitions);
        let claim_headers=self.claims.keys().map(|name|self.definitions[name].clone()).collect::<Vec<_>>();
        let mut claim_header=claim_headers.first().context("native claim header")?.clone();
        claim_header["additionalProperties"]=json!(true);
        let properties=claim_header["properties"].as_object_mut().context("claim header properties")?;
        properties.remove("fields");
        for field in ["id","kind","schema_id"] { properties.insert(field.into(),json!({"type":"string","minLength":1})); }
        properties.insert("ref".into(),st3_schema::subject_reference_schema(&[]));
        let retentions=claim_headers.iter().filter_map(|header|header["properties"]["retention"]["const"].as_str()).collect::<BTreeSet<_>>();
        properties.insert("retention".into(),json!({"enum":retentions}));
        claim_header["required"].as_array_mut().context("claim header required")?.retain(|field|field!="fields");
        runtime.insert("NativeClaimHeader".into(),claim_header);
        let subject_name=self.families.keys().next().context("native subject header")?;
        let mut subject_header=self.definitions[subject_name].clone();
        subject_header["additionalProperties"]=json!(true);
        for field in ["id","schema_id"] { subject_header["properties"][field]=json!({"type":"string","minLength":1}); }
        subject_header["properties"]["family"]=json!({"type":"string","pattern":"^[a-z][a-z0-9-]*$"});
        subject_header["properties"]["ref"]=st3_schema::subject_reference_schema(&[]);
        subject_header["properties"]["heads"]["items"]=json!({"$ref":"#/$defs/NativeClaimHeader"});
        runtime.insert("NativeSubjectHeader".into(),subject_header);
        let schemas_json=serde_json::to_string(&runtime)?;
        let custom_json=serde_json::to_string(&self.custom_effective)?;
        let mut rust=super::render_marker(include_str!("../templates/Subjects.rs.in"),"// @st3-codegen:native-rust-models",&models.rust)?;
        writeln!(rust,"const NATIVE_SCHEMAS_JSON:&str={schemas_json:?};\nconst CUSTOM_EFFECTIVE_JSON:&str={custom_json:?};")?;
        writeln!(rust,"const FAMILY_BINDINGS:&[(&str,&str)]=&[")?;
        let mut family_bindings=BTreeMap::new();
        for (name,id) in &self.families {
            let family=self.definitions[name]["properties"]["family"]["const"].as_str().context("native family")?;
            writeln!(rust,"({id:?},{family:?}),")?;family_bindings.insert(id.clone(),family.to_owned());
        }
        writeln!(rust,"];\nconst CLAIM_BINDINGS:&[(&str,&str,&str)]=&[")?;
        let mut claim_bindings=Map::new();
        for (name,id) in &self.claims {
            let kind=self.definitions[name]["properties"]["kind"]["const"].as_str().context("native kind")?;
            let family=self.definitions[name]["properties"]["ref"]["x-st-native-ref-families"][0].as_str().context("native reference family")?;
            writeln!(rust,"({id:?},{family:?},{kind:?}),")?;
            claim_bindings.insert(id.clone(),json!({"family":family,"kind":kind}));
        }
        writeln!(rust,"];")?;
        let mut swift=super::render_marker(include_str!("../templates/Subjects.swift.in"),"// @st3-codegen:native-swift-models",&models.swift)?;
        // Swift raw string delimiters preserve JSON backslashes without hand escaping.
        writeln!(swift,"private let nativeSchemas: [String: JSONValue] = try! JSONDecoder().decode([String: JSONValue].self, from: Data(###\"{schemas_json}\"###.utf8))")?;
        writeln!(swift,"private let customEffectiveJSON = ###\"{custom_json}\"###")?;
        writeln!(swift,"private let nativeFamilyBindings: [String: String] = {}",json!(family_bindings).to_string().replace('{',"[").replace('}',"]"))?;
        writeln!(swift,"private let nativeClaimBindings: [String: (family: String, kind: String)] = [")?;
        for (id,binding) in claim_bindings {writeln!(swift,"{id:?}: (family: {}, kind: {}),",binding["family"],binding["kind"])?;}
        writeln!(swift,"]")?;
        Ok((rust,swift))
    }
}
