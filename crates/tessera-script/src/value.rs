//! Value-boundary marshalling between Luau values and the engine's plain-value
//! model ([`tessera_core::script_part::PartValue`], integer coordinates,
//! sorted field tables).
//!
//! Nothing here lets a Rust ECS borrow cross into Luau: every crossing converts
//! a value into an owned [`mlua::Value`] or back. Field tables are marshalled
//! with a canonical sorted key order (determinism rule 4/5/7).

use std::collections::BTreeMap;

use mlua::{Lua, Table, Value};
use tessera_core::script_part::{FieldDef, FieldKind, PartValue, ScriptPartType};

use crate::error::ScriptError;

/// Build a Lua table from a [`PartValue`] field map with **sorted keys**
/// (determinism rule 4/7). Returns the table, or `ScriptError` on a value we
/// cannot represent (there are none of those in the closed v1 set).
pub fn field_table_to_lua(
    lua: &Lua,
    fields: &BTreeMap<String, PartValue>,
) -> Result<Table, ScriptError> {
    let t = lua.create_table()?;
    for (k, v) in fields {
        t.raw_set(k.as_str(), part_value_to_lua(lua, v)?)?;
    }
    Ok(t)
}

/// Convert one engine [`PartValue`] into a Luau value. Ints → integer-valued
/// doubles (Luau has only doubles); bools/strings → directly; lists → an
/// array table.
pub fn part_value_to_lua(lua: &Lua, v: &PartValue) -> Result<Value, ScriptError> {
    Ok(match v {
        PartValue::Int(n) => Value::Integer(*n),
        PartValue::Bool(b) => Value::Boolean(*b),
        PartValue::Str(s) => Value::String(lua.create_string(s)?),
        PartValue::List(items) => {
            let t = lua.create_table()?;
            for (idx, item) in items.iter().enumerate() {
                t.raw_set(idx + 1, part_value_to_lua(lua, item)?)?;
            }
            Value::Table(t)
        }
    })
}

/// Convert a Luau value back into a [`PartValue`], validating it against the
/// closed v1 set. Doubles that are not integer-valued are rejected (an *int*
/// field can never hold a fractional gameplay value, determinism rule 3/7).
pub fn lua_to_part_value(value: &Value) -> Result<PartValue, ScriptError> {
    match value {
        Value::Integer(n) => Ok(PartValue::Int(*n)),
        Value::Number(n) => {
            let as_i = *n as i64;
            if (as_i as f64) == *n {
                Ok(PartValue::Int(as_i))
            } else {
                Err(ScriptError::Internal(format!(
                    "non-integer number {n} cannot be a part value"
                )))
            }
        }
        Value::Boolean(b) => Ok(PartValue::Bool(*b)),
        Value::String(s) => Ok(PartValue::Str(s.to_string_lossy())),
        _ => Err(ScriptError::Internal(format!(
            "unexpected part value type {value:?}"
        ))),
    }
}

/// Parse a part *schema* table as declared by a script (`engine.define_part`)
/// into the core [`tessera_core::script_part::ScriptPartType`]. Validates the
/// shape (kind/set) so a malformed schema is rejected at load time.
pub fn schema_table_to_type(
    lua: &Lua,
    name: &str,
    schema_table: &Table,
) -> Result<ScriptPartType, ScriptError> {
    let fields_table: Table = schema_table
        .get::<Value>("fields")
        .ok()
        .and_then(|v| match v {
            Value::Table(t) => Some(t),
            _ => None,
        })
        .ok_or_else(|| ScriptError::Register {
            script: String::new(),
            msg: format!("define_part('{name}'): 'fields' must be a table"),
        })?;

    let mut schema: BTreeMap<String, FieldDef> = BTreeMap::new();
    for pair in fields_table.pairs::<Value, Value>() {
        let (k, v) = pair.map_err(ScriptError::from)?;
        let field = match &k {
            Value::String(s) => s.to_string_lossy(),
            _ => {
                return Err(ScriptError::Register {
                    script: String::new(),
                    msg: format!("define_part('{name}'): field keys must be strings"),
                });
            }
        };
        let def_table = match v {
            Value::Table(t) => t,
            _ => {
                return Err(ScriptError::Register {
                    script: String::new(),
                    msg: format!("define_part('{name}'): field '{field}' must be a table"),
                });
            }
        };
        schema.insert(
            field.clone(),
            field_def_to_core(name, &field, lua, &def_table)?,
        );
    }
    if schema.is_empty() {
        return Err(ScriptError::Register {
            script: String::new(),
            msg: format!("define_part('{name}'): schema has no fields"),
        });
    }
    Ok(ScriptPartType::new(name, schema))
}

