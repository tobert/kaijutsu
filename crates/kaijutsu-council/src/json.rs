//! An ordered JSON value.
//!
//! `serde_json::Value` sorts object members unless the whole workspace turns on
//! `preserve_order`. Fields the model reads in the order written (instructions,
//! an inline question's options, a case's state) need their order kept, so
//! they use this type, which keeps members in document order.

use std::fmt;

use indexmap::IndexMap;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A JSON value whose objects keep member order. A repeated member name is a
/// decode error.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Json {
    #[default]
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<Json>),
    Object(IndexMap<String, Json>),
}

impl Json {
    /// The string, when this is one.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(s) => Some(s),
            _ => None,
        }
    }
}

impl From<&str> for Json {
    fn from(s: &str) -> Self {
        Json::String(s.to_string())
    }
}

impl From<String> for Json {
    fn from(s: String) -> Self {
        Json::String(s)
    }
}

impl Serialize for Json {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        match self {
            Json::Null => ser.serialize_unit(),
            Json::Bool(b) => ser.serialize_bool(*b),
            Json::Number(n) => n.serialize(ser),
            Json::String(s) => ser.serialize_str(s),
            Json::Array(a) => {
                let mut seq = ser.serialize_seq(Some(a.len()))?;
                for v in a {
                    seq.serialize_element(v)?;
                }
                seq.end()
            }
            Json::Object(o) => {
                let mut map = ser.serialize_map(Some(o.len()))?;
                for (k, v) in o {
                    map.serialize_entry(k, v)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Json;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_unit<E: de::Error>(self) -> Result<Json, E> {
                Ok(Json::Null)
            }
            fn visit_none<E: de::Error>(self) -> Result<Json, E> {
                Ok(Json::Null)
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Json, E> {
                Ok(Json::Bool(v))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Json, E> {
                Ok(Json::Number(v.into()))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Json, E> {
                Ok(Json::Number(v.into()))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Json, E> {
                serde_json::Number::from_f64(v)
                    .map(Json::Number)
                    .ok_or_else(|| E::custom("non-finite number"))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Json, E> {
                Ok(Json::String(v.to_string()))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Json, E> {
                Ok(Json::String(v))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
                let mut out = Vec::new();
                while let Some(v) = seq.next_element()? {
                    out.push(v);
                }
                Ok(Json::Array(out))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
                let mut out = IndexMap::new();
                while let Some((k, v)) = map.next_entry::<String, Json>()? {
                    if out.contains_key(&k) {
                        return Err(de::Error::custom(format!("repeated member {k:?}")));
                    }
                    out.insert(k, v);
                }
                Ok(Json::Object(out))
            }
        }
        de.deserialize_any(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_order_survives_a_round_trip() {
        let text = r#"{"z":1,"a":{"y":[true,null],"b":"x"},"m":"s"}"#;
        let v: Json = serde_json::from_str(text).unwrap();
        assert_eq!(serde_json::to_string(&v).unwrap(), text);
    }

    #[test]
    fn a_repeated_member_is_an_error() {
        let err = serde_json::from_str::<Json>(r#"{"a":1,"a":2}"#).unwrap_err();
        assert!(err.to_string().contains("repeated member"), "{err}");
    }
}
