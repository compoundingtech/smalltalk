//! Client-only unknown-field filtering. The existing derived Config parser still validates
//! every known value; daemon readers never use this adapter or change deny_unknown_fields.
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

pub(super) fn parse(text: &str) -> Result<(super::Config, Vec<String>), toml::de::Error> {
    let mut unknown = Vec::new();
    let config = super::Config::deserialize(ClientDeserializer {
        inner: toml::de::Deserializer::parse(text)?,
        path: String::new(),
        unknown: &mut unknown,
    })?;
    unknown.sort();
    unknown.dedup();
    Ok((config, unknown))
}

fn key_path(parent: &str, key: &str) -> String {
    // Escape quotes, control characters, Unicode and terminal escapes, without any values.
    let escaped: String = key.chars().flat_map(char::escape_default).collect();
    format!("{parent}[\"{escaped}\"]")
}

struct ClientDeserializer<'a, D> {
    inner: D,
    path: String,
    unknown: &'a mut Vec<String>,
}

struct ClientVisitor<'a, V> {
    inner: V,
    path: String,
    unknown: &'a mut Vec<String>,
    fields: Option<&'static [&'static str]>,
}

impl<'de, D: Deserializer<'de>> Deserializer<'de> for ClientDeserializer<'_, D> {
    type Error = D::Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, D::Error> {
        self.inner.deserialize_any(ClientVisitor {
            inner: visitor,
            path: self.path,
            unknown: self.unknown,
            fields: None,
        })
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, D::Error> {
        // The derive supplies its actual field names (including aliases), avoiding a second
        // schema or any change to the shared types' strict deserialization implementations.
        self.inner.deserialize_struct(
            name,
            fields,
            ClientVisitor {
                inner: visitor,
                path: self.path,
                unknown: self.unknown,
                fields: Some(fields),
            },
        )
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, D::Error> {
        self.inner.deserialize_option(ClientVisitor {
            inner: visitor,
            path: self.path,
            unknown: self.unknown,
            fields: None,
        })
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, D::Error> {
        self.inner.deserialize_newtype_struct(
            name,
            ClientVisitor {
                inner: visitor,
                path: self.path,
                unknown: self.unknown,
                fields: None,
            },
        )
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, D::Error> {
        // Config enums are scalar policies: unknown variants must remain errors.
        self.inner.deserialize_enum(name, variants, visitor)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf unit unit_struct seq tuple tuple_struct map identifier ignored_any
    }
}

macro_rules! scalar_visits {
    ($($method:ident($ty:ty)),* $(,)?) => {$(
        fn $method<E: de::Error>(self, value: $ty) -> Result<Self::Value, E> {
            self.inner.$method(value)
        }
    )*};
}

impl<'de, V: Visitor<'de>> Visitor<'de> for ClientVisitor<'_, V> {
    type Value = V::Value;
    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.inner.expecting(formatter)
    }
    scalar_visits! {
        visit_bool(bool), visit_i8(i8), visit_i16(i16), visit_i32(i32), visit_i64(i64),
        visit_i128(i128), visit_u8(u8), visit_u16(u16), visit_u32(u32), visit_u64(u64),
        visit_u128(u128), visit_f32(f32), visit_f64(f64), visit_char(char),
        visit_str(&str), visit_borrowed_str(&'de str), visit_string(String),
        visit_bytes(&[u8]), visit_borrowed_bytes(&'de [u8]), visit_byte_buf(Vec<u8>)
    }
    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        self.inner.visit_unit()
    }
    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        self.inner.visit_none()
    }
    fn visit_some<D: Deserializer<'de>>(self, inner: D) -> Result<Self::Value, D::Error> {
        self.inner.visit_some(ClientDeserializer {
            inner,
            path: self.path,
            unknown: self.unknown,
        })
    }
    fn visit_newtype_struct<D: Deserializer<'de>>(self, inner: D) -> Result<Self::Value, D::Error> {
        self.inner.visit_newtype_struct(ClientDeserializer {
            inner,
            path: self.path,
            unknown: self.unknown,
        })
    }
    fn visit_seq<A: SeqAccess<'de>>(self, inner: A) -> Result<Self::Value, A::Error> {
        self.inner.visit_seq(ClientSequence {
            inner,
            path: self.path,
            unknown: self.unknown,
            index: 0,
        })
    }
    fn visit_map<A: MapAccess<'de>>(self, inner: A) -> Result<Self::Value, A::Error> {
        self.inner.visit_map(ClientMap {
            inner,
            path: self.path,
            unknown: self.unknown,
            fields: self.fields,
            value_path: None,
        })
    }
}

