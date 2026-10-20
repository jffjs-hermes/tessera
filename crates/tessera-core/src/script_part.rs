//! Deterministic scripted parts: a schema + one generic validated dynamic part.
//!
//! Mods (and later, scripts) declare a *schema* for a part type and entity
//! instances hold one [`ScriptPart`] per declared type in the existing
//! `PartMap`. Storing fields in a [`BTreeMap`] of a closed [`PartValue`] enum —
//! not freeform `toml::Value` — keeps iteration order sorted and the value
//! space finite, so saves and replay are deterministic.
//!
//! Validation is fail-closed: unknown fields, wrong kinds, out-of-range
//! integers and missing non-defaulted fields are loud errors, never silent
//! nil-reads. See `docs/determinism.md` (rule 3): there is **no**
//! `HashMap`/`HashSet` and no floating-point gameplay value anywhere here.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ecs::{PartError, PartTypeId};

/// A single runtime value held by a [`ScriptPart`].
///
/// The value set is closed (no freeform TOML, no floats): iteration over the
/// field map is sorted and every value serializes into a fixed shape, so
/// saves round-trip deterministically.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PartValue {
    Int(i64),
    Bool(bool),
    Str(String),
    /// Homogeneous list; the element `PartValue` variants are constrained by
    /// the declaring [`FieldKind`] (e.g. `ListInt` holds only `Int`s).
    List(Vec<PartValue>),
}

/// The declared data shape of one schema field.
///
/// `kind` ∈ {int, bool, string, list<int>, list<string>}. Defaults and range
/// constraints are host-side; a field with no default and no value is a spawn
/// error unless it is `optional`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
pub enum FieldKind {
    Int,
    Bool,
    Str,
    ListInt,
    ListStr,
}

/// One field declaration in a [`ScriptPartType`] schema.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldDef {
    pub kind: FieldKind,
    /// When `true`, an absent value (and no default) is left out of the
    /// `ScriptPart` instead of being an error.
    pub optional: bool,
    /// Default applied host-side when the payload omits this field.
    pub default: Option<PartValue>,
    /// Inclusive lower bound, applied to int values (and each int element of a
    /// `list<int>`).
    pub min: Option<i64>,
    /// Inclusive upper bound, applied to int values (and each int element of a
    /// `list<int>`).
    pub max: Option<i64>,
}

impl FieldDef {
    pub fn new(kind: FieldKind) -> Self {
        Self {
            kind,
            optional: false,
            default: None,
            min: None,
            max: None,
        }
    }

    pub fn int() -> Self {
        Self::new(FieldKind::Int)
    }
    pub fn bool() -> Self {
        Self::new(FieldKind::Bool)
    }
    pub fn str() -> Self {
        Self::new(FieldKind::Str)
    }
    pub fn list_int() -> Self {
        Self::new(FieldKind::ListInt)
    }
    pub fn list_str() -> Self {
        Self::new(FieldKind::ListStr)
    }

    pub fn optional(mut self) -> Self {
        self.optional = true;
        self
    }

    pub fn default(mut self, value: PartValue) -> Self {
        self.default = Some(value);
        self
    }

    pub fn min(mut self, value: i64) -> Self {
        self.min = Some(value);
        self
    }

    pub fn max(mut self, value: i64) -> Self {
        self.max = Some(value);
        self
    }
}

/// A registered, schema-backed scripted part type.
///
/// The schema maps field names to their [`FieldDef`]s in a [`BTreeMap`]; field
/// iteration is therefore sorted and deterministic.
#[derive(Clone, Debug, PartialEq)]
pub struct ScriptPartType {
    pub name: PartTypeId,
    pub schema: BTreeMap<String, FieldDef>,
}

impl ScriptPartType {
    pub fn new(name: impl Into<PartTypeId>, schema: BTreeMap<String, FieldDef>) -> Self {
        Self {
            name: name.into(),
            schema,
        }
    }

