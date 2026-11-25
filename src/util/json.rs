//! Reading fields out of JSON objects Claude Code writes, without a schema for them.
//!
//! The listing, the registry and the hook payloads are not documented, so each field is read
//! on its own: a missing optional field is `None`, a field of the wrong type is an error that
//! names it, and a field this code does not know is ignored.

use std::path::PathBuf;

use serde_json::{Map, Value};

use crate::types::{Instant, Pid, SessionId};

/// A field that could not be read, named by its key, qualified by the scope it is read in.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FieldError {
    #[error("{key:?} is missing")]
    Missing { key: String },
    #[error("{key:?} is not {expected}")]
    WrongType { key: String, expected: &'static str },
    #[error("{key:?} is not valid: {reason}")]
    Invalid { key: String, reason: String },
}

impl FieldError {
    pub fn missing(key: &str) -> Self {
        Self::Missing {
            key: key.to_owned(),
        }
    }

    pub fn invalid(key: &str, reason: String) -> Self {
        Self::Invalid {
            key: key.to_owned(),
            reason,
        }
    }

    fn wrong_type(key: &str, expected: &'static str) -> Self {
        Self::WrongType {
            key: key.to_owned(),
            expected,
        }
    }
}

/// The fields of one JSON object.
pub struct Fields<'a> {
    object: &'a Map<String, Value>,
    /// What an error names the object as: a field of an element of a list is reported as
    /// `list[].field`.
    scope: Option<&'static str>,
}

impl<'a> Fields<'a> {
    /// `value` as an object whose errors name its keys bare.
    pub fn of(value: &'a Value) -> Result<Self, FieldError> {
        Self::in_scope(value, None)
    }

    /// `value` as an object whose errors name its keys under `scope`, which also names the
    /// object itself when it is not one.
    pub fn scoped(value: &'a Value, scope: &'static str) -> Result<Self, FieldError> {
        Self::in_scope(value, Some(scope))
    }

    fn in_scope(value: &'a Value, scope: Option<&'static str>) -> Result<Self, FieldError> {
        value
            .as_object()
            .map(|object| Self { object, scope })
            .ok_or_else(|| FieldError::wrong_type(scope.unwrap_or("(entry)"), "a JSON object"))
    }

    fn label(&self, key: &str) -> String {
        self.scope
            .map_or_else(|| key.to_owned(), |scope| format!("{scope}.{key}"))
    }

    /// `null` counts as absent: Claude Code writes both for "not set".
    fn get(&self, key: &str) -> Option<&'a Value> {
        self.object.get(key).filter(|v| !v.is_null())
    }

    pub fn optional_array(&self, key: &'static str) -> Result<Option<&'a [Value]>, FieldError> {
        self.get(key)
            .map(|v| {
                v.as_array()
                    .map(Vec::as_slice)
                    .ok_or_else(|| FieldError::wrong_type(&self.label(key), "an array"))
            })
            .transpose()
    }

    pub fn optional_str(&self, key: &'static str) -> Result<Option<&'a str>, FieldError> {
        self.get(key)
            .map(|v| {
                v.as_str()
                    .ok_or_else(|| FieldError::wrong_type(&self.label(key), "a string"))
            })
            .transpose()
    }

    pub fn required_str(&self, key: &'static str) -> Result<&'a str, FieldError> {
        self.optional_str(key)?
            .ok_or_else(|| FieldError::missing(&self.label(key)))
    }

    pub fn optional_millis(&self, key: &'static str) -> Result<Option<Instant>, FieldError> {
        self.get(key)
            .map(|v| {
                let ms = v.as_i64().ok_or_else(|| {
                    FieldError::wrong_type(&self.label(key), "an integer of milliseconds")
                })?;
                Instant::from_millisecond(ms)
                    .map_err(|e| FieldError::invalid(&self.label(key), e.to_string()))
            })
            .transpose()
    }

    pub fn required_millis(&self, key: &'static str) -> Result<Instant, FieldError> {
        self.optional_millis(key)?
            .ok_or_else(|| FieldError::missing(&self.label(key)))
    }

    pub fn optional_pid(&self, key: &'static str) -> Result<Option<Pid>, FieldError> {
        self.get(key)
            .map(|v| {
                v.as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .map(Pid::new)
                    .ok_or_else(|| FieldError::wrong_type(&self.label(key), "a process id"))
            })
            .transpose()
    }

    pub fn required_path(&self, key: &'static str) -> Result<PathBuf, FieldError> {
        self.required_str(key).map(PathBuf::from)
    }

    pub fn optional_session_id(&self, key: &'static str) -> Result<Option<SessionId>, FieldError> {
        self.optional_str(key)?
            .map(|text| {
                SessionId::parse(text)
                    .map_err(|e| FieldError::invalid(&self.label(key), e.to_string()))
            })
            .transpose()
    }

    pub fn required_session_id(&self, key: &'static str) -> Result<SessionId, FieldError> {
        self.optional_session_id(key)?
            .ok_or_else(|| FieldError::missing(&self.label(key)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(json: &str) -> Value {
        serde_json::from_str(json).unwrap()
    }

    /// Mutation: stop filtering `null` in `get` -> `null` reads as wrong type -> red.
    #[test]
    fn null_is_absent() {
        let v = fields(r#"{"name": null}"#);
        assert_eq!(Fields::of(&v).unwrap().optional_str("name"), Ok(None));
    }

    /// Mutation: replace the `WrongType` error with `Ok(None)` -> red.
    #[test]
    fn a_wrong_type_is_an_error_naming_the_field() {
        let v = fields(r#"{"name": 7}"#);
        assert_eq!(
            Fields::of(&v).unwrap().optional_str("name"),
            Err(FieldError::WrongType {
                key: "name".to_owned(),
                expected: "a string"
            })
        );
    }

    /// Mutation: accept negative pids by switching `as_u64` to `as_i64` casting -> red.
    #[test]
    fn a_pid_outside_u32_is_rejected() {
        for bad in ["-1", "4294967296", "\"12\""] {
            let v = fields(&format!(r#"{{"pid": {bad}}}"#));
            assert!(
                Fields::of(&v).unwrap().optional_pid("pid").is_err(),
                "{bad}"
            );
        }
    }

    /// A field of a list element is named under the list: `tasks[].type`.
    /// Mutation: drop the scope from `label`.
    #[test]
    fn a_scoped_field_is_named_under_its_list() {
        let v = fields(r#"{"type": 7}"#);
        let scoped = Fields::scoped(&v, "tasks[]").unwrap();
        assert_eq!(
            scoped.required_str("type"),
            Err(FieldError::WrongType {
                key: "tasks[].type".to_owned(),
                expected: "a string"
            })
        );
        assert_eq!(
            scoped.required_str("id"),
            Err(FieldError::missing("tasks[].id"))
        );
        assert_eq!(
            Fields::scoped(&Value::Null, "tasks[]").err(),
            Some(FieldError::WrongType {
                key: "tasks[]".to_owned(),
                expected: "a JSON object"
            })
        );
    }

    #[test]
    fn millis_become_utc_instants() {
        let v = fields(r#"{"startedAt": 1761645600000}"#);
        let at = Fields::of(&v)
            .unwrap()
            .required_millis("startedAt")
            .unwrap();
        assert_eq!(at.to_string(), "2025-10-28T10:00:00Z");
    }
}
