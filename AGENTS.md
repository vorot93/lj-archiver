# Contributor and Agent Guide

This file is for human contributors and AI coding sessions working in this repository.

`README.md` is for end users. Keep contributor workflow, implementation notes, and test strategy in this file or in dedicated docs, not in the README.

## Project Overview

`lj-archiver` is a Rust CLI that archives public LiveJournal journals and posts into local AsciiDoc files.

The current tool is public-content only. Do not add login, private-content access, publishing, syncing, or editing behavior without a separate design.

## Module Map

- `src/main.rs` is the thin binary entry point.
- `src/lib.rs` exposes internal modules and the crate `Result` type.
- `src/error.rs` defines typed errors and the shared `Result` type.
- `src/cli.rs` defines the public command-line interface with `clap`.
- `src/html_scan.rs` is the shared forward-only HTML scanner: it aligns html5ever-decoded start tags and attributes with immutable source byte ranges and precomputes source element spans.
- `src/models.rs` defines core archive, post, comment, media, and run option data types. `RunOptions::generate_book` is the opt-in single-file `book.adoc` toggle, set by the `--book` flag (and the GUI checkbox).
- `src/target.rs` parses target strings (bare journal names, `*.livejournal.com` hostnames, journal home URLs, and post URLs) and builds LiveJournal post and calendar URLs.
- `src/client.rs` owns HTTP fetching behavior.
- `src/net.rs` provides shared network helpers — normalized DNS hosts, loopback/local test-server detection, `affirmation_cookie_jar()` (the age-affirmation cookie jar scoped to `livejournal.com` and installed on every production client), and `configured_client_builder(user_agent)`, which applies that jar, the shared TLS config, and an optional `User-Agent` (both `LjClient` and the media client build on it). Restricted IP-literal validation belongs to `src/media.rs`.
- `src/tls.rs` builds the shared rustls `ClientConfig` applied to every HTTP client (see HTTP And Media Safety).
- `src/discover.rs` finds journal post IDs by walking the calendar, per-year pages, and month archive pages.
- `src/parser.rs` parses LiveJournal HTML into post and comment models.
- `src/links.rs` resolves references against each delivered post URL and records typed active same-journal link spans for resolution and trusted retained-post upgrades.
- `src/link_metadata.rs` validates, serializes, and applies digest-bound active-link metadata for retained posts without scanning their source for link-like text.
- `src/media.rs` finds inline media, canonicalizes safe sources even when downloads are disabled, downloads allowed assets, rewrites HTML references, validates redirects, and rejects unsafe media URLs based on scheme, host presence, literal `localhost` hosts, and restricted IP literals.
- `src/adoc/` converts supported HTML into AsciiDoc and preserves unsupported structures with passthrough output (submodules `mod`, `classify`, `source_html`, `escape`).
- `src/archive.rs` owns archive paths, fail-closed existing-archive inspection, pending-post staging, controlled post-and-link-sidecar publication, index writes, and the optional single-file `book.adoc` assembly (`write_book`).
- `src/storage.rs` atomically replaces archive files through same-directory temporary-file persistence, which safely clobbers existing files on Windows, and serializes AsciiDoc paths with forward slashes independently of the host filesystem.
- `src/run.rs` orchestrates target parsing, discovery, fetching, parsing, media rewriting, rendering, bounded staging, and archive finalization.
- `src/progress.rs` owns user-facing progress output: a `Progress` handle with Interactive (indicatif bar/spinner via `tracing-indicatif`), Plain (stderr lines), and Quiet modes, plus `init_logging`. Progress is stderr-only UI, independent of the log level; `--verbose` keeps the bar and prints logs above it, `--quiet` suppresses progress.
- `tests/fixtures/post_basic.html` covers a basic post fixture.
- `tests/fixtures/post_with_comments.html` covers a post with threaded comments.
- `tests/fixtures/journal_calendar.html` covers journal calendar discovery.
- `tests/fixtures/journal_month.html` covers month archive discovery.
- `tests/cli_smoke.rs` contains end-to-end CLI tests using local HTTP servers.

## GUI And Packaging

The repo is a Cargo workspace. The root crate `lj-archiver` (lib + CLI) stays
pure-Rust with no GUI dependencies; the member crate `gui/` (`lj-archiver-gui`)
holds the iced 0.14 desktop GUI. CI keeps them apart: the `ci` job lints/tests
`-p lj-archiver` (fast, no iced), the `gui` job lints, tests, and builds the GUI, and
`cargo fmt --all` covers both.

Reuse seam (in the core, additive, CLI behavior unchanged):
- `run::run_archive(target, &RunOptions, &Progress) -> Result<RunOutcome>` is the
  shared entry point. It returns `Ok` even when some journal posts/pages failed
  (counts + `failures` in `RunOutcome`); only a hard failure yields `Err`.
  `RunOutcome::warnings` is the authoritative typed record of warning-only
  integrity degradation; it must not depend on tracing or the lossy progress
  channel. The
  CLI `run()` wrapper turns a partial-failure outcome back into
  `LjError::Discovery` (non-zero exit) — so the CLI is byte-identical, while the
  GUI can show a completed run with a failed-links list.
