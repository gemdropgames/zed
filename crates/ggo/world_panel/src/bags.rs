//! Component field bags <-> document component fields.
//!
//! The cart publishes and accepts a component as a binary FIELD BAG
//! (`name_hash u64 | field_count u16 | section_len u32 | (field_hash u64 |
//! tagged value)…`, the same shape the world blob carries). The document
//! side ([`ggo_worldlib::world_doc`]) holds each component as a
//! `serde_json::Map<String, Value>`. This module is the pure translation
//! between the two, driven by the cart's published schema table
//! ([`SchemaEntry`]) -- a bag names its fields only by hash, so the schema
//! is what supplies both the names and the types to decode them at.
//!
//! Conventions and limits, all deliberate:
//!
//! * `Fixed` and `Vec2` cross as JSON numbers in world PIXELS (`raw /
//!   65536`), matching what the document already stores for
//!   `Transform.pos`. A whole-valued `Fixed` is written by `FieldWriter`
//!   as an `Int`, so both tags are accepted wherever a `Fixed` is expected.
//!   A document value finer than one raw unit (1/65536 px) rounds to the
//!   nearest one on the way in, so it is not what comes back out.
//! * `AssetRef` is a plain string, like `Str`.
//! * A field the bag does not carry is OMITTED from the returned map (and
//!   a field the map does not carry is omitted from the bag). Decoding is
//!   therefore a MERGE: the caller keeps the document's existing value for
//!   every field not present in the returned map. That matters because the
//!   cart publishes only what changed.
//! * A field whose value tag does not fit its schema kind is likewise
//!   omitted rather than failing the whole component: a newer cart's
//!   re-typed field must not blank the rest of the mirror.
//! * A component whose schema has a `Struct` field is NOT convertible in
//!   either direction (`None`). The schema table crosses the wire with a
//!   flat `Struct` tag -- the nested shape is not in it -- so the field
//!   could neither be decoded into the document nor written back, and
//!   since `SetComponent` replaces the WHOLE component, silently dropping
//!   it would reset it on the cart.
//! * A `List` carries ints, decimals, bools and strings (mixed is fine);
//!   a nested list or struct inside one makes the field unconvertible. A
//!   whole-valued decimal in a list round-trips as an integer, since
//!   `Fixed` writes whole values as `Int`.

use emerald_core::{Fixed, Vec2};
use emerald_editor_link::{FieldKind, SchemaEntry};
use emerald_world::{BagField, FieldReader, FieldValue, FieldWriter, field_hash};
use serde_json::{Map, Value};

use crate::live::{from_raw, to_raw};

/// Decode one field bag into `(component name, fields)`. `None` when the
/// bag is malformed, names a component no schema describes, or names one
/// this module cannot convert (see the module docs).
pub fn fields_from_bag(
    schemas: &[SchemaEntry],
    bag: &[u8],
) -> Option<(String, Map<String, Value>)> {
    let (name_hash, reader, _rest) = FieldReader::from_bag(bag)?;
    let schema = convertible(schemas.iter().find(|s| field_hash(&s.name) == name_hash)?)?;
    let mut fields = Map::new();
    for field in &schema.fields {
        let Some(value) = reader.get(field_hash(&field.name)) else {
            continue;
        };
        if let Some(json) = json_from_value(&field.kind, value) {
            fields.insert(field.name.clone(), json);
        }
    }
    Some((schema.name.clone(), fields))
}

/// Encode `fields` as one field bag for `component`. Fields are written in
/// SCHEMA order; a field the map does not carry is omitted, and a key the
/// schema does not name is ignored. `None` when no schema describes the
/// component, when it is not convertible, or when a value's JSON type does
/// not fit its schema kind -- a bag that dropped a mistyped field would
/// reset that field on the cart, so the whole write is refused instead.
pub fn bag_from_fields(
    schemas: &[SchemaEntry],
    component: &str,
    fields: &Map<String, Value>,
) -> Option<Vec<u8>> {
    let schema = convertible(schemas.iter().find(|s| s.name == component)?)?;
    let mut writer = FieldWriter::new();
    for field in &schema.fields {
        let Some(value) = fields.get(&field.name) else {
            continue;
        };
        write_field(&mut writer, &field.name, &field.kind, value)?;
    }
    Some(writer.finish(&schema.name))
}