    /// Shape check at *define* time: the default (if any) must match the
    /// field's kind and be within its declared range, and `min <= max`. Called
    /// by [`crate::ecs::PartRegistry::register_script_part`] so a malformed
    /// schema is rejected at registration, before content can depend on it.
    pub fn validate_schema(&self) -> Result<(), PartError> {
        for (field, def) in &self.schema {
            if let Some(min) = def.min
                && let Some(max) = def.max
                && min > max
            {
                return Err(schema_err(
                    &self.name,
                    &format!("field '{field}': min ({min}) must not exceed max ({max})"),
                ));
            }
            if let Some(default) = &def.default {
                self.check_default(field, def, default)?;
            }
        }
        Ok(())
    }

    /// Value check at *write* time: validate a TOML payload (the table under
    /// `parts.<Name>`) against the schema into a [`ScriptPart`]. Unknown
    /// fields error, present fields are type- and range-checked, missing
    /// fields get their default (or error when required and non-defaulted).
    pub fn validate(&self, config: &toml::Value) -> Result<ScriptPart, PartError> {
        let table = config
            .as_table()
            .ok_or_else(|| schema_err(&self.name, "payload must be a TOML table"))?;

        let mut fields = BTreeMap::new();
        // Present fields first, so we check both unknown names and value shape.
        for (field, value) in table {
            let def = self.schema.get(field).ok_or_else(|| {
                schema_err(
                    &self.name,
                    &format!("unknown field '{field}' (not declared in the schema)"),
                )
            })?;
            fields.insert(field.clone(), self.check_value(def, field, value)?);
        }
        // Then apply defaults / require the rest.
        for (field, def) in &self.schema {
            if table.contains_key(field) {
                continue;
            }
            if let Some(default) = &def.default {
                self.check_default(field, def, default)?;
                fields.insert(field.clone(), default.clone());
            } else if !def.optional {
                return Err(schema_err(
                    &self.name,
                    &format!("missing required field '{field}' (no default and not optional)"),
                ));
            }
        }
        Ok(ScriptPart(fields))
    }

    /// Validate one payload value against its field definition.
    fn check_value(
        &self,
        def: &FieldDef,
        field: &str,
        value: &toml::Value,
    ) -> Result<PartValue, PartError> {
        match def.kind {
            FieldKind::Int => {
                let n = value
                    .as_integer()
                    .ok_or_else(|| wrong_kind(&self.name, field, "an integer", value))?;
                self.check_range(field, def, n)?;
                Ok(PartValue::Int(n))
            }
            FieldKind::Bool => value
                .as_bool()
                .map(PartValue::Bool)
                .ok_or_else(|| wrong_kind(&self.name, field, "a boolean", value)),
            FieldKind::Str => value
                .as_str()
                .map(|s| PartValue::Str(s.to_owned()))
                .ok_or_else(|| wrong_kind(&self.name, field, "a string", value)),
            FieldKind::ListInt => {
                let array = value
                    .as_array()
                    .ok_or_else(|| wrong_kind(&self.name, field, "an array", value))?;
                let mut items = Vec::with_capacity(array.len());
                for (idx, element) in array.iter().enumerate() {
                    let n = element.as_integer().ok_or_else(|| {
                        wrong_kind(&self.name, field, "an array of integers", element)
                    })?;
                    self.check_range(field, def, n)
                        .map_err(|e| prepend_index(e, idx))?;
                    items.push(PartValue::Int(n));
                }
                Ok(PartValue::List(items))
            }
            FieldKind::ListStr => {
                let array = value
                    .as_array()
                    .ok_or_else(|| wrong_kind(&self.name, field, "an array", value))?;
                let mut items = Vec::with_capacity(array.len());
                for element in array {
                    let s = element.as_str().ok_or_else(|| {
                        wrong_kind(&self.name, field, "an array of strings", element)
                    })?;
                    items.push(PartValue::Str(s.to_owned()));
                }
                Ok(PartValue::List(items))
            }
        }
    }

    fn check_range(&self, field: &str, def: &FieldDef, value: i64) -> Result<(), PartError> {
        if let Some(min) = def.min
            && value < min
        {
            return Err(schema_err(
                &self.name,
                &format!("field '{field}': value {value} is below the minimum {min}"),
            ));
        }
        if let Some(max) = def.max
            && value > max
        {
            return Err(schema_err(
                &self.name,
                &format!("field '{field}': value {value} exceeds the maximum {max}"),
            ));
        }
        Ok(())
    }