- `progress::ProgressEvent` + `Progress::channel(tx)` add a fourth sink beside
  Interactive/Plain/Quiet. Channel progress is deliberately bounded and lossy:
  all `Progress` clones share one mutex-protected `futures::mpsc::Sender`, and
  emission uses nonblocking `try_send`, silently dropping intermediate events
  when the buffer is full or disconnected. `futures` is already a core dep; do
  not add `tokio-stream` (a `futures` mpsc receiver is already a `Stream`).

GUI structure: `gui/src/state.rs` is a pure reducer (`App::update_state`) with
full unit tests; `gui/src/app.rs` is the iced glue (`boot`/`update`/`view`).
Start snapshots and parses `pending_validation` on the iced event thread, then
immediately enters the visible generation-tagged `Phase::Validating`. Blocking
directory reads and `Archive::inspect_existing` run on a standard worker thread
and return through a `futures` oneshot. Only the matching validation generation
may start the progress stream and archive task; stale or duplicate completions
are inert, and the core `run_archive` preflight remains authoritative.
Progress is driven by one capacity-128 channel in
`Task::batch([Task::run(rx, Message::Progress),
Task::perform(run_archive(...), Message::Finished)])`. Both callbacks carry
the active run generation. Progress may be dropped, so the separately delivered
generation-tagged `Finished` result is authoritative for terminal state and
exact counters, failures, and integrity warnings. The iced view is manual-verify only (no headless harness) — smoke-test with
`cargo run -p lj-archiver-gui` against a small real post.

Folder and failed-post link launches carry an independent monotonic generation
and return `open::that` results through iced tasks. A matching launch failure is
a non-terminal notice beneath the normal status; it never replaces completion
counts, failures, or warnings, and stale launch results are inert.

The reducer uses `Phase::FetchingPost` for a direct post target and for
`ProgressEvent::SingleStarted`; journal targets alone display discovery.

The GUI reuses core `Archive::inspect_existing` and accepts a selected folder
only when it is truly empty or a valid matching existing archive. It derives an
existing archive's identity from the controlled `index.adoc` title and compares
journal names with the shared hyphen/underscore alias rules before enabling
auto-resume.

Packaging uses cargo-packager (`[package.metadata.packager]` in `gui/Cargo.toml`).
`.github/workflows/release.yml` accepts only an exact `v<workspace-version>` tag
whose commit is on `master`, executes the same-commit CI gates, and then builds
installers: macOS `.dmg`
(Apple-Silicon-only — `macos-latest` is arm64, native build, no `--target`),
Linux `.deb`+AppImage, Windows NSIS+`.msi`. Builds are unsigned. rfd 0.17 uses
the XDG portal by default (no GTK3 link), keeping Linux free of GTK system libs.
The matrix uploads installer-only artifacts; one final fan-in job rejects a
missing or unexpected five-installer set before publishing once. All Actions
are full-SHA pinned, Dependabot owns weekly GitHub Actions update proposals,
and only that final publisher receives `contents: write`. Initial tag and
ancestry validation is not sufficient: immediately before creation, the
checkout-free sole publisher re-queries the live remote tag, bounded-peels at
most eight annotated tag objects, and requires the resolved commit to equal
`GITHUB_SHA`. The installer version/filename comes from
`workspace.package.version` in the root `Cargo.toml`, so bump it before pushing
the matching version tag.

## Development Commands

Run these before handing off changes:

```sh
cargo fmt --all --check -- --config=imports_granularity=Crate
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

Use focused commands while iterating, then run the full set before final review.

The workspace root is itself the `lj-archiver` package, so bare `cargo test`
and `cargo clippy` cover **only** that crate. The final bundle above uses
`--workspace`; CI mirrors it with separate core and GUI jobs. `cargo fmt --all`
already covers both crates.

CI enforces the exact `fmt` invocation above. There is no `rustfmt.toml`, so the `--config=imports_granularity=Crate` flag is required; plain `cargo fmt` reformats imports differently and will fail the check.

After installing Asciidoctor, Ubuntu CI explicitly runs both ignored converter
harnesses. The library command covers renderer semantics and the integration
command covers assembled-book rendering:

```sh
cargo test -p lj-archiver --lib -- --ignored --test-threads=1
cargo test -p lj-archiver --test book_render_smoke -- --ignored --test-threads=1
```

Both commands must execute nonzero tests; a filtered-to-zero result is not
converter coverage.

The Windows CI job owns coverage for same-directory atomic replacement and the
concurrent media replacement path.

`Cargo.lock` is gitignored — commit `Cargo.toml` only for dependency changes. CI additionally runs `cargo audit`, validates `CITATION.cff` with `cffconvert`, and executes Clippy/tests across feature combinations. The core each-feature Clippy gate is `cargo hack clippy -p lj-archiver --each-feature --all-targets -- -D warnings`, so every core target is linted under `-D warnings`.

With cargo-hack 0.6.45, Clippy rejects combining `--no-dev-deps` with
`--all-targets`, and `cargo hack test` rejects `--no-dev-deps` before invoking
Cargo. Run the two meaningful Clippy halves separately and retain dev
dependencies for test powersets:

```sh
cargo hack clippy --workspace --feature-powerset --depth 1 --exclude-features default --no-dev-deps -- -D warnings
cargo hack clippy --workspace --feature-powerset --depth 1 --exclude-features default --all-targets -- -D warnings
cargo hack test --workspace --feature-powerset --depth 1 --exclude-features default
```

## Testing Rules

Tests must not hit LiveJournal or any other external site.

Use checked-in fixtures and local HTTP test servers for parser, discovery, media, redirect, and CLI behavior.

Because tests use fixtures only, a green suite does not prove the parser or discovery works against real LiveJournal — the live markup has drifted before while every fixture-based test passed. Manually verify parser and discovery changes against a live page (for example `cargo run -- --no-media <journal>/<id>`), and derive any new fixture from real fetched HTML rather than hand-writing it.

When CLI behavior changes, update all relevant tests and README examples in the same change.

Discovery, target parsing, and URL building have two journal URL-scheme code paths (subdomain and `users.livejournal.com`; see LiveJournal Discovery And URL Structure). Cover both — a change that passes for one scheme can silently break the other.

When archive output changes, assert the exact output that matters. Keep output stable unless the task explicitly changes it.

## Implementation Guidance

Prefer small, scoped changes that match the existing module boundaries.

Avoid unrelated refactors. Boundary cleanup is appropriate when it directly supports the task being implemented.

Keep user-visible behavior consistent with the README. If behavior changes, update the README in the same change.

Preserve public-only archiving behavior unless authentication or private-content support has gone through a separate design.

Treat comments and media as part of the archive record. Do not silently drop content that can be preserved.

For journal archives, preserve the current resilient behavior: continue past individual post failures, write successful posts, and return a nonzero status when any post failed.

The AsciiDoc renderer (`src/adoc/`) produces byte-exact output guarded by a large black-box test suite; change it with care and assert exact output. Passthrough source recovery precomputes range-backed original-byte mappings with one tokenizer-compatible source scan plus one DOM traversal; both must remain linear. The shared scanner treats comments, declarations, and raw-text/RCDATA payloads as opaque, and aligns tokenizer-decoded attributes with ranges into the unchanged input rather than decoding or rescanning source values itself. Script payload scanning must retain ScriptData escaped and double-escaped states: an appropriate `</script>` in double-escaped data only returns to escaped data, so it is not the element's structural close. Quotes enter quoted attribute-value state only after `=`, and a self-closing non-void tag stays on the tag stack so normalized fallback retains its trailing children. A `ScanError` means alignment is unavailable: callers must use their explicit conservative fallback or return a parse error, never guess source ranges or attributes. Raw-text collection is iterative, while recursive rendering and plain-inline collection are capped at `MAX_RENDER_DEPTH`; missing a recursive cap lets deeply nested input overflow the stack (an uncatchable abort).

The renderer also embodies these output conventions — keep them unless a task deliberately changes them: the post body follows the attribute block directly, with no `== Post`/`== Comments` section headings; LiveJournal chrome injected into the body (e.g. `<div class="lj-like…">`, and the generic userhead glyph `<img class="i-ljuser-userhead">` in user mentions, which then render as just the linked username) is stripped; and `<br>` runs collapse — a lone `<br>` renders as an AsciiDoc hard line break (` +`), while two or more consecutive `<br>` become a paragraph break. A hard break only separates content, so a break with nothing before it on its line is dropped at **both** ends — trailing (ending a block/body) and *leading* (starting a block, or landing where output already broke the line), else a leading break's space is trimmed into a stray lone `+` line. Leading-drop lives in `flush_pending_line_breaks` and, for blockquotes, `trim_leading_hard_breaks`; a break *between* content is kept. That kept lone-`<br>` newline is load-bearing — it lands the next content at a line start so a block macro there (`include::`, `:toc:`, a list marker, …) is neutralized; dropping a *leading* break is safe because empty/newline-terminated output is already a line start. Plain user-originated fragments are neutralized across inline boundaries so macro, attribute, heading, list (including `-` list markers), comment (`//`), description list (`term::`), page/thematic break (`<<<`/`'''`), block title, open-block delimiter, table delimiter, and related AsciiDoc syntax stays inert. Authored prose at block-metadata line starts is escaped rather than allowed to activate roles, substitutions, or other metadata. Inline code remains one semantic code span assembled from special-character-only segments and fixed delimiter fragments, so authored backticks cannot close it or enable substitutions. An `<a>` whose only content is an `<img>` renders as an inline linked image (`image:target[alt,link=href]`), never a block macro inside link brackets.