fn convertible(schema: &SchemaEntry) -> Option<&SchemaEntry> {
    schema
        .fields
        .iter()
        .all(|f| !matches!(f.kind, FieldKind::Struct))
        .then_some(schema)
}

fn json_from_value(kind: &FieldKind, value: FieldValue<'_>) -> Option<Value> {
    match (kind, value) {
        (FieldKind::Int, FieldValue::Int(v)) => Some(Value::from(v)),
        (FieldKind::Fixed, _) => scalar_px(value).map(Value::from),
        (FieldKind::Bool, FieldValue::Bool(v)) => Some(Value::Bool(v)),
        (FieldKind::Str | FieldKind::AssetRef { .. }, FieldValue::Str(v)) => {
            Some(Value::String(v.to_string()))
        }
        (FieldKind::Vec2, FieldValue::List(elements)) => {
            let axes: Option<Vec<f64>> = elements.map(scalar_px).collect();
            let axes = axes?;
            let [x, y] = axes.as_slice() else {
                return None;
            };
            Some(Value::Array(vec![Value::from(*x), Value::from(*y)]))
        }
        (FieldKind::List, FieldValue::List(elements)) => {
            let values: Option<Vec<Value>> = elements.map(json_from_element).collect();
            Some(Value::Array(values?))
        }
        _ => None,
    }
}

/// A `Fixed`-shaped value as world pixels, accepting the `Int` tag a whole
/// number is written under.
fn scalar_px(value: FieldValue<'_>) -> Option<f64> {
    match value {
        FieldValue::Int(v) => Some(f64::from(v)),
        FieldValue::Fixed(raw) => Some(from_raw(raw)),
        _ => None,
    }
}

fn json_from_element(value: FieldValue<'_>) -> Option<Value> {
    match value {
        FieldValue::Int(v) => Some(Value::from(v)),
        FieldValue::Fixed(raw) => Some(Value::from(from_raw(raw))),
        FieldValue::Bool(v) => Some(Value::Bool(v)),
        FieldValue::Str(v) => Some(Value::String(v.to_string())),
        FieldValue::List(_) | FieldValue::Struct(_) => None,
    }
}

fn write_field(
    writer: &mut FieldWriter,
    name: &str,
    kind: &FieldKind,
    value: &Value,
) -> Option<()> {
    match kind {
        FieldKind::Int => writer.int(name, json_int(value)?),
        FieldKind::Fixed => writer.fixed(name, json_fixed(value)?),
        FieldKind::Bool => writer.bool_(name, value.as_bool()?),
        FieldKind::Str | FieldKind::AssetRef { .. } => writer.str_(name, value.as_str()?),
        FieldKind::Vec2 => {
            let [x, y] = value.as_array()?.as_slice() else {
                return None;
            };
            writer.vec2(name, Vec2::new(json_fixed(x)?, json_fixed(y)?));
        }
        FieldKind::List => {
            let elements: Option<Vec<Element<'_>>> =
                value.as_array()?.iter().map(element_from_json).collect();
            writer.field(name, elements?.as_slice());
        }
        FieldKind::Struct => return None,
    }
    Some(())
}

fn json_int(value: &Value) -> Option<i32> {
    if let Some(n) = value.as_i64() {
        return i32::try_from(n).ok();
    }
    // A document that went through a float-valued JSON number still holds a
    // whole number for an `Int` field; anything with a fraction does not.
    let n = value.as_f64()?;
    (n.fract() == 0.0 && n >= f64::from(i32::MIN) && n <= f64::from(i32::MAX)).then_some(n as i32)
}

fn json_fixed(value: &Value) -> Option<Fixed> {
    Some(Fixed::from_raw(to_raw(value.as_f64()?)))
}

/// One list element, so a mixed list can be handed to `FieldWriter` through
/// the same `BagField` slice impl the typed lists use.
enum Element<'a> {
    Int(i32),
    Fixed(Fixed),
    Bool(bool),
    Str(&'a str),
}

impl BagField for Element<'_> {
    fn write_bag(&self, out: &mut Vec<u8>) {
        match self {
            Element::Int(v) => v.write_bag(out),
            Element::Fixed(v) => v.write_bag(out),
            Element::Bool(v) => v.write_bag(out),
            Element::Str(v) => v.write_bag(out),
        }
    }
}