    /// A declared default must be consistent with its field kind and range.
    fn check_default(
        &self,
        field: &str,
        def: &FieldDef,
        default: &PartValue,
    ) -> Result<(), PartError> {
        match def.kind {
            FieldKind::Int => {
                let PartValue::Int(n) = default else {
                    return Err(schema_err(
                        &self.name,
                        &format!("field '{field}': default is not an integer"),
                    ));
                };
                self.check_range(field, def, *n)
            }
            FieldKind::Bool => {
                if matches!(default, PartValue::Bool(_)) {
                    Ok(())
                } else {
                    Err(schema_err(
                        &self.name,
                        &format!("field '{field}': default is not a boolean"),
                    ))
                }
            }
            FieldKind::Str => {
                if matches!(default, PartValue::Str(_)) {
                    Ok(())
                } else {
                    Err(schema_err(
                        &self.name,
                        &format!("field '{field}': default is not a string"),
                    ))
                }
            }
            FieldKind::ListInt => match default {
                PartValue::List(items) if items.iter().all(|v| matches!(v, PartValue::Int(_))) => {
                    for (idx, item) in items.iter().enumerate() {
                        if let PartValue::Int(n) = item {
                            self.check_range(field, def, *n)
                                .map_err(|e| prepend_index(e, idx))?;
                        }
                    }
                    Ok(())
                }
                _ => Err(schema_err(
                    &self.name,
                    &format!("field '{field}': default is not an array of integers"),
                )),
            },
            FieldKind::ListStr => match default {
                PartValue::List(items) if items.iter().all(|v| matches!(v, PartValue::Str(_))) => {
                    Ok(())
                }
                _ => Err(schema_err(
                    &self.name,
                    &format!("field '{field}': default is not an array of strings"),
                )),
            },
        }
    }
}

/// A validated scripted part instance: field name → [`PartValue`], stored in a
/// [`BTreeMap`] so iteration order is sorted (deterministic replay/saves). It
/// is always a `Dynamic` (PartMap-stored) part, never a compile-time ECS
/// component.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptPart(pub BTreeMap<String, PartValue>);

// `ScriptPart` is automatically a [`crate::ecs::Part`] via the blanket
// `impl<T: Any + Send + Sync> Part for T` — all its fields are `Send + Sync`.

/// Field accessors mirroring the engine's other parts: `get`/`get_mut` return
/// the raw `PartValue` for a named field.
impl ScriptPart {
    pub fn get(&self, field: &str) -> Option<&PartValue> {
        self.0.get(field)
    }

    pub fn get_mut(&mut self, field: &str) -> Option<&mut PartValue> {
        self.0.get_mut(field)
    }

    pub fn insert(&mut self, field: String, value: PartValue) -> Option<PartValue> {
        self.0.insert(field, value)
    }

    pub fn contains(&self, field: &str) -> bool {
        self.0.contains_key(field)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Sorted field names (deterministic iteration).
    pub fn fields(&self) -> impl Iterator<Item = &String> {
        self.0.keys()
    }
}

fn schema_err(name: &PartTypeId, msg: &str) -> PartError {
    PartError::Schema {
        name: name.to_string(),
        msg: msg.to_string(),
    }
}

fn wrong_kind(name: &PartTypeId, field: &str, expected: &str, value: &toml::Value) -> PartError {
    schema_err(
        name,
        &format!(
            "field '{field}' must be {expected}, got {}",
            type_name(value)
        ),
    )
}

fn type_name(value: &toml::Value) -> &'static str {
    match value {
        toml::Value::String(_) => "a string",
        toml::Value::Integer(_) => "an integer",
        toml::Value::Float(_) => "a float",
        toml::Value::Boolean(_) => "a boolean",
        toml::Value::Datetime(_) => "a datetime",
        toml::Value::Array(_) => "an array",
        toml::Value::Table(_) => "a table",
    }
}