Renderer-owned inline output (strong, emphasis, code, anchors, linked images) is
appended through the single `append_generated_inline` path in `src/adoc/mod.rs`.
That path compares the current logical line before and after the append and, when
the generated delimiters *newly* complete active block syntax (a block title such
as `.` + `*bold*`, a list marker, a heading, `[...]` block metadata, a delimiter
line, …), inserts the trusted empty inline passthrough `pass:[]` at the line
start. The guard is renderer-only — authored `pass:` macros are neutralized by
the authored-text pass, so post content cannot forge it. Never append generated
inline output with a bare `append_inline_fragment`; that reintroduces
line-completion attacks that the authored-text neutralizer cannot see, because
the authored run alone is inert.

There is exactly one exemption, and it is deliberately narrow. A bare `<img>`
renders to a real block macro (`image::src[alt]`), so when *that fragment*
stands alone on its line it is the renderer's intended output and must not be
guarded — a `pass:[]` in front of it renders the image as literal text instead.
The exemption keys on the fragment, never on "the line was previously empty":
an anchor whose sanitized target begins with `.` (`../other/page.html[x]`,
`./page.html[x]`) also starts its line, and left unguarded Asciidoctor parses it
as a block title that swallows the *following* block. Both behaviors are pinned
by tests — widening the exemption to a blank-line check reopens the second one.

Attribute-reference neutralization includes Asciidoctor's state-changing
`{set:...}`, `{counter:...}`, and `{counter2:...}` forms, not only simple
`{name}` references. Authored commas in image labels are numeric-reference
encoded before an attribute list. Authored backslashes in raw macro labels and
terminal backslashes in already-rendered labels, headings, and document
attributes are encoded before a renderer-owned delimiter or newline. These
encodings preserve converted visible text while preventing attribute
injection, macro absorption, and line continuation into trusted output.

A same-journal target in a linked image's `link=` attribute is a contextual
`{lj-archiver-post-link-prefix}<id>.adoc` URI, not a complete
`xref:<id>.adoc` macro. Every generated post that has a `LinkRewriter`
conditionally defaults that internal attribute to empty, preserving the
standalone `link=<id>.adoc` result; `book.adoc` sets it to `posts/` before any
post include, and the included post's conditional must not override it.
Pending link records retain that output context: ordinary links resolve
locally to `xref:`, linked images resolve to the contextual URI, and a
linked-image external fallback has commas encoded before token substitution so
it cannot split the image attribute list.

Pending tokens use a fresh random 128-bit namespace for each render. The
complete rendered post is validated so every recorded token occurs exactly
once before it can be staged or resolved. A namespace collision causes a full
rerender with another fresh namespace; four-attempt exhaustion is a
`LinkIntegrity` failure rather than permission to substitute ambiguous text.

Unsupported HTML remains exact source-preserved raw passthrough only when the
shared scanner classifies every tokenizer-decoded capability as reviewed and
inert. HTML-to-AsciiDoc conversion performs one authoritative scan before DOM
rendering and reuses that successful scan for source-map construction. Any
`ScanError` returns one visible listing of the complete unchanged input
fragment; do not continue with an empty map, serialize an unmapped element, or
rescan normalized fallback HTML as though it were trusted original source. The
element allowlist is ordinary structural/phrasing HTML through
tables; the only global attributes are `class`, `id`, `title`, `lang`, `dir`,
`role`, and `aria-*`. Tag-specific attributes are limited to `a[href]`,
`img[src,alt,width,height]`, `col`/`colgroup[span]`,
`ol[start,reversed,type]`, `li[value]`,
`td`/`th[colspan,rowspan,headers,scope,abbr]`, `time[datetime]`, and
`del`/`ins[datetime]`. Qualified attributes, `on*`, `style`, `srcdoc`, custom or
namespaced elements, and every unreviewed element or attribute fail closed to a
visible listing. `href` and `src` decisions use the scanner's decoded value:
hosted HTTP(S) absolute URLs and scheme-less references are safe; explicit
non-HTTP(S) schemes and malformed absolute URLs are not. Unreviewed elements
remain exact listings even in inline positions and nested within reviewed
inline wrappers; both root and child accumulators must flush surrounding text
and append such output as a block so listing delimiters stay at line boundaries.
Plain-text collectors must reject unreviewed names rather than flattening away
their tags or attributes. The direct raw-text renderers for `code` and `pre`
must likewise detect any unreviewed element descendant and route the complete
source-preserved containing element through passthrough classification; do not
splice guessed descendant source into literal text. (`script` and `style`
bodies parse as raw text nodes, so they cannot contain parsed element
descendants.) Never guess source ranges or serialize a safety DOM.

All generated listings declare `[subs=specialcharacters]` immediately before
the delimiter. Every generated block delimiter is chosen with one pass over `content.lines()`:
inspect each line once, remember the longest line made only of the delimiter
character, and emit one more character than that length (with four characters
as the minimum). Listings preserve source terminal newlines by construction:
append the separator newline only when the source lacks one. Do not restore a
candidate-by-candidate rescan ladder.

Without a `LinkRewriter`, image macro targets retain scheme-less references and
hosted HTTP(S) URLs only; explicit non-HTTP(S) or malformed absolute schemes
fall back to inert visible text. `LinkRewriter`-controlled asset and absolute
target branches are unchanged.

