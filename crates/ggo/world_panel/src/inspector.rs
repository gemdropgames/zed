//! Pure inspector logic: field targets, display text, and text ->
//! `WorldOp` commits. Ported from ggo-ide's `pages/world/inspector.rs`
//! (`display_value`/`commit_field`), with one deliberate reshape: vec2
//! fields edit as TWO single-axis inputs (this task's brief) instead of
//! ggo-ide's single `"x, y"` text box, so a commit parses one `f64` and
//! reads the OTHER axis from the live doc. Everything here is
//! framework-free and directly unit-testable; the panel wires each
//! [`FieldTarget`] to a gpui `Editor` and calls [`commit_field`] on
//! Enter/blur (matching ggo-ide's Enter-or-cross-field-commit rule; an
//! unparsable buffer is dropped, not committed).

use ggo_worldlib::render::Selection;
use ggo_worldlib::schemas::{ComponentSchema, FieldKind};
use ggo_worldlib::world_doc::{WorldOp, WorldState};
use serde_json::Value;

/// What one inspector text input edits.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FieldTarget {
    /// A non-vec2 component field (int/fixed/str/asset/unknown kinds).
    EntityField {
        entity: usize,
        component: String,
        field: String,
    },
    /// One axis (0 = x, 1 = y) of a vec2 component field.
    EntityVec2Axis {
        entity: usize,
        component: String,
        field: String,
        axis: usize,
    },
    /// One axis of an `[[instance]]`'s `pos` -- commits as
    /// `MoveInstance { gesture: None }`, same op ggo-ide's instance pos
    /// field uses.
    InstancePosAxis { index: usize, axis: usize },
}

/// One text input the inspector should render, in order.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldSpec {
    pub target: FieldTarget,
    pub label: String,
}

pub fn field_kind<'a>(
    schemas: &'a [ComponentSchema],
    component: &str,
    field: &str,
) -> Option<&'a FieldKind> {
    schemas
        .iter()
        .find(|s| s.name == component)
        .and_then(|s| s.fields.iter().find(|f| f.name == field))
        .map(|f| &f.kind)
}

/// The selected entity's `Transform.pos`, if it has a well-formed one --
/// ggo-ide's `entity_pos` (drag-start anchor).
pub fn entity_pos(state: &WorldState, index: usize) -> Option<[f64; 2]> {
    let e = state.entities.get(index)?;
    let t = e.components.get("Transform")?.as_object()?;
    let pos = t.get("pos")?.as_array()?;
    if pos.len() != 2 {
        return None;
    }
    Some([pos[0].as_f64()?, pos[1].as_f64()?])
}

/// A vec2-kinded field's value, with ggo-ide `display_value`'s `[0, 0]`
/// fallback for a missing/malformed one.
fn vec2_of(value: Option<&Value>) -> [f64; 2] {
    value
        .and_then(Value::as_array)
        .and_then(|a| Some([a.first()?.as_f64()?, a.get(1)?.as_f64()?]))
        .unwrap_or([0.0, 0.0])
}

fn entity_field_value<'a>(
    state: &'a WorldState,
    entity: usize,
    component: &str,
    field: &str,
) -> Option<&'a Value> {
    state
        .entities
        .get(entity)?
        .components
        .get(component)?
        .as_object()?
        .get(field)
}

/// The display string an unfocused input shows for `target` -- ggo-ide's
/// `display_value`, split per target shape.
pub fn display_text(
    target: &FieldTarget,
    state: &WorldState,
    schemas: &[ComponentSchema],
) -> String {
    match target {
        FieldTarget::EntityField {
            entity,
            component,
            field,
        } => {
            let value = entity_field_value(state, *entity, component, field);
            match field_kind(schemas, component, field) {
                // Color565 is stored and displayed as the raw packed
                // integer; the panel layers the picker UI on top.
                Some(FieldKind::Int) | Some(FieldKind::Fixed) | Some(FieldKind::Color565) => value
                    .and_then(Value::as_f64)
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "0".to_string()),
                Some(FieldKind::Str) | Some(FieldKind::Asset(_)) => {
                    value.and_then(Value::as_str).unwrap_or("").to_string()
                }
                // Bool never renders as text (checkbox); Vec2 never
                // reaches this variant (axis targets).
                Some(FieldKind::Bool) | Some(FieldKind::Vec2) => String::new(),
                Some(FieldKind::Unknown(_)) | None => {
                    value.map(|v| v.to_string()).unwrap_or_default()
                }
            }
        }
        FieldTarget::EntityVec2Axis {
            entity,
            component,
            field,
            axis,
        } => vec2_of(entity_field_value(state, *entity, component, field))[*axis].to_string(),
        FieldTarget::InstancePosAxis { index, axis } => state
            .instances
            .get(*index)
            .map(|i| i.pos[*axis].to_string())
            .unwrap_or_else(|| "0".to_string()),
    }
}

