//! "New Tileset…": authoring an empty `.til`/`.pal` pair.
//!
//! **Why this and not a route to `ggo_import_panel`.** The other half of
//! the choice the brief offered was to have the entry reveal the import
//! panel instead of writing anything. That panel cannot be entered without
//! a source: its only entry point is `open_source(rel)` for a `.png` the
//! user right-clicked, and it has no in-panel file picker (its empty state
//! literally reads "Right-click a .png in the project panel → Import as
//! tileset…"). A "New Tileset…" that routed there would open a panel whose
//! only content is an instruction to use a different menu entry.
//!
//! Writing the pair is worth doing on its own terms, because a `.til` is a
//! PREREQUISITE in this fork, not a leaf:
//! - `ggo_sprite_panel`'s New Sprite form can only be cancelled when the
//!   project has no `.til` to bind to;
//! - `ggo_map_panel` binds a `.map` to a tileset the same way.
//!
//! So in a project with no art yet, this is the file that unblocks
//! authoring at all -- and it is not a dead end, because importing a PNG
//! to the same stem later overwrites it in place (with the import panel's
//! own overwrite confirmation), so every `.spr`/`.map` already bound to
//! the name keeps its binding and gains real pixels.
//!
//! The one thing this module refuses to invent is the PALETTE. worldlib
//! already defines what a `.pal`-less tileset reads back as (16 evenly
//! spaced grays, `io.rs`'s private `grayscale_palette`), so the blank pair
//! is authored by writing the `.til` alone and then reading it back to
//! LEARN that palette before saving it -- one definition, worldlib's, and
//! `blank_tileset_adopts_worldlibs_pal_less_fallback` is the mechanical
//! check that they still agree.

use std::path::Path;

use ggo_worldlib::sprites::io;
use ggo_worldlib::sprites::tileset_doc::{TILE_PIXELS, pack_indices_to_til};

/// The extension a tileset sheet carries.
pub const TILESET_EXT: &str = "til";

/// How many tiles a blank tileset starts with. One: the sheet exists so a
/// sprite or map can name it, and every tile in it is transparent either
/// way, so more of them would only be more blank.
pub const BLANK_TILES: usize = 1;

/// The asset-root-relative `.til` rel a blank tileset is written to, or
/// `Err(message)` for a stem the panel must refuse.
///
/// Same rules as `ggo_sprite_panel::rename_target`'s stem half -- a `/`
/// or `\` would move the write out of the directory the user
/// right-clicked, and `.`/`..` are not names -- plus the same courtesy of
/// accepting a retyped extension without doubling it. Deliberately NOT
/// `valid_item_name`: this is a FILENAME, not an `emd` identifier, and
/// nothing downstream requires an asset file to be snake_case.
pub fn tileset_rel(dir_rel: &str, text: &str) -> Result<String, String> {
    let typed = text.trim();
    let stem = typed
        .strip_suffix(&format!(".{TILESET_EXT}"))
        .unwrap_or(typed)
        .trim();
    if stem.is_empty() {
        return Err("name cannot be empty".to_string());
    }
    if stem.contains('/') || stem.contains('\\') {
        return Err("name cannot contain a path separator".to_string());
    }
    if stem == "." || stem == ".." {
        return Err(format!("{stem} is not a name"));
    }
    let file = format!("{stem}.{TILESET_EXT}");
    Ok(if dir_rel.is_empty() {
        file
    } else {
        format!("{}/{file}", dir_rel.trim_end_matches('/'))
    })
}

/// Write a blank `.til` + `.pal` pair at asset-root-relative `til_rel`
/// under `asset_root`. Refuses to overwrite an existing sheet.
///
/// `til_rel` is asset-root-relative, which is the frame every downstream
/// binder stores (`.spr`'s `til_path`, `.map`'s `til_path` -- the F4
/// `ggo-sprfix` contract), and the same frame `io::save_tileset` expects.
pub fn create_blank_tileset(asset_root: &Path, til_rel: &str) -> Result<(), String> {
    let til_abs = asset_root.join(til_rel);
    if til_abs.exists() {
        return Err(format!("{til_rel} already exists"));
    }
    if let Some(parent) = til_abs.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // Step 1: the `.til` alone. `io::save_tileset` would write a `.pal`
    // too, and there is no palette to write yet -- see the module doc.
    let indices = vec![0u8; BLANK_TILES * TILE_PIXELS];
    std::fs::write(&til_abs, pack_indices_to_til(&indices, BLANK_TILES))
        .map_err(|e| e.to_string())?;
    // Step 2: read it back to learn worldlib's own `.pal`-less palette...
    let read = io::open_tileset(asset_root, til_rel).map_err(|e| e.to_string())?;
    // ...and step 3: persist it, so the pair is complete and the tileset
    // panel doesn't have to show its "no .pal found" note for a sheet this
    // fork just authored.
    io::save_tileset(
        asset_root,
        til_rel,
        &read.indices,
        read.tile_count,
        &read.palette,
    )
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ggo_worldlib::sprites::palette565::PAL_SLOTS;

    #[test]
    fn tileset_rel_keeps_the_write_in_the_clicked_directory() {
        assert_eq!(tileset_rel("tiles", "world").unwrap(), "tiles/world.til");
        assert_eq!(
            tileset_rel("tiles", "world.til").unwrap(),
            "tiles/world.til",
            "a retyped extension is not doubled"
        );
        assert_eq!(tileset_rel("", "world").unwrap(), "world.til");
        assert_eq!(tileset_rel("tiles/", " world ").unwrap(), "tiles/world.til");

        for bad in ["", "   ", "a/b", "a\\b", ".", "..", ".til"] {
            assert!(
                tileset_rel("tiles", bad).is_err(),
                "{bad:?} must be refused"
            );
        }
    }

    /// The whole point of the read-back: the `.pal` this writes must be
    /// EXACTLY what worldlib hands back for a `.pal`-less `.til`, with no
    /// second copy of the 16-gray ramp living in this crate.
    #[test]
    fn blank_tileset_adopts_worldlibs_pal_less_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        create_blank_tileset(root, "tiles/world.til").unwrap();

        // What worldlib says a `.pal`-less sheet's palette is.
        std::fs::create_dir_all(root.join("ref")).unwrap();
        std::fs::write(
            root.join("ref/bare.til"),
            pack_indices_to_til(&vec![0u8; BLANK_TILES * TILE_PIXELS], BLANK_TILES),
        )
        .unwrap();
        let bare = io::open_tileset(root, "ref/bare.til").unwrap();
        assert!(bare.missing_pal, "the reference sheet has no .pal");

        let written = io::open_tileset(root, "tiles/world.til").unwrap();
        assert!(!written.missing_pal, "the pair must be complete");
        assert_eq!(written.palette, bare.palette);
        assert_eq!(written.palette.len(), PAL_SLOTS);
        assert_eq!(written.tile_count, BLANK_TILES);
        assert!(written.indices.iter().all(|i| *i == 0), "every tile blank");
        assert!(root.join("tiles/world.pal").is_file());
    }

    #[test]
    fn create_refuses_to_overwrite_an_existing_sheet() {
        let dir = tempfile::tempdir().unwrap();
        create_blank_tileset(dir.path(), "tiles/world.til").unwrap();
        let err = create_blank_tileset(dir.path(), "tiles/world.til").unwrap_err();
        assert!(err.contains("already exists"), "{err}");
    }
}
