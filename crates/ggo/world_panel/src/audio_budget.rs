//! The toolbar's `audio N / 384 KiB` readout: every audio stem the open
//! world (and its resolved instances) names, sized against the APU sample
//! region.
//!
//! Why here and not in the audio tab: the region is reset PER WORLD by
//! emerald's runtime, so "does it fit" is a property of a world's whole
//! audio set, and the world panel is the only place that set is known. And
//! why at all: an upload past the region is a **silent skip** at runtime --
//! the editor is the one place that failure can be made visible before a
//! cart ships.
//!
//! Stems are collected on every render (the same walk the draw list
//! already makes); sizes are cached per stem in [`OpenWorld`] and filled
//! off-thread, because sizing a raw `.ogg` means decoding it. The cache
//! clears with the panel's world refresh (activation), so a re-imported
//! file is re-sized the next time the panel is shown.

use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;

use ggo_worldlib::schemas::{ComponentSchema, FieldKind};
use ggo_worldlib::world_doc::WorldState;
use ggo_worldlib::world_file::WorldEntity;

/// Region bytes per stem: `Some(bytes)` for a file that resolves,
/// `None` for one that does not (missing, undecodable).
pub type AudioSizes = HashMap<String, Option<u32>>;

/// The extensions a schema field must name to count as audio: the baked
/// form, and the two sources emerald bakes at pack.
const AUDIO_EXTS: [&str; 3] = ["adp", "wav", "ogg"];

/// Every distinct audio stem the world names, through resolved instance
/// subtrees too. Schema-driven: any component field whose kind is
/// `Asset("adp" | "wav" | "ogg")` (the builtin `Music`/`Sfx` `stem`
/// fields, or a manifest component's).
pub fn audio_stems(state: &WorldState, schemas: &[ComponentSchema]) -> Vec<String> {
    let audio_fields: Vec<(&str, &str)> = schemas
        .iter()
        .flat_map(|schema| {
            schema.fields.iter().filter_map(move |field| match &field.kind {
                FieldKind::Asset(ext) if AUDIO_EXTS.iter().any(|e| ext.eq_ignore_ascii_case(e)) => {
                    Some((schema.name.as_str(), field.name.as_str()))
                }
                _ => None,
            })
        })
        .collect();
    let mut stems = std::collections::BTreeSet::new();
    visit_entities(&state.entities, &audio_fields, &mut stems);
    for instance in &state.instances {
        if let Some(resolved) = &instance.resolved {
            visit_resolved(resolved, &audio_fields, &mut stems);
        }
    }
    stems.into_iter().collect()
}

fn visit_entities(
    entities: &[WorldEntity],
    audio_fields: &[(&str, &str)],
    stems: &mut std::collections::BTreeSet<String>,
) {
    for entity in entities {
        for (component, field) in audio_fields {
            if let Some(stem) = entity
                .components
                .get(*component)
                .and_then(|c| c.get(*field))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                stems.insert(stem.to_string());
            }
        }
    }
}

/// The resolved-subtree JSON (`loader::resolve_world_value`'s shape):
/// `{ entities: [{ components }], instances: [{ resolved | error }] }`.
fn visit_resolved(
    node: &Value,
    audio_fields: &[(&str, &str)],
    stems: &mut std::collections::BTreeSet<String>,
) {
    if let Some(entities) = node.get("entities").and_then(Value::as_array) {
        for entity in entities {
            let Some(components) = entity.get("components").and_then(Value::as_object) else {
                continue;
            };
            for (component, field) in audio_fields {
                if let Some(stem) = components
                    .get(*component)
                    .and_then(|c| c.get(*field))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    stems.insert(stem.to_string());
                }
            }
        }
    }
    if let Some(instances) = node.get("instances").and_then(Value::as_array) {
        for instance in instances {
            if let Some(resolved) = instance.get("resolved") {
                visit_resolved(resolved, audio_fields, stems);
            }
        }
    }
}

/// Region bytes `stem` will occupy: a `<stem>.adp` under `asset_root` is
/// its blocks; a `<stem>.wav` / `<stem>.ogg` is what emerald's default-rate
/// bake would produce. `None` when nothing resolves. Blocking (decodes),
/// so callers run it off-thread.
pub fn size_stem(asset_root: &Path, stem: &str) -> Option<u32> {
    let adp = asset_root.join(format!("{stem}.adp"));
    if let Ok(bytes) = std::fs::read(&adp) {
        return ggo_audio::adp_region_bytes(&bytes);
    }
    for ext in ["wav", "ogg"] {
        let path = asset_root.join(format!("{stem}.{ext}"));
        if path.is_file() {
            let decoded = ggo_audio::decode(&path).ok()?;
            let rate = ggo_audio::default_rate(&path);
            return Some(ggo_audio::baked_bytes(
                decoded.samples.len(),
                decoded.rate_hz,
                rate,
            ));
        }
    }
    None
}

/// What the toolbar shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AudioBudget {
    pub used: u32,
    /// Stems with no file behind them (counted as 0).
    pub missing: Vec<String>,
    /// Stems not sized yet.
    pub pending: usize,
}

impl AudioBudget {
    pub fn over(&self) -> bool {
        self.used > ggo_audio::SAMPLE_REGION_BYTES
    }

