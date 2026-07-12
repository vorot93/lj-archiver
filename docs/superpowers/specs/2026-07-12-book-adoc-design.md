# Design: `--book` — assemble the archive into a single `book.adoc`

- **Date:** 2026-07-12
- **Status:** Approved, ready for implementation planning
- **Scope:** One CLI flag + one GUI checkbox + a new `Archive::write_book` writer. No changes to discovery, fetching, parsing, media, or the AsciiDoc renderer.

## Goal

Add an opt-in option that assembles a full archive into a single `book.adoc`
document at the archive root. The book uses AsciiDoc `include::` directives so it
references the existing per-post files rather than duplicating their content, and
demotes each included document's title with `leveloffset=+1` so the standalone
posts become chapters of one book.

## Behavior

- **Opt-in, off by default.** Enabled by `--book` (CLI) or the "Generate
  book.adoc" checkbox (GUI).
- **Applies to both archive kinds.** Single-post and journal archives both
  already write `index.adoc`; when the option is on, both also write `book.adoc`.
- **Regenerated on every run** when enabled, including under `--resume`.
  `book.adoc` only references files by path, so regenerating it is cheap and keeps
  it in sync with whatever `posts/` currently contains.
- **Mirrors `index.adoc` exactly.** `book.adoc` includes the same posts
  `index.adoc` lists — the same sorted (oldest-first) `&[ArchivedPost]` slice.
  Failed posts are excluded (they are not in the slice); `--resume`-skipped posts
  are included (their files exist on disk).
- **Written atomically** via the existing `write_atomic` helper, like every other
  archive file.

## Generated `book.adoc` format

For journal `news` with archived posts `166511` and `166512`:

```adoc
= news Archive
:doctype: book
:toc:

include::index.adoc[leveloffset=+1]

include::posts/166511.adoc[leveloffset=+1]

include::posts/166512.adoc[leveloffset=+1]
```

Exact construction:

- Header: `= <journal> Archive\n:doctype: book\n:toc:\n\n`. The title reuses the
  existing `index.adoc` title string, so the included index renders as a
  `== <journal> Archive` chapter (a minor, predictable heading echo).
- `include::index.adoc[leveloffset=+1]\n`.
- For each post, in `index.adoc` order:
  `\ninclude::posts/<id>.adoc[leveloffset=+1]\n`.
- The file ends with exactly one `\n`, matching the project's trailing-newline
  convention.

**Blank line between every include (refinement over the first sketch).** Each
`include::` is preceded by a blank line, not only the first. AsciiDoc needs a
block boundary before each included document's now-`==` chapter title; without a
blank line a heading can be absorbed into the preceding post's trailing
paragraph. This is the standard AsciiDoctor include idiom.

**`:doctype: book` + `:toc:`.** `leveloffset=+1` turns each standalone post's
level-0 `= Title` into a level-1 `== Title`, which under `book` doctype is a
chapter. `:toc:` generates in-document navigation to those chapters.

## Implementation

### Core crate (`lj-archiver`)

- **`src/models.rs`** — `RunOptions` gains `pub generate_book: bool`.
- **`src/cli.rs`** — add `#[arg(long)] pub book: bool`; `Cli::options()` sets
  `generate_book: self.book`.
- **`src/archive.rs`** — new
  `pub fn write_book(&self, posts: &[ArchivedPost]) -> Result<PathBuf>`, a sibling
  of `write_index`:
  - `fs::create_dir_all(&self.root)` (same error mapping as `write_index`).
  - Build the string described above, reusing the existing private
    `index_path()` to relativize each `post.path` to `posts/<id>.adoc`. The index
    include is the literal `index.adoc`.
  - `write_atomic` to `self.root.join("book.adoc")`; return the path.
- **`src/run.rs`** — after each `archive.write_index(...)` call, gated on
  `options.generate_book`, call `archive.write_book(...)`:
  - `archive_post`: bind `let posts = [post];` and pass `&posts` to both
    `write_index` and `write_book` (the single post currently moves straight into
    `write_index(&[post])`).
  - `archive_journal`: pass the already-sorted `&posts` to both.
  - The two test `RunOptions` literals in `run.rs` (~lines 595, 653) gain
    `generate_book: false`.

