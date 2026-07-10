//! Field-level diff of two snapshots — the "what really changed" view.
//! Unchanged fields are dropped so an update touching one column reads
//! as one line, not a wall of JSON.

use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;

#[derive(Debug, PartialEq, Serialize)]
pub struct FieldChange {
    pub field: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new: Option<Value>,
}

/// Compare two optional snapshots field by field. Objects are compared
/// per key (union of both sides); anything else is treated as a single
/// `value` field. Equal fields are omitted.
pub fn diff(old: Option<&Value>, new: Option<&Value>) -> Vec<FieldChange> {
    match (old, new) {
        (None, None) => Vec::new(),
        (Some(Value::Object(o)), Some(Value::Object(n))) => {
            let keys: BTreeSet<&String> = o.keys().chain(n.keys()).collect();
            keys.into_iter()
                .filter(|k| o.get(*k) != n.get(*k))
                .map(|k| FieldChange {
                    field: k.clone(),
                    old: o.get(k).cloned(),
                    new: n.get(k).cloned(),
                })
                .collect()
        }
        (Some(Value::Object(o)), None) => o
            .iter()
            .map(|(k, v)| FieldChange {
                field: k.clone(),
                old: Some(v.clone()),
                new: None,
            })
            .collect(),
        (None, Some(Value::Object(n))) => n
            .iter()
            .map(|(k, v)| FieldChange {
                field: k.clone(),
                old: None,
                new: Some(v.clone()),
            })
            .collect(),
        (o, n) if o == n => Vec::new(),
        (o, n) => vec![FieldChange {
            field: "value".into(),
            old: o.cloned(),
            new: n.cloned(),
        }],
    }
}