struct ClientSeed<'a, S> {
    inner: S,
    path: String,
    unknown: &'a mut Vec<String>,
}
impl<'de, S: DeserializeSeed<'de>> DeserializeSeed<'de> for ClientSeed<'_, S> {
    type Value = S::Value;
    fn deserialize<D: Deserializer<'de>>(self, inner: D) -> Result<Self::Value, D::Error> {
        self.inner.deserialize(ClientDeserializer {
            inner,
            path: self.path,
            unknown: self.unknown,
        })
    }
}

struct ClientMap<'a, A> {
    inner: A,
    path: String,
    unknown: &'a mut Vec<String>,
    fields: Option<&'static [&'static str]>,
    value_path: Option<String>,
}
impl<'de, A: MapAccess<'de>> MapAccess<'de> for ClientMap<'_, A> {
    type Error = A::Error;
    fn next_key_seed<S: DeserializeSeed<'de>>(
        &mut self,
        seed: S,
    ) -> Result<Option<S::Value>, A::Error> {
        while let Some(key) = self.inner.next_key::<String>()? {
            let path = key_path(&self.path, &key);
            if self
                .fields
                .is_some_and(|fields| !fields.contains(&key.as_str()))
            {
                self.inner.next_value::<de::IgnoredAny>()?;
                self.unknown.push(path);
                continue;
            }
            self.value_path = Some(path);
            return seed
                .deserialize(de::value::StringDeserializer::<A::Error>::new(key))
                .map(Some);
        }
        Ok(None)
    }
    fn next_value_seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<S::Value, A::Error> {
        self.inner.next_value_seed(ClientSeed {
            inner: seed,
            path: self.value_path.take().unwrap_or_else(|| self.path.clone()),
            unknown: self.unknown,
        })
    }
}