### GUI crate (`lj-archiver-gui`)

- **`gui/src/state.rs`**
  - `RunConfig` gains `generate_book: bool`; `Default` sets it to `false`.
  - `Message::ToggleBook(bool)`.
  - Reducer: `Message::ToggleBook(on) => self.config.generate_book = *on`.
  - `run_request()` sets `generate_book: self.config.generate_book` in the built
    `RunOptions`.
- **`gui/src/app.rs`** — add
  `checkbox(self.config.generate_book).label("Generate book.adoc").on_toggle(Message::ToggleBook)`
  to the `view` form, beside the existing comments/media checkboxes.

## Edge cases and AsciiDoc semantics (accepted)

- **Empty archive** (0 archived posts): `book.adoc` is still written — header plus
  the index include, no post includes. Valid AsciiDoc.
- **Index xrefs inside the book:** the included index's `xref:posts/<id>.adoc[…]`
  links resolve to the *sibling* `posts/*.html` output files, not to the embedded
  chapters. That is inherent to keeping the index, and is why `:toc:` is added for
  in-document navigation. Path-based `xref:file.adoc[…]` links do not require the
  target to exist at conversion time, so they do not raise a converter warning.
- **Per-post `:lj-*:` attribute entries** in included files become harmless
  body-level attribute assignments (overwritten per chapter), not part of the book
  header; they produce no visible output.
- **Path separators** follow `write_index`'s existing `to_string_lossy()`
  behavior. This is pre-existing and out of scope here.

## Testing

- **`src/archive.rs` unit tests:**
  - `write_book` emits the exact byte string for a two-post slice — asserting the
    header, the blank line before each include, and `leveloffset=+1` on each.
  - The empty-slice case (header + index include only, single trailing newline).
- **`tests/cli_smoke.rs`:**
  - With `--book`, `book.adoc` exists and contains the `index.adoc` include and a
    per-post include.
  - Without `--book`, `book.adoc` is absent.
- **`gui/src/state.rs` unit tests:**
  - Default `generate_book == false`.
  - `ToggleBook(true)` sets it.
  - `run_request()` carries `generate_book` into `RunOptions`.

The AsciiDoc renderer (`src/adoc/`) and its byte-exact suite are untouched: post
files are unchanged, and `write_book` only emits a fixed header plus include
directives. Final book *rendering* is not exercised by the Rust suite (the project
generates AsciiDoc, it does not convert it); verify the assembled book with
`asciidoctor` by hand if available.

## Documentation updates (same change)

- **`README.md`:** a `--book` row in the Options table; `book.adoc` in the Archive
  Layout tree; a short note under Behavior; the GUI paragraph mentions the
  checkbox.
- **`AGENTS.md`:** note `write_book` on the `src/archive.rs` module-map line and
  the `RunOptions.generate_book` field in the reuse seam.

## Rationale

- **`write_book` lives in `archive.rs`, not `run.rs`.** That module owns archive
  paths, atomic writes, and root-relativization; `write_book` is a direct sibling
  of `write_index` reusing the same helpers. Generating the string in `run.rs`
  would duplicate path logic and cross the established module boundary.
- **Keep the index include** (rather than replacing it with only `:toc:`) per the
  chosen structure: the manual index table is preserved and `:toc:` adds
  in-document navigation on top.
- **Regenerate rather than skip under `--resume`:** `book.adoc` is a cheap,
  path-only manifest; always rewriting it avoids a stale book after a resumed run
  adds posts.

## Non-goals

- No configurable book filename or output path (fixed `book.adoc` at archive root).
- No change to `index.adoc` content or to any per-post file.
- No AsciiDoc-to-HTML/PDF conversion in the tool.
- No rewriting of the index's `xref:` links into intra-book anchors.