/// A completed edit's buffer -> the [`WorldOp`] to apply, or `None` if the
/// buffer doesn't parse (dropped, not committed -- ggo-ide's
/// `commit_field` rule). Per-kind parsing is ggo-ide's, verbatim: Int ->
/// `i64`, Fixed -> `f64`, Str/Asset -> the raw buffer verbatim (never a
/// JSON parse -- `"42"` stays the STRING `"42"`), Unknown/no-schema ->
/// JSON parse with string fallback, Bool -> never commits through here.
pub fn commit_field(
    target: &FieldTarget,
    text: &str,
    state: &WorldState,
    schemas: &[ComponentSchema],
) -> Option<WorldOp> {
    match target {
        FieldTarget::EntityField {
            entity,
            component,
            field,
        } => {
            let value = match field_kind(schemas, component, field) {
                Some(FieldKind::Int) | Some(FieldKind::Color565) => {
                    Value::from(text.trim().parse::<i64>().ok()?)
                }
                Some(FieldKind::Fixed) => Value::from(text.trim().parse::<f64>().ok()?),
                Some(FieldKind::Str) | Some(FieldKind::Asset(_)) => Value::String(text.to_string()),
                Some(FieldKind::Bool) | Some(FieldKind::Vec2) => return None,
                Some(FieldKind::Unknown(_)) | None => {
                    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()))
                }
            };
            Some(WorldOp::SetField {
                entity: *entity,
                component: component.clone(),
                field: field.clone(),
                value,
            })
        }
        FieldTarget::EntityVec2Axis {
            entity,
            component,
            field,
            axis,
        } => {
            let parsed: f64 = text.trim().parse().ok()?;
            let mut pos = vec2_of(entity_field_value(state, *entity, component, field));
            pos[*axis] = parsed;
            Some(WorldOp::SetField {
                entity: *entity,
                component: component.clone(),
                field: field.clone(),
                value: serde_json::json!([pos[0], pos[1]]),
            })
        }
        FieldTarget::InstancePosAxis { index, axis } => {
            let parsed: f64 = text.trim().parse().ok()?;
            let mut pos = state.instances.get(*index)?.pos;
            pos[*axis] = parsed;
            Some(WorldOp::MoveInstance {
                index: *index,
                pos,
                gesture: None,
            })
        }
    }
}