The composed document ends with exactly one newline **by construction**, not a post-hoc trim: each block ends with a single `\n` (`push_body`) and the blank-line separator is inserted *before* each block (`render_post`, `render_comment`). Do not switch to per-block `\n\n` plus a trailing trim.

The optional `book.adoc` (opt-in initially via `--book` or the GUI checkbox, both setting `RunOptions::generate_book`) is assembled by `archive::Archive::write_book`, called after `write_index` in both run paths (`archive_post`, `archive_journal`) from the **same sorted `&[ArchivedPost]` slice** — so it lists exactly what `index.adoc` lists, oldest-first. Once a validated archive already has a book, every later successful update rewrites it even when the current run omits the option; otherwise generation remains opt-in. Its output is byte-exact and asserted like the rest of `archive.rs` (the `write_book` unit tests pin the exact bytes): a `= <journal> Archive` title carrying `:doctype: book`, `:toc:`, `:imagesdir: posts`, and `:lj-archiver-post-link-prefix: posts/`, then `include::index.adoc[leveloffset=+1]`, then one include for each post's surviving actual path. Two load-bearing invariants: a blank line precedes **every** include (block separation — without it an included post's now-`==` chapter title is absorbed into the preceding post's trailing paragraph), and the file ends with exactly one newline. `leveloffset=+1` demotes each standalone post's level-0 `= Title` to a `==` chapter under `:doctype: book`, and `:toc:` supplies in-document navigation; the included `index.adoc`'s path-based `xref:` links deliberately resolve to the sibling post files, not in-book anchors (they need no existing target, so no converter warning). `:imagesdir: posts` is load-bearing for downloaded media: included post targets such as `../assets/<hash>.<ext>` then render as archive-root `assets/...` paths when converter output is written beside `book.adoc`. The internal post-link prefix is independently load-bearing for linked images whose click target is another archived post: standalone posts default it to empty (for example, `01.adoc`), while the book keeps it as `posts/` (for example, `posts/01.adoc`). `write_book` shares `ensure_root()` (archive-root creation) with `write_index`; `write_post` keeps its own controlled post-only write path for a different purpose — do not merge those. Apart from those context attributes, book assembly emits only include directives and leaves the rendered post body unchanged. Ubuntu CI explicitly runs the ignored library Asciidoctor tests for renderer semantics and the ignored `book_render_smoke` integration tests for assembled output; normal Rust test runs remain converter-independent.

## LiveJournal Discovery And URL Structure

Discovery walks LiveJournal's own archive navigation, all served as static (no-JS) pages:

- The journal `/calendar/` page links one page per year (`/YYYY/`), plus month pages (`/YYYY/MM/`) for the current year only. Each year page links that year's month pages, and each month page links individual post URLs. `src/discover.rs` walks `calendar → year → month → posts`, reading year links from the calendar only (every archive page repeats the year navigation, so recursing would re-crawl). Matching only `/YYYY/MM/` links off the calendar silently limits a journal to its current year — the year hop is required.
- Discovery parses archive links against reqwest's final response URL. This matters because underscore-journal calendar URLs redirect to hyphen subdomains.
- `DiscoveryTick.found` always means unique post IDs found so far. Year-page ticks keep the current post count unchanged; a successful month inserts its parsed IDs before emitting the next monotonic count. Never substitute month/page totals for this field.
- Posts are archived oldest-first (discovered IDs are sorted ascending), so the smallest archived post ID reflects how far back a walk reached — useful when confirming a full walk. A full multi-year pull makes many sequential requests before the first post is written, and there is no request throttling.

Two journal URL schemes exist; handle and test both wherever discovery, target parsing, or URL building touch a journal:

- Names without `_`: `https://<journal>.livejournal.com/...` — calendar `/calendar`, year `/YYYY/`, month `/YYYY/MM/`, post `/<id>.html`.
- Names with `_`: `https://users.livejournal.com/<journal>/...`, where every path is prefixed with `/<journal>` (`/<journal>/calendar`, `/<journal>/YYYY/`, `/<journal>/YYYY/MM/`, `/<journal>/<id>.html`). Link filtering must strip that prefix using the known journal name, not re-derive it from the base URL's shape.

Accepted target forms (`src/target.rs`): a bare journal name, a `*.livejournal.com` hostname, a journal home URL, or a single-post URL — for either scheme.

Real post pages use `aentry-post*` markup — body `.aentry-post__text`, title `.aentry-post__title-text`, tags `.aentry-tags a` — with the older `.entry-*` classes kept only as fallbacks. Real post pages embed a server-rendered `Site.entry = {…}` JSON object carrying the post's `journal` (the canonical account name), `poster` (the authoring account, distinct from the journal), and `eventtime` (a Unix-epoch publication timestamp). `src/parser.rs` anchors on `Site.entry` (the page also carries many unrelated `"poster"` values in sidebar JSON), deserializes the single leading object with `serde_json`, and trusts it only when its `ditemid` equals the resolved post id. `poster` populates `:lj-poster:`; `eventtime` populates `:lj-date:` via `DateTime::from_timestamp`, falling back to the older `time.entry-date[datetime]` selector when `Site.entry` is absent. Post pages otherwise have no `datetime` attribute, meta tag, or JSON-LD date. `eventtime` is recorded as a UTC instant, but LiveJournal's event time historically reflects the poster's local wall-clock, so treat `:lj-date:` as a best-effort date (reliable to the day). The journal is sourced from `Site.entry.journal` when its `ditemid` matches (then `data-journal`, then the URL host), so `:lj-author:` is the canonical account name; the poster is a separate `:lj-poster:` attribute.