/// Annotate a list-element error with its index so the user can find the
/// offending entry.
fn prepend_index(err: PartError, idx: usize) -> PartError {
    match err {
        PartError::Schema { name, msg } => PartError::Schema {
            name,
            msg: format!("list element {idx}: {msg}"),
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn schema(fields: Vec<(&str, FieldDef)>) -> BTreeMap<String, FieldDef> {
        fields
            .into_iter()
            .map(|(name, def)| (name.to_string(), def))
            .collect()
    }

    fn charge_type() -> ScriptPartType {
        ScriptPartType::new(
            "Charge",
            schema(vec![(
                "charges",
                FieldDef::int().default(PartValue::Int(1)).min(0).max(99),
            )]),
        )
    }

    fn toml(s: &str) -> toml::Value {
        toml::from_str(s).unwrap()
    }

    #[test]
    fn valid_payload_materializes_fields() {
        let part = charge_type().validate(&toml("charges = 3")).unwrap();
        assert_eq!(part.get("charges"), Some(&PartValue::Int(3)));
        assert_eq!(part.len(), 1);
    }

    #[test]
    fn default_applied_when_field_missing() {
        let part = charge_type().validate(&toml("")).unwrap();
        assert_eq!(part.get("charges"), Some(&PartValue::Int(1)));
    }

    #[test]
    fn required_field_without_default_is_error() {
        let t = ScriptPartType::new("Health", schema(vec![("hp", FieldDef::int())]));
        assert!(t.validate(&toml("hp = 3")).is_ok());
        match t.validate(&toml("")) {
            Err(PartError::Schema { msg, .. }) => {
                assert!(msg.contains("missing required field 'hp'"), "{msg}");
            }
            other => panic!("expected Schema error, got {other:?}"),
        }
    }

    #[test]
    fn unknown_field_is_error() {
        match charge_type().validate(&toml("charges = 1\nbogus = 2")) {
            Err(PartError::Schema { msg, .. }) => {
                assert!(msg.contains("unknown field 'bogus'"), "{msg}");
            }
            other => panic!("expected Schema error, got {other:?}"),
        }
    }

    #[test]
    fn wrong_kind_is_error() {
        match charge_type().validate(&toml("charges = \"many\"")) {
            Err(PartError::Schema { msg, .. }) => {
                assert!(msg.contains("must be an integer"), "{msg}");
            }
            other => panic!("expected Schema error, got {other:?}"),
        }
    }

    #[test]
    fn payload_must_be_a_table() {
        match charge_type().validate(&toml::Value::Array(vec![toml::Value::Integer(1)])) {
            Err(PartError::Schema { msg, .. }) => {
                assert!(msg.contains("payload must be a TOML table"), "{msg}");
            }
            other => panic!("expected Schema error, got {other:?}"),
        }
    }

    #[test]
    fn integer_ranges_are_enforced() {
        // Below min.
        match charge_type().validate(&toml("charges = -1")) {
            Err(PartError::Schema { msg, .. }) => {
                assert!(msg.contains("below the minimum 0"), "{msg}");
            }
            other => panic!("expected Schema error, got {other:?}"),
        }
        // Above max.
        match charge_type().validate(&toml("charges = 100")) {
            Err(PartError::Schema { msg, .. }) => {
                assert!(msg.contains("exceeds the maximum 99"), "{msg}");
            }
            other => panic!("expected Schema error, got {other:?}"),
        }
        // Boundary values pass.
        assert!(charge_type().validate(&toml("charges = 0")).is_ok());
        assert!(charge_type().validate(&toml("charges = 99")).is_ok());
    }

    #[test]
    fn bool_and_string_kinds() {
        let t = ScriptPartType::new(
            "Flags",
            schema(vec![
                ("alive", FieldDef::bool()),
                ("label", FieldDef::str().optional()),
            ]),
        );
        let part = t.validate(&toml("alive = true\nlabel = \"hi\"")).unwrap();
        assert_eq!(part.get("alive"), Some(&PartValue::Bool(true)));
        assert_eq!(part.get("label"), Some(&PartValue::Str("hi".into())));

        // Wrong kinds fail.
        match t.validate(&toml("alive = \"yes\"")) {
            Err(PartError::Schema { msg, .. }) => {
                assert!(msg.contains("must be a boolean"), "{msg}");
            }
            other => panic!("expected Schema error, got {other:?}"),
        }
        // Optional omitted => field absent, no error.
        assert!(t.validate(&toml("alive = false")).is_ok());
    }

    #[test]
    fn list_kinds_validate_each_element() {
        let t = ScriptPartType::new(
            "Bag",
            schema(vec![
                ("ids", FieldDef::list_int().min(0).max(9)),
                ("tags", FieldDef::list_str()),
            ]),
        );
        let part = t
            .validate(&toml("ids = [1, 2, 9]\ntags = [\"a\", \"b\"]"))
            .unwrap();
        assert_eq!(
            part.get("ids"),
            Some(&PartValue::List(vec![
                PartValue::Int(1),
                PartValue::Int(2),
                PartValue::Int(9)
            ]))
        );
        assert_eq!(
            part.get("tags"),
            Some(&PartValue::List(vec![
                PartValue::Str("a".into()),
                PartValue::Str("b".into())
            ]))
        );

        // Element out of range reports its index.
        match t.validate(&toml("ids = [0, 12]")) {
            Err(PartError::Schema { msg, .. }) => {
                assert!(msg.contains("list element 1"), "{msg}");
                assert!(msg.contains("exceeds the maximum 9"), "{msg}");
            }
            other => panic!("expected Schema error, got {other:?}"),
        }
        // Wrong element kind.
        match t.validate(&toml("ids = [1, \"x\"]")) {
            Err(PartError::Schema { msg, .. }) => {
                assert!(msg.contains("array of integers"), "{msg}");
            }
            other => panic!("expected Schema error, got {other:?}"),
        }
        // Wrong list type.
        match t.validate(&toml("ids = 5")) {
            Err(PartError::Schema { msg, .. }) => {
                assert!(msg.contains("must be an array"), "{msg}");
            }
            other => panic!("expected Schema error, got {other:?}"),
        }
    }

    #[test]
    fn malformed_schema_rejected_at_define_time() {
        // min > max.
        let bad = ScriptPartType::new("Bad", schema(vec![("x", FieldDef::int().min(5).max(1))]));
        match bad.validate_schema() {
            Err(PartError::Schema { msg, .. }) => {
                assert!(msg.contains("must not exceed"), "{msg}");
            }
            other => panic!("expected Schema error, got {other:?}"),
        }
        // Default of the wrong kind for the field.
        let bad_default = ScriptPartType::new(
            "Bad",
            schema(vec![("x", FieldDef::int().default(PartValue::Bool(false)))]),
        );
        match bad_default.validate_schema() {
            Err(PartError::Schema { msg, .. }) => {
                assert!(msg.contains("default is not an integer"), "{msg}");
            }
            other => panic!("expected Schema error, got {other:?}"),
        }
    }

    #[test]
    fn fields_iterate_in_sorted_order() {
        let t = ScriptPartType::new(
            "Ordered",
            schema(vec![("zeta", FieldDef::int()), ("alpha", FieldDef::int())]),
        );
        let part = t.validate(&toml("alpha = 1\nzeta = 2")).unwrap();
        // BTreeMap orders by bytes: lowercase, so alpha then zeta.
        let names: Vec<&str> = part.fields().map(|s| s.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zeta"]);
        // Into the same order regardless of TOML authoring order above already
        // verified via the byte-sorted key set; assert behavior not source.
    }

    #[test]
    fn script_part_serialization_round_trips() {
        let t = ScriptPartType::new(
            "Charge",
            schema(vec![(
                "charges",
                FieldDef::int().default(PartValue::Int(1)).min(0).max(99),
            )]),
        );
        let part = t.validate(&toml("charges = 7")).unwrap();

        let json = serde_json::to_string(&part).unwrap();
        let back: ScriptPart = serde_json::from_str(&json).unwrap();
        assert_eq!(back, part);
    }

    #[test]
    fn script_part_is_a_dynamic_part_via_downcast() {
        use crate::ecs::Part;
        let mut part = ScriptPart::default();
        part.insert("x".into(), PartValue::Int(1));
        // Blanket Part impl: verify Any-downcast round trip works as the
        // PartMap uses it.
        let boxed: Box<dyn Part> = Box::new(part.clone());
        let got = boxed
            .as_ref()
            .as_any()
            .downcast_ref::<ScriptPart>()
            .unwrap();
        assert_eq!(*got, part);
    }
}