/// The ordered text inputs for the current selection. Bool fields are
/// skipped (rendered as checkboxes, which apply their op directly);
/// vec2-kinded fields contribute an x and a y input; non-object component
/// values contribute nothing (shown read-only). Iteration order is the
/// stored field order, same as ggo-ide's inspector.
pub fn selection_field_specs(
    selection: Option<Selection>,
    state: &WorldState,
    schemas: &[ComponentSchema],
) -> Vec<FieldSpec> {
    let mut out = Vec::new();
    match selection {
        Some(Selection::Entity(entity)) => {
            let Some(e) = state.entities.get(entity) else {
                return out;
            };
            for (component, value) in &e.components {
                let Some(fields) = value.as_object() else {
                    continue;
                };
                for field in fields.keys() {
                    match field_kind(schemas, component, field) {
                        Some(FieldKind::Bool) => {}
                        Some(FieldKind::Vec2) => {
                            for (axis, axis_name) in ["x", "y"].iter().enumerate() {
                                out.push(FieldSpec {
                                    target: FieldTarget::EntityVec2Axis {
                                        entity,
                                        component: component.clone(),
                                        field: field.clone(),
                                        axis,
                                    },
                                    label: format!("{field}.{axis_name}"),
                                });
                            }
                        }
                        _ => out.push(FieldSpec {
                            target: FieldTarget::EntityField {
                                entity,
                                component: component.clone(),
                                field: field.clone(),
                            },
                            label: field.clone(),
                        }),
                    }
                }
            }
        }
        Some(Selection::Instance(index)) => {
            if state.instances.get(index).is_some() {
                for (axis, axis_name) in ["x", "y"].iter().enumerate() {
                    out.push(FieldSpec {
                        target: FieldTarget::InstancePosAxis { index, axis },
                        label: format!("pos.{axis_name}"),
                    });
                }
            }
        }
        None => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ggo_worldlib::schemas::builtin_schemas;
    use ggo_worldlib::world_doc::{WorldDocStore, WorldDocWire, WorldEntity};
    use ggo_worldlib::world_file::WorldFile;
    use serde_json::json;

    fn state_with(components: serde_json::Value) -> WorldState {
        let file = WorldFile {
            entities: vec![WorldEntity {
                components: components.as_object().unwrap().clone(),
            }],
            instances: vec![],
            backgrounds: vec![],
        };
        WorldDocStore::new(WorldDocWire::from(file)).state()
    }

    fn instance_state(pos: [f64; 2]) -> WorldState {
        let file = WorldFile {
            entities: vec![],
            instances: vec![ggo_worldlib::world_file::WorldInstance {
                world: "worlds/sub".to_string(),
                pos,
                background_priority: false,
            }],
            backgrounds: vec![],
        };
        WorldDocStore::new(WorldDocWire::from(file)).state()
    }

    fn transform_state() -> WorldState {
        state_with(json!({"Transform": {"pos": [4.0, 5.0], "z": 2.0}}))
    }

    // ------------------------------------------------------------ display

    #[test]
    fn display_text_int_and_vec2_axes_and_instance_pos() {
        let schemas = builtin_schemas();
        let state = transform_state();
        let z = FieldTarget::EntityField {
            entity: 0,
            component: "Transform".to_string(),
            field: "z".to_string(),
        };
        assert_eq!(display_text(&z, &state, &schemas), "2");
        let x = FieldTarget::EntityVec2Axis {
            entity: 0,
            component: "Transform".to_string(),
            field: "pos".to_string(),
            axis: 0,
        };
        let y = FieldTarget::EntityVec2Axis {
            entity: 0,
            component: "Transform".to_string(),
            field: "pos".to_string(),
            axis: 1,
        };
        assert_eq!(display_text(&x, &state, &schemas), "4");
        assert_eq!(display_text(&y, &state, &schemas), "5");

        let ipos = FieldTarget::InstancePosAxis { index: 0, axis: 1 };
        assert_eq!(display_text(&ipos, &instance_state([7.0, 8.5]), &[]), "8.5");
    }

    #[test]
    fn display_text_str_field_shows_raw_string_missing_int_shows_zero() {
        let schemas = builtin_schemas();
        let state = state_with(json!({"Text": {"content": "hi"}}));
        let content = FieldTarget::EntityField {
            entity: 0,
            component: "Text".to_string(),
            field: "content".to_string(),
        };
        assert_eq!(display_text(&content, &state, &schemas), "hi");
        let missing = FieldTarget::EntityField {
            entity: 0,
            component: "Text".to_string(),
            field: "max_width".to_string(),
        };
        assert_eq!(display_text(&missing, &state, &schemas), "0");
    }

    // ------------------------------------------------------------- commit

    fn ef(component: &str, field: &str) -> FieldTarget {
        FieldTarget::EntityField {
            entity: 0,
            component: component.to_string(),
            field: field.to_string(),
        }
    }

    #[test]
    fn commit_int_field_parses_as_json_integer_unparsable_dropped() {
        let schemas = builtin_schemas();
        let state = transform_state();
        let op = commit_field(&ef("Transform", "z"), "42", &state, &schemas).unwrap();
        assert_eq!(
            op,
            WorldOp::SetField {
                entity: 0,
                component: "Transform".to_string(),
                field: "z".to_string(),
                value: json!(42)
            }
        );
        assert_eq!(
            commit_field(&ef("Transform", "z"), "abc", &state, &schemas),
            None
        );
        // Int kind rejects a float buffer (i64 parse), same as ggo-ide.
        assert_eq!(
            commit_field(&ef("Transform", "z"), "1.5", &state, &schemas),
            None
        );
    }

    #[test]
    fn color565_field_displays_and_commits_as_a_raw_integer() {
        let schemas = builtin_schemas();
        let state = state_with(json!({"RectFill": {"w": 8.0, "h": 8.0, "color": 63488.0}}));
        let color = ef("RectFill", "color");
        assert_eq!(display_text(&color, &state, &schemas), "63488");
        let op = commit_field(&color, "2016", &state, &schemas).unwrap();
        assert!(matches!(op, WorldOp::SetField { ref value, .. } if *value == json!(2016)));
        assert_eq!(commit_field(&color, "red", &state, &schemas), None);
    }

    #[test]
    fn commit_str_field_commits_raw_buffer_verbatim_no_json_parse() {
        let schemas = builtin_schemas();
        let state = state_with(json!({"Text": {"content": "hi"}}));
        let op = commit_field(&ef("Text", "content"), "42", &state, &schemas).unwrap();
        assert_eq!(
            op,
            WorldOp::SetField {
                entity: 0,
                component: "Text".to_string(),
                field: "content".to_string(),
                value: json!("42")
            }
        );
    }

    #[test]
    fn commit_unknown_component_tries_json_parse_falls_back_to_string() {
        let state = state_with(json!({"Mystery": {"note": "x"}}));
        let op = commit_field(&ef("Mystery", "note"), "42", &state, &[]).unwrap();
        assert!(matches!(op, WorldOp::SetField { ref value, .. } if *value == json!(42)));
        let op = commit_field(&ef("Mystery", "note"), "not json", &state, &[]).unwrap();
        assert!(matches!(op, WorldOp::SetField { ref value, .. } if *value == json!("not json")));
    }

    #[test]
    fn commit_bool_kind_never_commits_through_text_path() {
        let schemas = builtin_schemas();
        let state = state_with(json!({"Camera": {"is_active": true}}));
        assert_eq!(
            commit_field(&ef("Camera", "is_active"), "true", &state, &schemas),
            None
        );
    }

    #[test]
    fn commit_vec2_axis_replaces_one_axis_and_keeps_the_other_from_the_doc() {
        let schemas = builtin_schemas();
        let state = transform_state();
        let y = FieldTarget::EntityVec2Axis {
            entity: 0,
            component: "Transform".to_string(),
            field: "pos".to_string(),
            axis: 1,
        };
        let op = commit_field(&y, "9.5", &state, &schemas).unwrap();
        assert_eq!(
            op,
            WorldOp::SetField {
                entity: 0,
                component: "Transform".to_string(),
                field: "pos".to_string(),
                value: json!([4.0, 9.5])
            }
        );
        assert_eq!(commit_field(&y, "", &state, &schemas), None);
    }

    #[test]
    fn commit_instance_pos_axis_moves_with_no_gesture() {
        let state = instance_state([7.0, 8.0]);
        let x = FieldTarget::InstancePosAxis { index: 0, axis: 0 };
        let op = commit_field(&x, "3", &state, &[]).unwrap();
        assert_eq!(
            op,
            WorldOp::MoveInstance {
                index: 0,
                pos: [3.0, 8.0],
                gesture: None
            }
        );
        let oob = FieldTarget::InstancePosAxis { index: 9, axis: 0 };
        assert_eq!(commit_field(&oob, "3", &state, &[]), None);
    }

    // -------------------------------------------------------------- specs

    #[test]
    fn selection_field_specs_skips_bools_splits_vec2_into_axes() {
        let schemas = builtin_schemas();
        let state = state_with(json!({
            "Transform": {"pos": [0.0, 0.0], "z": 0.0},
            "Camera": {"is_active": true}
        }));
        let specs = selection_field_specs(Some(Selection::Entity(0)), &state, &schemas);
        let labels: Vec<&str> = specs.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, ["pos.x", "pos.y", "z"]); // Camera bools: no inputs
    }

    #[test]
    fn selection_field_specs_instance_and_none_and_oob() {
        let state = instance_state([1.0, 2.0]);
        let specs = selection_field_specs(Some(Selection::Instance(0)), &state, &[]);
        let labels: Vec<&str> = specs.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, ["pos.x", "pos.y"]);
        assert!(selection_field_specs(None, &state, &[]).is_empty());
        assert!(selection_field_specs(Some(Selection::Instance(4)), &state, &[]).is_empty());
        assert!(selection_field_specs(Some(Selection::Entity(0)), &state, &[]).is_empty());
    }

    #[test]
    fn entity_pos_reads_transform_none_when_absent_or_malformed() {
        assert_eq!(entity_pos(&transform_state(), 0), Some([4.0, 5.0]));
        let no_transform = state_with(json!({"Camera": {"is_active": true}}));
        assert_eq!(entity_pos(&no_transform, 0), None);
        let bad = state_with(json!({"Transform": {"pos": [1.0]}}));
        assert_eq!(entity_pos(&bad, 0), None);
        assert_eq!(entity_pos(&transform_state(), 5), None);
    }
}