Before either run path constructs a client, `Archive::inspect_existing`
preflights every nonempty output root as a controlled archive, including the
`assets`, `.lj-archiver`, and `.lj-archiver/links` paths. Existing symlinks,
non-directory controlled components, and non-regular controlled leaves are
rejected before networking. The index journal must alias the requested journal,
and public post candidates must be readable regular files with lone positive
numeric `.adoc` stems and nonempty level-zero titles. Malformed, unverified, or
mismatched roots fail closed before networking or writes. Missing and truly
empty roots alone begin without a snapshot.

Validated snapshot posts seed an ID-keyed additive final map for single-post and
journal updates. Current successes replace/add; resume skips, failed refetches,
and baseline posts omitted by partial rediscovery retain their trusted entry.
Current-run archived/skipped/failure counters exclude entries that were merely
retained. Index and book consume the same sorted final `ArchivedPost` values.
`ArchivedTitle` preserves whether each title is fresh authored text or an
already-rendered retained heading, so index regeneration applies the matching
sanitizer without double-escaping renderer protection. Inspection reads only a
maximum 64 KiB newline-terminated UTF-8 controlled title line from `index.adoc`
and each public post; it never decodes an entire retained post body merely to
recover its title.
Existing validated positive numeric filename spellings are a public-path
contract: replacement, repair, manifests/book, ordinary local xrefs, and linked
image targets retain the actual inspected filename (for example `01.adoc` for
ID 1). New IDs alone use canonical `<id>.adoc`; never migrate, delete, or create
a canonical duplicate for a validated alias.

On a bulk journal export, rendered bodies and their validated pending links are
staged on disk as non-public `posts/*.pending` files while the fetch stream
remains bounded by `RunOptions::concurrency`. As each preparation completes,
first-stage finalization resolves every staged link to its absolute href from
that post's delivered URL and atomically publishes an externally complete
`.adoc` at the selected actual path. A best-effort atomic second pass upgrades
links to IDs in the additive final set, so a failed upgrade retains the complete
external fallback and public files never contain staging tokens. Pending-link
substitution uses the validated exact-once records and one monotonic cursor and
must remain linear.

`run::run_archive` rejects `RunOptions::concurrency == 0` with
`LjError::InvalidOptions` before target networking; never pass zero to
`buffer_unordered`. Pending-post read/write/remove helpers are crate-private,
accept a positive post ID rather than an arbitrary path, derive exactly
`posts/<id>.pending`, and validate the controlled regular leaf before reading
or removing it.

Normal successful pair publication writes an implementation-owned matching
sidecar under `.lj-archiver/links`. A sidecar binds strictly sorted,
nonoverlapping typed active spans to the exact SHA-256 digest of the full public
post. A warning-only post-success metadata I/O failure may leave that sidecar
missing or stale while the externally complete post remains successful. Retained
repair first validates the digest, span boundaries, context, target identity,
and current expression, then touches only those valid recorded spans. Missing,
older, stale, malformed, or user-invalidated sidecars preserve the post and
warn; there is no heuristic or broad-source fallback repair.

Publication derives and validates both the numeric `posts/*.adoc` path and its
hidden metadata path before either write. Controlled paths accept normalized
components only, create or inspect real directory parents, reject symlinks and
non-regular destinations, and verify canonical parent containment within the
archive root. This applies to asset writes as well as hidden metadata writes.
Publication writes the post first and then its sidecar. A post-success/sidecar
I/O failure is a warning because the externally complete post is already
public; serialization or path-validation failure before publication remains a
hard error.

Pending cleanup is best effort: it is attempted after first-stage failure and
after the upgrade pass, but an unlink failure may leave a non-public pending
file without invalidating the externally complete public fallback. A failed new
post stays out of `index.adoc` and `book.adoc`, while a failed existing post
retains its validated baseline entry. Journal names are matched with `-`/`_`
folded together. Downloaded media is stored once in a single `assets/` folder
at the archive root (filenames are a URL hash, so a reused image is stored once
and referenced as `../assets/<hash>.<ext>`).

Fetched posts are parsed against the delivered final response URL, which is
preserved as `Post.url` for rendering and downstream processing. Before media
requests or writes, the parsed post ID must match the requested post; a
recognizable final LiveJournal journal must alias the requested journal even
when its path is not a recognizable post path, and an independently
recognizable final post ID must match the requested ID. A loopback test URL
only validates its numeric final path segment (with optional `.html`) against
the requested ID. Identity mismatches fail only that row during journal exports
and fail the whole run for a single-post target; no file is staged or written
under the returned wrong ID.

