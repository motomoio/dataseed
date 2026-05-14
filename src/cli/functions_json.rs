//! Format the static catalog as the schema documented in README:
//!
//! ```json
//! {
//!   "functions": [
//!     {
//!       "name": "...",
//!       "args": [{"name": "...", "type": "...", "required": true, "default": ...}],
//!       "variadic": "any",        // present only when the function is variadic
//!       "returns": "...",
//!       "example": "..."
//!     }
//!   ]
//! }
//! ```
//!
//! Defaults are emitted as their JSON-native type so consumers can use them
//! directly (`"decimals": 2`, not `"decimals": "2"`).

use serde_json::{json, Map, Value};

use crate::generators::spec::{ArgSpec, ArgType, FunctionSpec, CATALOG};

pub fn catalog_as_json() -> Value {
    let functions: Vec<Value> = CATALOG.iter().map(function_as_json).collect();
    json!({ "functions": functions })
}

fn function_as_json(spec: &FunctionSpec) -> Value {
    let args: Vec<Value> = spec.args.iter().map(arg_as_json).collect();
    let mut obj = Map::new();
    obj.insert("name".into(), Value::String(spec.name.into()));
    obj.insert("args".into(), Value::Array(args));
    if let Some(v) = spec.variadic {
        obj.insert("variadic".into(), Value::String(v.as_str().into()));
    }
    obj.insert("returns".into(), Value::String(spec.returns.into()));
    obj.insert("example".into(), Value::String(spec.example.into()));
    Value::Object(obj)
}

fn arg_as_json(arg: &ArgSpec) -> Value {
    let mut obj = Map::new();
    obj.insert("name".into(), Value::String(arg.name.into()));
    obj.insert("type".into(), Value::String(arg.ty.as_str().into()));
    obj.insert("required".into(), Value::Bool(arg.required));
    if let Some(len) = arg.length {
        obj.insert("length".into(), Value::from(len));
    }
    if let Some(default) = arg.default {
        obj.insert("default".into(), parse_default(arg.ty, default));
    }
    Value::Object(obj)
}

/// Convert the static-string default into a JSON value of the declared type.
/// Array defaults aren't supported in Phase 2 — no generator declares one,
/// and serialising them as raw strings is the safe fallback.
fn parse_default(ty: ArgType, raw: &str) -> Value {
    match ty {
        ArgType::Integer => raw
            .parse::<i64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(raw.into())),
        ArgType::Number => raw
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .unwrap_or_else(|| Value::String(raw.into())),
        ArgType::Boolean => match raw {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => Value::String(raw.into()),
        },
        ArgType::String
        | ArgType::Any
        | ArgType::Array(_)
        | ArgType::ColumnRef
        | ArgType::Range => Value::String(raw.into()),
    }
}