    pub fn label(&self) -> String {
        let kib = |bytes: u32| bytes.div_ceil(1024);
        let mut label = format!(
            "audio {} / {} KiB",
            kib(self.used),
            kib(ggo_audio::SAMPLE_REGION_BYTES)
        );
        if self.pending > 0 {
            label.push('…');
        }
        if !self.missing.is_empty() {
            label.push_str(&format!(" · {} missing", self.missing.len()));
        }
        label
    }
}

pub fn summarize(stems: &[String], sizes: &AudioSizes) -> AudioBudget {
    let mut budget = AudioBudget::default();
    for stem in stems {
        match sizes.get(stem) {
            Some(Some(bytes)) => budget.used = budget.used.saturating_add(*bytes),
            Some(None) => budget.missing.push(stem.clone()),
            None => budget.pending += 1,
        }
    }
    budget
}

#[cfg(test)]
mod tests {
    use super::*;
    use ggo_worldlib::schemas::builtin_schemas;
    use ggo_worldlib::world_doc::{WorldDocStore, WorldDocWire};
    use ggo_worldlib::world_file::{WorldFile, WorldInstance};
    use serde_json::json;

    fn entity(components: Value) -> WorldEntity {
        WorldEntity {
            components: components.as_object().cloned().unwrap_or_default(),
        }
    }

    fn state_with(entities: Vec<WorldEntity>) -> WorldState {
        WorldDocStore::new(WorldDocWire::from(WorldFile {
            entities,
            instances: vec![],
            backgrounds: vec![],
        }))
        .state()
    }

    #[test]
    fn stems_come_from_music_and_sfx_fields_deduped_and_sorted() {
        let state = state_with(vec![
            entity(json!({ "Sfx": { "stem": "sfx/jump" }, "Transform": { "pos": [0, 0] } })),
            entity(json!({ "Music": { "stem": "music/theme" } })),
            entity(json!({ "Sfx": { "stem": "sfx/jump" } })),
            entity(json!({ "Sfx": { "stem": "" } })),
            entity(json!({ "Text": { "content": "sfx/not-audio" } })),
        ]);
        assert_eq!(
            audio_stems(&state, &builtin_schemas()),
            vec!["music/theme".to_string(), "sfx/jump".to_string()]
        );
    }

    #[test]
    fn stems_inside_resolved_instances_are_collected_too() {
        let mut store = WorldDocStore::new(WorldDocWire::from(WorldFile {
            entities: vec![],
            instances: vec![WorldInstance {
                world: "worlds/arena".to_string(),
                pos: [0.0, 0.0],
                background_priority: false,
            }],
            backgrounds: vec![],
        }));
        let resolved = json!({
            "entities": [{ "components": { "Sfx": { "stem": "sfx/hit" } } }],
            "instances": [{
                "world": "worlds/deeper",
                "pos": [0, 0],
                "resolved": { "entities": [{ "components": { "Music": { "stem": "music/deep" } } }], "instances": [] }
            }]
        });
        store.set_instances_resolved("worlds/arena", &Ok(resolved), true);
        assert_eq!(
            audio_stems(&store.state(), &builtin_schemas()),
            vec!["music/deep".to_string(), "sfx/hit".to_string()]
        );
    }

    #[test]
    fn size_stem_reads_an_adp_or_bakes_a_source_and_reports_missing() {
        let dir = tempfile::tempdir().unwrap();
        let decoded = ggo_audio::Decoded {
            samples: vec![1000; 16_000],
            rate_hz: 16_000,
            source_channels: 1,
        };
        let blob = ggo_audio::bake(&decoded, 16_000);
        ggo_audio::write_adp(dir.path(), "sfx/jump.adp", &blob).unwrap();
        assert_eq!(size_stem(dir.path(), "sfx/jump"), Some((16_000 / 120 + 1) * 64));

        // A raw wav at 32 kHz bakes at the 16 kHz SFX rate: 8000 samples.
        let mut wav = Vec::new();
        let data: Vec<u8> = vec![0u8; 16_000 * 2];
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&((36 + data.len()) as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&32_000u32.to_le_bytes());
        wav.extend_from_slice(&64_000u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);
        std::fs::create_dir_all(dir.path().join("sfx")).unwrap();
        std::fs::write(dir.path().join("sfx/step.wav"), wav).unwrap();
        assert_eq!(size_stem(dir.path(), "sfx/step"), Some((8_000 / 120 + 1) * 64));

        assert_eq!(size_stem(dir.path(), "sfx/nope"), None);
    }

    #[test]
    fn the_summary_sums_counts_missing_and_flags_over_region() {
        let stems: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
        let mut sizes = AudioSizes::new();
        sizes.insert("a".into(), Some(100 * 1024));
        sizes.insert("b".into(), Some(200 * 1024));
        sizes.insert("c".into(), None);
        let budget = summarize(&stems, &sizes);
        assert_eq!(budget.used, 300 * 1024);
        assert_eq!(budget.missing, vec!["c".to_string()]);
        assert_eq!(budget.pending, 1);
        assert!(!budget.over());
        assert_eq!(budget.label(), "audio 300 / 384 KiB… · 1 missing");

        sizes.insert("d".into(), Some(100 * 1024));
        let budget = summarize(&stems, &sizes);
        assert!(budget.over());
        assert_eq!(budget.label(), "audio 400 / 384 KiB · 1 missing");
    }
}