fn field_def_to_core(
    part: &str,
    field: &str,
    _lua: &Lua,
    t: &Table,
) -> Result<FieldDef, ScriptError> {
    let kind: String = match t.get::<Value>("kind") {
        Ok(Value::String(s)) => s.to_string_lossy(),
        _ => {
            return Err(ScriptError::Register {
                script: String::new(),
                msg: format!("define_part('{part}'): field '{field}' needs a string 'kind'"),
            });
        }
    };
    let mut def = match kind.as_str() {
        "int" => FieldDef::int(),
        "bool" => FieldDef::bool(),
        "string" => FieldDef::str(),
        "list<int>" => FieldDef::list_int(),
        "list<string>" => FieldDef::list_str(),
        other => {
            return Err(ScriptError::Register {
                script: String::new(),
                msg: format!("define_part('{part}'): field '{field}' has unknown kind '{other}'"),
            });
        }
    };

    if t.get::<Value>("optional")
        .ok()
        .is_some_and(|v| v == Value::Boolean(true))
    {
        def = def.optional();
    }
    if let Ok(v) = t.get::<Value>("default") {
        if v != Value::Nil {
            let pv = lua_to_part_value(&v)?;
            def = def.default(pv);
        }
    }
    if let Ok(v) = t.get::<Value>("min") {
        if v != Value::Nil {
            def = def.min(match v {
                Value::Integer(n) => n,
                Value::Number(n) => n as i64,
                _ => {
                    return Err(ScriptError::Register {
                        script: String::new(),
                        msg: format!("define_part('{part}'): field '{field}' min must be an int"),
                    });
                }
            });
        }
    }
    if let Ok(v) = t.get::<Value>("max") {
        if v != Value::Nil {
            def = def.max(match v {
                Value::Integer(n) => n,
                Value::Number(n) => n as i64,
                _ => {
                    return Err(ScriptError::Register {
                        script: String::new(),
                        msg: format!("define_part('{part}'): field '{field}' max must be an int"),
                    });
                }
            });
        }
    }
    Ok(def)
}

/// Parse an integer entity id from a Luau value that may be an Integer or an
/// integer-valued Number.
pub fn lua_to_entity_id(value: &Value) -> Result<u64, ScriptError> {
    match value {
        Value::Integer(n) if *n >= 0 => Ok(*n as u64),
        Value::Number(n) if *n >= 0.0 && (*n as i64) as f64 == *n => Ok(*n as i64 as u64),
        _ => Err(ScriptError::Internal(format!(
            "invalid entity id {value:?}"
        ))),
    }
}

/// Marshal event parameters (a fixed set of int/string/bool/id values) into a
/// Lua table. Used by the dispatch seam to hand handler-callable data to a
/// script.
pub fn params_to_lua(lua: &Lua, params: &[(&str, Value)]) -> Result<Table, ScriptError> {
    let t = lua.create_table()?;
    for (k, v) in params {
        t.raw_set(*k, v.clone())?;
    }
    Ok(t)
}

/// Validate a `set_part` write against a scripted part's declared schema
/// (§4.4): every field must be declared and its value must match the field's
/// kind and be within the declared range — the same write-time check a spawn
/// payload gets. Scripts may mutate their own scripted parts freely, but not
/// arbitrarily; a violation is a runtime content bug, surfaced as a `set_part`
/// error (never a panic).
pub fn validate_scripted_write(
    part: &ScriptPartType,
    fields: &BTreeMap<String, PartValue>,
) -> Result<(), ScriptError> {
    for (field, value) in fields {
        let def = part.schema.get(field).ok_or_else(|| {
            ScriptError::Internal(format!(
                "set_part('{}'): unknown field '{field}' (not declared in the schema)",
                part.name.as_str()
            ))
        })?;
        check_scripted_value(part, field, def, value)?;
    }
    Ok(())
}

fn check_scripted_value(
    part: &ScriptPartType,
    field: &str,
    def: &FieldDef,
    value: &PartValue,
) -> Result<(), ScriptError> {
    match def.kind {
        FieldKind::Int => {
            let PartValue::Int(n) = value else {
                return Err(scripted_write_err(part, field, "expected an integer"));
            };
            check_scripted_range(part, field, def, *n)
        }
        FieldKind::Bool => {
            if matches!(value, PartValue::Bool(_)) {
                Ok(())
            } else {
                Err(scripted_write_err(part, field, "expected a boolean"))
            }
        }
        FieldKind::Str => {
            if matches!(value, PartValue::Str(_)) {
                Ok(())
            } else {
                Err(scripted_write_err(part, field, "expected a string"))
            }
        }
        FieldKind::ListInt => match value {
            PartValue::List(items) if items.iter().all(|v| matches!(v, PartValue::Int(_))) => {
                for (idx, item) in items.iter().enumerate() {
                    if let PartValue::Int(n) = item {
                        check_scripted_range(part, field, def, *n).map_err(|e| {
                            ScriptError::Internal(format!("{e} (list index {idx})"))
                        })?;
                    }
                }
                Ok(())
            }
            _ => Err(scripted_write_err(
                part,
                field,
                "expected an array of integers",
            )),
        },
        FieldKind::ListStr => match value {
            PartValue::List(items) if items.iter().all(|v| matches!(v, PartValue::Str(_))) => {
                Ok(())
            }
            _ => Err(scripted_write_err(
                part,
                field,
                "expected an array of strings",
            )),
        },
    }
}

fn check_scripted_range(
    part: &ScriptPartType,
    field: &str,
    def: &FieldDef,
    value: i64,
) -> Result<(), ScriptError> {
    if let Some(min) = def.min {
        if value < min {
            return Err(scripted_write_err(
                part,
                field,
                &format!("value {value} is below the minimum {min}"),
            ));
        }
    }
    if let Some(max) = def.max {
        if value > max {
            return Err(scripted_write_err(
                part,
                field,
                &format!("value {value} exceeds the maximum {max}"),
            ));
        }
    }
    Ok(())
}

fn scripted_write_err(part: &ScriptPartType, field: &str, detail: &str) -> ScriptError {
    ScriptError::Internal(format!(
        "set_part('{}').{field}: {detail}",
        part.name.as_str()
    ))
}
