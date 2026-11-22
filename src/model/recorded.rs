//! A persisted value together with the exact bytes it was recorded as.
//!
//! A digest over a record must cover the bytes that were written, not a
//! fresh serialization of the parsed value: a later CPS can serialize the
//! same value differently, for example once a field gains a default. A
//! `Recorded` value keeps its original JSON text, writes it back unchanged,
//! and is digested over that text.

use std::ops::Deref;

use anyhow::Result;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::DeserializeOwned};

use super::digest;

#[derive(Clone, Debug)]
pub(crate) struct Recorded<T> {
    text: String,
    value: T,
}

impl<T: Serialize> Recorded<T> {
    /// Record a value as its current JSON serialization.
    pub(crate) fn new(value: T) -> Result<Self> {
        Ok(Self {
            text: serde_json::to_string(&value)?,
            value,
        })
    }
}

impl<T> Recorded<T> {
    /// The SHA-256 of the recorded text.
    pub(crate) fn sha256(&self) -> String {
        digest(self.text.as_bytes())
    }

    #[cfg(test)]
    pub(crate) fn text(&self) -> &str {
        &self.text
    }
}

impl<T> Deref for Recorded<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

/// A recorded value is stored as a JSON string that holds its text, so an
/// enclosing record can be rewritten without touching it.
impl<T> Serialize for Recorded<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.text)
    }
}

impl<'de, T: DeserializeOwned> Deserialize<'de> for Recorded<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        let value = serde_json::from_str(&text).map_err(serde::de::Error::custom)?;
        Ok(Self { text, value })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn records_keep_their_text_and_digest_it() -> Result<()> {
        // Spacing and key order that serde would not produce survive a round trip.
        let text = r#"{ "b": 1,  "a": 2 }"#;
        let outer = serde_json::to_string(&BTreeMap::from([("record", text)]))?;
        let parsed: BTreeMap<String, Recorded<BTreeMap<String, u32>>> =
            serde_json::from_str(&outer)?;
        let record = &parsed["record"];
        assert_eq!(record.text(), text);
        assert_eq!(record["a"], 2);
        assert_eq!(record.sha256(), digest(text.as_bytes()));
        assert_eq!(serde_json::to_string(&parsed)?, outer);

        let fresh = Recorded::new(BTreeMap::from([("a".to_owned(), 2u32)]))?;
        assert_eq!(fresh.text(), r#"{"a":2}"#);
        Ok(())
    }
}
