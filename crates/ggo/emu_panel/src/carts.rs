//! `.cart` enumeration under the project root -- the picker's feed.
//!
//! Deliberately a local walk rather than a worldlib call: worldlib's
//! `sprites::io` walkers all decode-validate a `.spr` before listing it,
//! and there is no cart-shaped equivalent there (cart parsing lives in
//! `ggo_emu_core::cart`, which worldlib doesn't depend on). The walk
//! *shape* is copied from `ggo_worldlib::sprites::io::walk_spr_files`
//! verbatim so the two pickers behave identically: dotfiles and the four
//! build/vendor directory names skipped, depth-capped, case-insensitive
//! extension, sorted forward-slash rel paths.
//!
//! Enumeration does NOT parse the cart header. A malformed `.cart` still
//! shows up in the picker and fails loudly at Run (`drive`'s `Ended`
//! message) rather than silently vanishing from the list -- the opposite
//! of the sprite rail's choice, and the right one here: a cart that just
//! failed to pack is exactly the file a user is trying to run.

use std::fs;
use std::path::Path;

/// Directory names never descended into. Same set as
/// `ggo_worldlib::sprites::io::SKIP_DIR_NAMES`.
const SKIP_DIR_NAMES: &[&str] = &["target", "node_modules", ".git", "dist"];

/// Directory nesting cap, mirroring worldlib's `MAX_SCAN_DEPTH`.
const MAX_SCAN_DEPTH: usize = 8;

/// The cart extension the picker accepts. The single-file `.ggo` variant
/// carries the same `GGOC` header plus an asset section, and
/// `ggo_emu_core::cart::Cart::parse` handles both -- but F3's brief scopes
/// the picker to `.cart`, so `.ggo` is deliberately not listed yet.
const CART_EXT: &str = "cart";

/// Whether `path` is something the picker should offer. Pure, so the
/// extension rule (case-insensitive, extension-only -- a bare file named
/// `cart` is not a cart) is testable without touching a filesystem.
pub fn is_cart_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(CART_EXT))
}

/// Whether the walk descends into / considers a directory entry named
/// `name`. Pure for the same reason as [`is_cart_file`].
pub fn is_skipped_name(name: &str) -> bool {
    name.starts_with('.') || SKIP_DIR_NAMES.contains(&name)
}

/// Every `.cart` under `root`, as sorted forward-slash relative paths.
/// Unreadable directories are skipped rather than reported -- a picker
/// that shows fewer carts is strictly better than one that shows an error
/// because some sibling directory was mode 000.
pub fn list_carts(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    walk(root, root, 0, &mut out);
    out.sort();
    out
}

fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) {
    if depth > MAX_SCAN_DEPTH {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if is_skipped_name(&name) {
            continue;
        }
        if path.is_dir() {
            walk(root, &path, depth + 1, out);
            continue;
        }
        if !is_cart_file(&path) {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        out.push(rel.to_string_lossy().replace('\\', "/"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn is_cart_file_is_extension_only_and_case_insensitive() {
        assert!(is_cart_file(&PathBuf::from("a/b/game.cart")));
        assert!(is_cart_file(&PathBuf::from("GAME.CART")));
        assert!(is_cart_file(&PathBuf::from("game.Cart")));
        // Extension, not a suffix match: these are not carts.
        assert!(!is_cart_file(&PathBuf::from("cart")));
        assert!(!is_cart_file(&PathBuf::from("game.cartridge")));
        assert!(!is_cart_file(&PathBuf::from("game.cart.bak")));
        // The single-file variant is deliberately out of scope for now.
        assert!(!is_cart_file(&PathBuf::from("game.ggo")));
    }

    #[test]
    fn skipped_names_cover_dotfiles_and_build_dirs() {
        for name in [".git", ".zed", "target", "node_modules", "dist"] {
            assert!(is_skipped_name(name), "{name} should be skipped");
        }
        for name in ["src", "carts", "assets", "cart"] {
            assert!(!is_skipped_name(name), "{name} should NOT be skipped");
        }
    }

    #[test]
    fn list_carts_walks_recursively_sorted_and_skips_build_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("carts/sub")).unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("zeta.cart"), b"x").unwrap();
        fs::write(root.join("carts/alpha.cart"), b"x").unwrap();
        fs::write(root.join("carts/sub/beta.CART"), b"x").unwrap();
        fs::write(root.join("carts/notes.txt"), b"x").unwrap();
        fs::write(root.join("target/debug/built.cart"), b"x").unwrap();
        fs::write(root.join(".git/hidden.cart"), b"x").unwrap();

        assert_eq!(
            list_carts(root),
            vec![
                "carts/alpha.cart".to_string(),
                "carts/sub/beta.CART".to_string(),
                "zeta.cart".to_string(),
            ]
        );
    }

    #[test]
    fn list_carts_on_a_missing_root_is_empty_not_a_panic() {
        assert!(list_carts(Path::new("/definitely/not/a/real/root")).is_empty());
    }

    #[test]
    fn list_carts_stops_at_the_depth_cap() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // MAX_SCAN_DEPTH + 2 levels: the deepest cart is out of reach.
        let mut deep = root.to_path_buf();
        for i in 0..=(MAX_SCAN_DEPTH + 1) {
            deep = deep.join(format!("d{i}"));
        }
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("too_deep.cart"), b"x").unwrap();
        fs::write(root.join("shallow.cart"), b"x").unwrap();

        assert_eq!(list_carts(root), vec!["shallow.cart".to_string()]);
    }
}