DNS hosts may carry the equivalent trailing root dot. Target parsing,
discovery calendar/year/month/post filtering and same-origin checks, final-link
identity, and `localhost` recognition strip one such dot consistently; IP
literals continue to use their parsed `Host` variants and are not normalized
as domains. After strict acceptance of a production LiveJournal post target,
its returned client URL removes one terminal root-label dot before requests so
the domain-scoped age-affirmation cookie jar matches the canonical hostname;
loopback `localhost.` test-server aliases are not rewritten.

Every archive render constructs a per-post `LinkRewriter` only after identity
validation and uses that post's delivered `Post.url` as the sole resolution
base. The body, comment bodies, and linked-image hrefs share this context.
Same-journal exported posts remain local or pending; other root-relative,
path-relative, and protocol-relative HTTP(S) hrefs become absolute generated
targets. Non-HTTP(S) hrefs and image sources render as neutralized visible text,
never active macros.

## HTTP And Media Safety

Keep network behavior conservative.

Do not weaken redirect validation or address blocking in `src/media.rs`.

Media fetching must continue to reject non-HTTP(S) URLs, URLs without hosts, literal `localhost` hosts, and every IP literal that is not public-routable unicast. The literal policy follows the current IANA-style standard-library classifications: it rejects unspecified, loopback, private, link-local, shared/CGNAT, special-use, documentation, benchmarking, multicast, reserved, broadcast, IPv6 unique-local, IPv6 site-local, discard-only, local-use translation, IETF protocol-assignment, and SRv6 ranges, plus IPv6-embedded IPv4 forms (IPv4-mapped, NAT64, 6to4, and IPv4-compatible) whose decoded IPv4 address is not public-routable unicast. Redirects are re-validated through `media_url_is_allowed` per hop, so a blocked redirect is never dispatched. The only test seam is its final explicit `loopback_page_mode && net::is_loopback_test_server(url)` branch, which permits individually scoped loopback test-server URLs; it does not relax the production policy.

Hermetic fixture downloads enable a loopback-page mode only when the delivered
page is an explicit-port loopback HTTP URL. That mode does not bypass the media
policy globally: each initial candidate and redirect is allowed through the
restricted-literal check only when that individual URL is also an
explicit-port loopback HTTP URL. Private, link-local, portless loopback, and
HTTPS loopback targets remain blocked.

Media source scanning runs even under `--no-media`. Every real hosted HTTP(S)
source is resolved against the delivered post URL and rewritten to either a
local `../assets/...` target after a successful download or that resolved
absolute remote URL on no-media, address/redirect rejection, HTTP or transport
failure, and body-cap failure. Invalid and non-HTTP(S) values issue no request
and rely on the renderer's inert visible-text fallback.

Inline-media discovery completes the shared tokenizer-compatible scan before
issuing any request. The tokenizer-decoded `src` drives URL resolution, but
rewriting changes only its aligned raw value range in the immutable input, so
all other bytes (including quote style) remain untouched. If source/tokenizer
alignment is ambiguous, media rewriting returns a URL-contextual
`LjError::Parse` before network I/O rather than guessing a range. Generated
replacements are HTML-attribute-escaped because the renderer parses the
rewritten fragment again: that one parse must recover the exact target.
Download filenames keep a 12-hex digest stem and use only lowercase allowlisted image
suffixes (`jpg`, `jpeg`, `png`, `gif`, `webp`, `svg`, `avif`, `bmp`, `ico`), falling back to
`bin` so each filename component remains portable and bounded.

The current media checks do not resolve DNS names to block private networks. Do not document or rely on DNS-based private-network blocking unless that behavior is implemented and tested.

Downloads are size-capped and enforced while streaming — 64 MiB per page fetch, 50 MiB per media asset — and a response exceeding its cap is abandoned mid-stream, never truncated into the archive. The page-fetch client retries transient 5xx responses, transport errors, and interrupted response bodies a couple of times before surfacing a failure; interrupted-body attempts discard their partial bytes before retrying.

Page-fetch errors caused after a response was reached retain that response's
final URL (including status, body-size, and exhausted interrupted-body errors).
A transport error uses a later URL from reqwest only when the error identifies
one; otherwise a run with no reached response reports the requested URL.

When changing HTTP or media behavior, cover redirects and blocked-address cases with local test servers.

Every `reqwest::Client` must be built with the shared TLS config from `src/tls.rs` (`.use_preconfigured_tls(rustls_config())`). Because the `rustls-no-provider` feature is used, a client that omits it panics at the TLS handshake — not at compile time — and the `rustls` dependency version must match reqwest's. Tests use HTTP-only local servers, so none exercises TLS; verify HTTPS manually after changing this or the TLS dependencies.