struct ClientSequence<'a, A> {
    inner: A,
    path: String,
    unknown: &'a mut Vec<String>,
    index: usize,
}
impl<'de, A: SeqAccess<'de>> SeqAccess<'de> for ClientSequence<'_, A> {
    type Error = A::Error;
    fn next_element_seed<S: DeserializeSeed<'de>>(
        &mut self,
        seed: S,
    ) -> Result<Option<S::Value>, A::Error> {
        let path = format!("{}[{}]", self.path, self.index);
        self.index += 1;
        self.inner.next_element_seed(ClientSeed {
            inner: seed,
            path,
            unknown: self.unknown,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_ignores_unknown_top_nested_optional_and_array_keys() {
        let text = r#"
node = "fixture"
seat_priority = "normal"
future_root = { secret = "do-not-print" }
[github]
future_token = "do-not-print"
[observations.otlp]
endpoint = "http://127.0.0.1:4318"
future_export = true
[[peers]]
name = "peer"
url = "http://127.0.0.1:9000"
future_route = 7
"#;
        let (config, unknown) = parse(text).unwrap();
        assert_eq!(config.node, "fixture");
        assert_eq!(
            config.active_seat_priority(),
            st_runtime::SeatPriority::Normal
        );
        assert_eq!(config.peers[0].name, "peer");
        assert_eq!(
            unknown,
            [
                "[\"future_root\"]",
                "[\"github\"][\"future_token\"]",
                "[\"observations\"][\"otlp\"][\"future_export\"]",
                "[\"peers\"][0][\"future_route\"]",
            ]
        );
        let warning = super::super::Config::unknown_client_keys_message(&unknown);
        assert!(!warning.contains("do-not-print"));
        assert_eq!(warning.lines().count(), 1);
    }

    #[test]
    fn client_known_types_variants_required_fields_and_semantics_remain_strict() {
        for text in [
            "seat_priority = 'high'",
            "seat_priority = 3",
            "[checkpoint]\nenabled = 'yes'",
            "[limits]\nstop_at_weekly_percent = 'many'",
            "[observations.otlp]\nfuture_key = true",
            "[[peers]]\nname = 4",
            "[github]\nsekrets_profile = 7",
            "[planner]\nprovider = 'codex'\nmodel = 3",
            "node = [",
        ] {
            assert!(
                parse(text).is_err(),
                "known malformed configuration was accepted: {text}"
            );
        }
        let (config, _) = parse("future = true\n[observations]\nretention = '1m'").unwrap();
        assert!(
            config.validate().is_err(),
            "existing semantic validation must still refuse"
        );
    }

    #[test]
    fn client_paths_escape_control_quotes_unicode_and_terminal_bytes_without_values() {
        let text = "\"bad\\n\\u001b\\\"\\\\\\u202e\" = 'hidden-secret'\n";
        let (_, unknown) = parse(text).unwrap();
        assert_eq!(unknown, ["[\"bad\\n\\u{1b}\\\"\\\\\\u{202e}\"]"]);
        let warning = super::super::Config::unknown_client_keys_message(&unknown);
        assert!(warning.is_ascii());
        assert_eq!(warning.lines().count(), 1);
        assert!(!warning.contains('\u{1b}'));
        assert!(!warning.contains("hidden-secret"));
    }

    #[test]
    fn strict_daemon_loader_refuses_what_the_client_loader_ignores() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        std::fs::write(
            &path,
            "node = 'fixture'\nfuture_root = 1\n[checkpoint]\nfuture_nested = 2\n",
        )
        .unwrap();
        let (config, unknown) = super::super::Config::load_client_unvalidated(Some(&path)).unwrap();
        assert_eq!(config.node, "fixture");
        assert_eq!(unknown.len(), 2);
        assert!(super::super::Config::load_unvalidated(Some(&path)).is_err());
        assert!(super::super::Config::load(Some(&path)).is_err());
        assert!(super::super::Config::load_with_fleet(Some(&path)).is_err());
        // An unknown key in a nested peer row is independently rejected by the real
        // strict loader; client support does not change PeerConfig's shared derive.
        std::fs::write(
            &path,
            "[[peers]]\nname = 'peer'\nurl = 'http://127.0.0.1:9000'\nfuture_peer_key = true\n",
        )
        .unwrap();
        assert!(super::super::Config::load_client_unvalidated(Some(&path)).is_ok());
        assert!(super::super::Config::load_unvalidated(Some(&path)).is_err());
    }

    #[test]
    fn client_adapter_preserves_known_default_values() {
        let config = super::super::Config::default();
        let (parsed, unknown) = parse(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(parsed, config);
        assert!(unknown.is_empty());
    }
    #[test]
    fn client_warning_caps_paths_and_names_in_one_diagnostic() {
        let text = (0..20)
            .map(|index| format!("{}_{index} = 'hidden-value'\n", "k".repeat(500)))
            .collect::<String>();
        let (_, unknown) = parse(&text).unwrap();
        assert_eq!(unknown.len(), 20);
        let warning = super::super::Config::unknown_client_keys_message(&unknown);
        assert!(warning.len() < 4096);
        assert_eq!(warning.lines().count(), 1);
        assert!(!warning.contains(&"k".repeat(161)));
        assert!(warning.contains("and 4 more key paths"));
        assert!(!warning.contains("hidden-value"));
    }
}
