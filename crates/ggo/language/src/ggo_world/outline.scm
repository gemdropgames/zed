; Array-of-table headers -- `[[entity]]`, `[[instance]]`, `[[background]]`,
; the three block kinds a GGO world file is made of (see ggo-worldlib's
; `world_file.rs`). The `table_array_element` node spans the header AND every
; pair under it, so the outline row selects the whole block.
;
; The key is matched by node type rather than `(_)`, because an unrestricted
; wildcard would also match the `pair` children that follow the header.
(table_array_element
  "[[" @context
  [
    (bare_key)
    (dotted_key)
    (quoted_key)
  ] @name
  "]]" @context) @item

; Ordinary `[table]` headers (e.g. `[layers]`), same shape.
(table
  "[" @context
  [
    (bare_key)
    (dotted_key)
    (quoted_key)
  ] @name
  "]" @context) @item

; Keys nest one level under the header that contains them: for a `[[entity]]`
; block those are its component names (`Transform`, `Enemy`), which is the
; thing worth navigating to.
;
; Each parent is spelled out instead of a bare `(pair ...)` pattern for a
; reason worth stating: inline-table members ARE `pair` nodes in this grammar,
; so an unparented pattern also emits a row per component FIELD -- `Transform =
; { pos = [3, 4], z = 2 }` would contribute `Transform`, `pos` and `z` rather
; than just `Transform`, tripling the outline of a real world file. Requiring
; the parent to be a header (or the document root, for keys written before any
; header) excludes exactly the inline-table case. A test pins this.
(document
  (pair
    [
      (bare_key)
      (dotted_key)
      (quoted_key)
    ] @name) @item)

(table
  (pair
    [
      (bare_key)
      (dotted_key)
      (quoted_key)
    ] @name) @item)

(table_array_element
  (pair
    [
      (bare_key)
      (dotted_key)
      (quoted_key)
    ] @name) @item)