fn element_from_json(value: &Value) -> Option<Element<'_>> {
    match value {
        Value::Bool(v) => Some(Element::Bool(*v)),
        Value::String(v) => Some(Element::Str(v)),
        // An integer too wide for the `Int` tag is a type error, not a
        // coordinate to clamp: routing it through `Fixed` would saturate it
        // into a plausible-looking small number.
        Value::Number(_) if value.is_i64() || value.is_u64() => {
            i32::try_from(value.as_i64()?).ok().map(Element::Int)
        }
        Value::Number(_) => Some(Element::Fixed(json_fixed(value)?)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use emerald_editor_link::SchemaField;
    use serde_json::json;

    fn field(name: &str, kind: FieldKind) -> SchemaField {
        SchemaField {
            name: name.to_string(),
            kind,
        }
    }

    fn asset(ext: &str) -> FieldKind {
        FieldKind::AssetRef {
            ext: ext.to_string(),
        }
    }

    fn entry(name: &str, fields: Vec<SchemaField>) -> SchemaEntry {
        SchemaEntry {
            name: name.to_string(),
            fields,
        }
    }

    /// The eight built-in component shapes, mirroring `emerald_world`'s
    /// `SceneComponent::SCHEMA` constants.
    fn builtins() -> Vec<SchemaEntry> {
        vec![
            entry(
                "Transform",
                vec![field("pos", FieldKind::Vec2), field("z", FieldKind::Int)],
            ),
            entry(
                "Sprite",
                vec![
                    field("stem", asset("png")),
                    field("centered", FieldKind::Bool),
                    field("offset", FieldKind::Vec2),
                    field("visible", FieldKind::Bool),
                ],
            ),
            entry(
                "MetaSprite",
                vec![
                    field("stem", asset("png")),
                    field("centered", FieldKind::Bool),
                    field("offset", FieldKind::Vec2),
                    field("visible", FieldKind::Bool),
                    field("clip", FieldKind::Str),
                    field("loop", FieldKind::Bool),
                ],
            ),
            entry(
                "Text",
                vec![
                    field("font", asset("ttf")),
                    field("content", FieldKind::Str),
                    field("centered", FieldKind::Bool),
                    field("max_width", FieldKind::Int),
                    field("max_height", FieldKind::Int),
                    field("wrap", FieldKind::Str),
                    field("visible", FieldKind::Bool),
                ],
            ),
            entry(
                "Tilemap",
                vec![
                    field("layer", FieldKind::Str),
                    field("stem", asset("png")),
                    field("col", FieldKind::Int),
                    field("row", FieldKind::Int),
                ],
            ),
            entry(
                "Music",
                vec![
                    field("stem", asset("ogg")),
                    field("once", FieldKind::Bool),
                    field("volume", FieldKind::Int),
                ],
            ),
            entry(
                "Sfx",
                vec![
                    field("stem", asset("wav")),
                    field("looping", FieldKind::Bool),
                ],
            ),
            entry(
                "Camera",
                vec![
                    field("is_active", FieldKind::Bool),
                    field("is_centered", FieldKind::Bool),
                ],
            ),
        ]
    }

    fn fields_of(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            other => panic!("not an object: {other}"),
        }
    }

    fn round_trip(component: &str, fields: Value) {
        let schemas = builtins();
        let fields = fields_of(fields);
        let bag = bag_from_fields(&schemas, component, &fields)
            .unwrap_or_else(|| panic!("{component} did not encode"));
        let (name, back) =
            fields_from_bag(&schemas, &bag).unwrap_or_else(|| panic!("{component} did not decode"));
        assert_eq!(name, component);
        assert_eq!(back, fields, "{component} round trip");
    }

    #[test]
    fn round_trips_every_builtin_shape() {
        round_trip("Transform", json!({"pos": [1.5, -2.0], "z": 3}));
        round_trip(
            "Sprite",
            json!({"stem": "hero", "centered": true, "offset": [0.0, -4.0], "visible": false}),
        );
        round_trip(
            "MetaSprite",
            json!({
                "stem": "hero", "centered": false, "offset": [2.25, 0.0],
                "visible": true, "clip": "walk", "loop": true
            }),
        );
        round_trip(
            "Text",
            json!({
                "font": "vt323", "content": "hello", "centered": true,
                "max_width": 120, "max_height": 0, "wrap": "word", "visible": true
            }),
        );
        round_trip(
            "Tilemap",
            json!({"layer": "bg0", "stem": "town", "col": 3, "row": -1}),
        );
        round_trip(
            "Music",
            json!({"stem": "theme", "once": false, "volume": 64}),
        );
        round_trip("Sfx", json!({"stem": "blip", "looping": true}));
        round_trip("Camera", json!({"is_active": true, "is_centered": false}));
    }

    #[test]
    fn transform_bag_is_byte_identical_to_the_field_writer() {
        let mut writer = FieldWriter::new();
        writer.vec2(
            "pos",
            Vec2::new(Fixed::from_raw(to_raw(1.5)), Fixed::from_int(-2)),
        );
        writer.int("z", 3);
        let expected = writer.finish("Transform");

        let fields = fields_of(json!({"pos": [1.5, -2], "z": 3}));
        assert_eq!(
            bag_from_fields(&builtins(), "Transform", &fields),
            Some(expected)
        );
    }

    #[test]
    fn a_component_no_schema_names_is_refused() {
        let mut writer = FieldWriter::new();
        writer.int("hp", 3);
        let bag = writer.finish("Health");
        assert_eq!(fields_from_bag(&builtins(), &bag), None);
        assert_eq!(
            bag_from_fields(&builtins(), "Health", &fields_of(json!({"hp": 3}))),
            None
        );
    }

    #[test]
    fn a_malformed_bag_is_refused() {
        assert_eq!(fields_from_bag(&builtins(), &[]), None);
        assert_eq!(fields_from_bag(&builtins(), &[0; 12]), None);
    }

    #[test]
    fn a_field_missing_from_the_bag_is_omitted() {
        let mut writer = FieldWriter::new();
        writer.int("z", 7);
        let bag = writer.finish("Transform");
        let (name, fields) = fields_from_bag(&builtins(), &bag).unwrap();
        assert_eq!(name, "Transform");
        assert_eq!(fields, fields_of(json!({"z": 7})));
    }

    #[test]
    fn a_field_missing_from_the_map_is_omitted() {
        let schemas = builtins();
        let bag = bag_from_fields(&schemas, "Transform", &fields_of(json!({"z": 7}))).unwrap();
        assert_eq!(
            bag,
            {
                let mut writer = FieldWriter::new();
                writer.int("z", 7);
                writer.finish("Transform")
            },
            "only the fields the map carries are written"
        );
        let (_, back) = fields_from_bag(&schemas, &bag).unwrap();
        assert_eq!(back, fields_of(json!({"z": 7})));
    }

    #[test]
    fn a_key_the_schema_does_not_name_is_ignored() {
        let schemas = builtins();
        let bag = bag_from_fields(
            &schemas,
            "Camera",
            &fields_of(json!({"is_active": true, "is_centered": false, "zoom": 2})),
        )
        .unwrap();
        let (_, back) = fields_from_bag(&schemas, &bag).unwrap();
        assert_eq!(
            back,
            fields_of(json!({"is_active": true, "is_centered": false}))
        );
    }

    #[test]
    fn a_wrongly_typed_value_refuses_the_whole_bag() {
        let schemas = builtins();
        let cases = [
            json!({"pos": [0.0, 0.0], "z": "three"}),
            json!({"pos": [0.0, 0.0], "z": 1.5}),
            json!({"pos": [0.0, 0.0], "z": 4294967296i64}),
            json!({"pos": 5, "z": 0}),
            json!({"pos": [0.0], "z": 0}),
            json!({"pos": [0.0, "y"], "z": 0}),
            json!({"pos": null, "z": 0}),
        ];
        for case in cases {
            assert_eq!(
                bag_from_fields(&schemas, "Transform", &fields_of(case.clone())),
                None,
                "{case} should not encode"
            );
        }
        assert_eq!(
            bag_from_fields(
                &schemas,
                "Sfx",
                &fields_of(json!({"stem": 3, "looping": true}))
            ),
            None
        );
        assert_eq!(
            bag_from_fields(
                &schemas,
                "Camera",
                &fields_of(json!({"is_active": "yes", "is_centered": false}))
            ),
            None
        );
    }

    #[test]
    fn a_value_whose_tag_does_not_fit_the_schema_is_omitted() {
        let mut writer = FieldWriter::new();
        writer.str_("z", "three");
        writer.vec2("pos", Vec2::int(1, 2));
        let bag = writer.finish("Transform");
        let (_, fields) = fields_from_bag(&builtins(), &bag).unwrap();
        assert_eq!(fields, fields_of(json!({"pos": [1.0, 2.0]})));
    }

    #[test]
    fn a_struct_field_makes_the_component_unconvertible() {
        let schemas = vec![entry(
            "Body",
            vec![
                field("shape", FieldKind::Struct),
                field("mass", FieldKind::Int),
            ],
        )];
        let mut writer = FieldWriter::new();
        writer.int("mass", 5);
        let bag = writer.finish("Body");
        assert_eq!(fields_from_bag(&schemas, &bag), None);
        assert_eq!(
            bag_from_fields(&schemas, "Body", &fields_of(json!({"mass": 5}))),
            None
        );
    }

    #[test]
    fn a_list_carries_ints_decimals_bools_and_strings() {
        let schemas = vec![entry("Path", vec![field("steps", FieldKind::List)])];
        let fields = fields_of(json!({"steps": [1, -2, 0.5, true, "end"]}));
        let bag = bag_from_fields(&schemas, "Path", &fields).unwrap();
        let (name, back) = fields_from_bag(&schemas, &bag).unwrap();
        assert_eq!(name, "Path");
        assert_eq!(back, fields);

        let int_only = fields_of(json!({"steps": [4, 5, 6]}));
        let bag = bag_from_fields(&schemas, "Path", &int_only).unwrap();
        assert_eq!(bag, {
            let mut writer = FieldWriter::new();
            writer.list_int("steps", &[4, 5, 6]);
            writer.finish("Path")
        });
    }

    /// A whole-valued float IS an `Int`: TOML round-trips a document's
    /// `z = 64` back as either tag depending on how it was authored, and a
    /// bag that encoded the two differently would re-send the component on
    /// every commit that touched neither.
    #[test]
    fn a_whole_valued_float_encodes_as_the_integer_it_is() {
        let schemas = vec![entry("Depth", vec![field("z", FieldKind::Int)])];
        let whole = bag_from_fields(&schemas, "Depth", &fields_of(json!({"z": 64.0})));
        let integer = bag_from_fields(&schemas, "Depth", &fields_of(json!({"z": 64})));
        assert_eq!(whole, integer);
        assert_eq!(whole, {
            let mut writer = FieldWriter::new();
            writer.int("z", 64);
            Some(writer.finish("Depth"))
        });
        assert_eq!(
            bag_from_fields(&schemas, "Depth", &fields_of(json!({"z": 64.5}))),
            None,
            "a fraction is not an integer, and a bag that dropped it would
             reset the field on the cart"
        );
    }

    #[test]
    fn a_list_integer_too_wide_for_the_tag_is_refused() {
        let schemas = vec![entry("Path", vec![field("steps", FieldKind::List)])];
        assert_eq!(
            bag_from_fields(
                &schemas,
                "Path",
                &fields_of(json!({"steps": [4294967296i64]}))
            ),
            None
        );
    }

    #[test]
    fn a_nested_list_is_refused() {
        let schemas = vec![entry("Path", vec![field("steps", FieldKind::List)])];
        assert_eq!(
            bag_from_fields(&schemas, "Path", &fields_of(json!({"steps": [[1, 2]]}))),
            None
        );

        let mut inner = FieldWriter::new();
        inner.list_int("steps", &[1, 2]);
        let nested = inner.finish("Path");
        let mut writer = FieldWriter::new();
        writer.field("steps", &vec![vec![1i32, 2]][..]);
        let bag = writer.finish("Path");
        assert_ne!(bag, nested);
        let (_, fields) = fields_from_bag(&schemas, &bag).unwrap();
        assert_eq!(fields, Map::new(), "an unconvertible field is omitted");
    }
}