Every production `reqwest::Client` installs `net::affirmation_cookie_jar()` — a cookie jar scoped to `livejournal.com` holding the static, tokenless age-affirmation cookies (`adult_explicit=1; adult_concepts=1`) — and by default sends **no** `User-Agent`; the optional `--user-agent` flag (carried as `RunOptions::user_agent` and applied through `net::configured_client_builder` on both the page-fetch and media clients) sets one on every request. A jar rather than a static `Cookie` header, because reqwest strips a manually-set `Cookie` on a cross-host redirect, and underscore journals are fetched via `users.livejournal.com/<journal>/<id>.html`, which 301-redirects to `<journal>.livejournal.com`; the jar is re-consulted per hop so the affirmation survives that redirect, and its `livejournal.com` scope keeps the cookie off non-LiveJournal hosts (e.g. third-party media). This is what lets the archiver read adult-flagged *public* posts: LiveJournal otherwise serves an age-affirmation interstitial (a form posting to `misc/adult_explicit.bml`) instead of the post body. Sending the cookie is equivalent to the interstitial's "Yes, I am at least 18" click-through; it is harmless on non-gated pages, and LiveJournal serves content with no `User-Agent`. As a safety net, `parser::parse_post_page` returns `LjError::AgeGated` when it still sees the interstitial (detected via the `misc/adult_explicit.bml` marker), so a future change to this mechanism surfaces as a clear error rather than a generic "missing post body".

## Progress Output

`src/progress.rs` renders archiving progress to stderr (stdout is unused). A `Progress` handle picks one of three modes at startup from `--quiet` and whether stderr is a TTY: Interactive (an `indicatif` bar/spinner driven through `tracing-indicatif` progress spans), Plain (stderr status lines when piped/redirected), and Quiet (`--quiet`, silent except errors). Discovery shows a spinner with a running total; archiving shows a determinate bar (count + ETA); both degrade to plain lines when piped.

Remote titles pass through one display-only sanitizer before reaching either
Interactive messages or Plain stderr lines. It removes C0/C1 controls and bidi
formatting controls so ANSI/OSC/log spoofing cannot execute, while the parsed
title used by archive rendering remains unchanged.

Logging and progress share stderr, so `init_logging` wires `tracing-indicatif` deliberately — keep these invariants if you touch it:

- Progress spans use a private tracing target (`PROGRESS_TARGET`), and the `IndicatifLayer` is filtered to that target alone, so only our spans draw bars — never dependency spans (which would otherwise show up as stray bars under `-v`).
- The `EnvFilter` (log level) is attached to the fmt layer only, not to the registry globally. That is what keeps progress independent of the log level: progress spans are `info`-level yet still render at the default `warn` level and under any `RUST_LOG`.
- Bars must be layer-managed (created via the `tracing-indicatif` span extensions), because the fmt layer writes through the layer's `get_stderr_writer()`, which suspends exactly that layer's own bars around each log line. A bar owned by a separate `MultiProgress` would be clobbered by `-v` logs. This is why the crate is used span-based rather than hand-rolling a writer.
- `init_logging` installs the subscriber with `try_init` (ignoring the already-initialized error), so repeated in-process calls from tests do not panic.

Testing limitation (hard-won): because `init_logging` installs a process-global subscriber via `try_init`, a test that installs a real subscriber would win the global slot and pollute every other test in the shared test binary. So the Interactive bar and the `-v`-keeps-the-bar-clean coexistence are **not** unit-tested — they are covered by the Plain-mode `cli_smoke` tests (per-post lines, `--quiet` silence, empty stdout) plus manual smoke: run `cargo run -- --no-media <journal>/<id>` in a real terminal to see the bar, and a piped run to confirm stdout stays empty. If you change the layering, verify the interactive bar and `-v` coexistence by hand — no automated test will catch a regression there.

## Known Limitations

- Comments on modern LiveJournal are JavaScript-loaded and absent from static HTML, so a static fetch cannot archive comment threads.
- `:lj-author:` is the journal name, not the poster — the canonical account name from `Site.entry.journal` when its `ditemid` matches (e.g. `evo_lutio`, not the `evo-lutio` subdomain spelling, and never `users` for `users.livejournal.com` posts). The real poster is captured separately as `:lj-poster:` from `Site.entry`; for personal journals the poster equals the journal name.

## Documentation Rules

**After each iteration, write hard-won knowledge into committed docs before you finish.** Assume your working context can be wiped at any moment — anything not captured in committed docs, code, or tests is lost. Whenever you learn something non-obvious (a subsystem's behavior or invariants, why an approach was chosen or rejected, a gotcha, a dependency quirk, a testing limitation, a debugging insight), record it in this file or under `docs/` as part of the same change — not after "one more task." Do not leave durable knowledge only in chat, a commit message, or agent-private memory; put it where the next contributor (human or AI) will find it. Prefer updating the relevant existing section over appending a stray note.

Do not include machine-specific paths, private source names, or one-off development context in repository docs.

Document implemented behavior only. Avoid promises about unimplemented behavior in user-facing docs.

Keep README examples copy-pasteable for normal users of the compiled binary.

Record durable project and technical knowledge — subsystem behavior, LiveJournal and domain structure, gotchas, and conventions — in this file or under `docs/` so it is versioned and shared with everyone working the repo, not in agent-private memory.
