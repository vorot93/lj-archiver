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
- `src/models.rs` defines core archive, post, comment, media, and run option data types. `RunOptions::generate_book` is the opt-in single-file `book.adoc` toggle, set by the `--book` flag (and the GUI checkbox).
- `src/target.rs` parses target strings (bare journal names, `*.livejournal.com` hostnames, journal home URLs, and post URLs) and builds LiveJournal post and calendar URLs.
- `src/client.rs` owns HTTP fetching behavior.
- `src/net.rs` provides shared network helpers — loopback/local test-server detection and restricted IP-literal checks (used by media safety and target/discovery), plus `affirmation_cookie_jar()`, the age-affirmation cookie jar (scoped to `livejournal.com`) installed on every production client, and `configured_client_builder(user_agent)`, which applies that jar, the shared TLS config, and an optional `User-Agent` (both `LjClient` and the media client build on it).
- `src/tls.rs` builds the shared rustls `ClientConfig` applied to every HTTP client (see HTTP And Media Safety).
- `src/discover.rs` finds journal post IDs by walking the calendar, per-year pages, and month archive pages.
- `src/parser.rs` parses LiveJournal HTML into post and comment models.
- `src/links.rs` records and resolves same-journal post links after bulk-export write outcomes are known, and repairs stale local cross-references during resume runs.
- `src/media.rs` finds inline media, downloads allowed assets, rewrites HTML references, validates redirects, and rejects unsafe media URLs based on scheme, host presence, literal `localhost` hosts, and restricted IP literals.
- `src/adoc/` converts supported HTML into AsciiDoc and preserves unsupported structures with passthrough output (submodules `mod`, `classify`, `source_html`, `escape`).
- `src/archive.rs` owns archive paths, non-public pending-post staging, atomic per-post writes, index writes, the optional single-file `book.adoc` assembly (`write_book`), and resume checks.
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
`-p lj-archiver` (fast, no iced), the `gui` job compiles the GUI, and
`cargo fmt --all` covers both.

Reuse seam (in the core, additive, CLI behavior unchanged):
- `run::run_archive(target, &RunOptions, &Progress) -> Result<RunOutcome>` is the
  shared entry point. It returns `Ok` even when some journal posts/pages failed
  (counts + `failures` in `RunOutcome`); only a hard failure yields `Err`. The
  CLI `run()` wrapper turns a partial-failure outcome back into
  `LjError::Discovery` (non-zero exit) — so the CLI is byte-identical, while the
  GUI can show a completed run with a failed-links list.
- `progress::ProgressEvent` + `Progress::channel(tx)` add a fourth sink beside
  Interactive/Plain/Quiet, emitting typed events to a `futures::mpsc` sender.
  `futures` is already a core dep; do not add `tokio-stream` (a `futures` mpsc
  receiver is already a `Stream`).

GUI structure: `gui/src/state.rs` is a pure reducer (`App::update_state`) with
full unit tests; `gui/src/app.rs` is the iced glue (`boot`/`update`/`view`).
Progress is driven by `Task::batch([Task::run(rx, Message::Progress),
Task::perform(run_archive(...), Message::Finished)])`. The iced view is
manual-verify only (no headless harness) — smoke-test with
`cargo run -p lj-archiver-gui` against a small real post.

Packaging uses cargo-packager (`[package.metadata.packager]` in `gui/Cargo.toml`).
`.github/workflows/release.yml` builds installers on `v*` tags: macOS `.dmg`
(Apple-Silicon-only — `macos-latest` is arm64, native build, no `--target`),
Linux `.deb`+AppImage, Windows NSIS+`.msi`. Builds are unsigned. rfd 0.17 uses
the XDG portal by default (no GTK3 link), keeping Linux free of GTK system libs.
The installer version/filename comes from `workspace.package.version` in the
root `Cargo.toml`, not from the git tag, so bump it to match before pushing a
`v*` release tag.

## Development Commands

Run these before handing off changes:

```sh
cargo fmt --all --check -- --config=imports_granularity=Crate
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Use focused commands while iterating, then run the full set before final review.

The workspace root is itself the `lj-archiver` package, so bare `cargo test` and `cargo clippy` (as above) cover **only** that crate — the `gui` member (`lj-archiver-gui`) is silently skipped, so a green run does not mean the GUI compiles or its tests pass. Add `--workspace` (or run `-p lj-archiver-gui` separately) to include it; this mirrors CI's split into the `ci` (`-p lj-archiver`) and `gui` jobs. `cargo fmt --all` already covers both crates.

CI enforces the exact `fmt` invocation above. There is no `rustfmt.toml`, so the `--config=imports_granularity=Crate` flag is required; plain `cargo fmt` reformats imports differently and will fail the check.

`Cargo.lock` is gitignored — commit `Cargo.toml` only for dependency changes. CI additionally runs `cargo audit` and executes clippy and tests across feature combinations (`cargo hack ... --each-feature`).

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

The AsciiDoc renderer (`src/adoc/`) produces byte-exact output guarded by a large black-box test suite; change it with care and assert exact output. Preserve two invariants: passthrough source recovery is a single-pass byte-span scan (keep it linear, and let self-closing non-void tags fall through to the tag stack so trailing content is not lost); and render recursion is depth-capped (`MAX_RENDER_DEPTH`) at every recursive entry point (`render_element`, `plain_text_inline_content_at`, `raw_text_at`) — missing one lets deeply nested input overflow the stack (an uncatchable abort).

The renderer also embodies these output conventions — keep them unless a task deliberately changes them: the post body follows the attribute block directly, with no `== Post`/`== Comments` section headings; LiveJournal chrome injected into the body (e.g. `<div class="lj-like…">`, and the generic userhead glyph `<img class="i-ljuser-userhead">` in user mentions, which then render as just the linked username) is stripped; and `<br>` runs collapse — a lone `<br>` renders as an AsciiDoc hard line break (` +`), while two or more consecutive `<br>` become a paragraph break. A hard break only separates content, so a break with nothing before it on its line is dropped at **both** ends — trailing (ending a block/body) and *leading* (starting a block, or landing where output already broke the line), else a leading break's space is trimmed into a stray lone `+` line. Leading-drop lives in `flush_pending_line_breaks` and, for blockquotes, `trim_leading_hard_breaks`; a break *between* content is kept. That kept lone-`<br>` newline is load-bearing — it lands the next content at a line start so a block macro there (`include::`, `:toc:`, a list marker, …) is neutralized; dropping a *leading* break is safe because empty/newline-terminated output is already a line start. An `<a>` whose only content is an `<img>` renders as an inline linked image (`image:target[alt,link=href]`), never a block macro inside link brackets.

The composed document ends with exactly one newline **by construction**, not a post-hoc trim: each block ends with a single `\n` (`push_body`) and the blank-line separator is inserted *before* each block (`render_post`, `render_comment`). Do not switch to per-block `\n\n` plus a trailing trim.

The optional `book.adoc` (opt-in via `--book` or the GUI checkbox, both setting `RunOptions::generate_book`) is assembled by `archive::Archive::write_book`, called after `write_index` in both run paths (`archive_post`, `archive_journal`) from the **same sorted `&[ArchivedPost]` slice** — so it lists exactly what `index.adoc` lists, oldest-first, and is rewritten on every enabled run (including `--resume`; failed posts are absent from the slice, resume-skipped posts are present). Its output is byte-exact and asserted like the rest of `archive.rs` (the `write_book` unit tests pin the exact bytes): a `= <journal> Archive` title carrying `:doctype: book` and `:toc:`, then `include::index.adoc[leveloffset=+1]`, then one `include::posts/<id>.adoc[leveloffset=+1]` per post. Two load-bearing invariants: a blank line precedes **every** include (block separation — without it an included post's now-`==` chapter title is absorbed into the preceding post's trailing paragraph), and the file ends with exactly one newline. `leveloffset=+1` demotes each standalone post's level-0 `= Title` to a `==` chapter under `:doctype: book`, and `:toc:` supplies in-document navigation; the included `index.adoc`'s `xref:posts/<id>.adoc[…]` links deliberately resolve to the sibling post files, not in-book anchors (path-based `xref:` needs no existing target, so no converter warning). `write_book` shares `ensure_root()` (archive-root creation) with `write_index`; `write_post` keeps its own parent-dir creation for a different path — do not merge those. Because `write_book` only emits include directives, the renderer and per-post files are untouched, and since the Rust suite does **not** convert AsciiDoc, book *rendering* is unverified here — check with `asciidoctor` by hand if a change could affect how the assembled book renders.

## LiveJournal Discovery And URL Structure

Discovery walks LiveJournal's own archive navigation, all served as static (no-JS) pages:

- The journal `/calendar/` page links one page per year (`/YYYY/`), plus month pages (`/YYYY/MM/`) for the current year only. Each year page links that year's month pages, and each month page links individual post URLs. `src/discover.rs` walks `calendar → year → month → posts`, reading year links from the calendar only (every archive page repeats the year navigation, so recursing would re-crawl). Matching only `/YYYY/MM/` links off the calendar silently limits a journal to its current year — the year hop is required.
- Posts are archived oldest-first (discovered IDs are sorted ascending), so the smallest archived post ID reflects how far back a walk reached — useful when confirming a full walk. A full multi-year pull makes many sequential requests before the first post is written, and there is no request throttling.

Two journal URL schemes exist; handle and test both wherever discovery, target parsing, or URL building touch a journal:

- Names without `_`: `https://<journal>.livejournal.com/...` — calendar `/calendar`, year `/YYYY/`, month `/YYYY/MM/`, post `/<id>.html`.
- Names with `_`: `https://users.livejournal.com/<journal>/...`, where every path is prefixed with `/<journal>` (`/<journal>/calendar`, `/<journal>/YYYY/`, `/<journal>/YYYY/MM/`, `/<journal>/<id>.html`). Link filtering must strip that prefix using the known journal name, not re-derive it from the base URL's shape.

Accepted target forms (`src/target.rs`): a bare journal name, a `*.livejournal.com` hostname, a journal home URL, or a single-post URL — for either scheme.

Real post pages use `aentry-post*` markup — body `.aentry-post__text`, title `.aentry-post__title-text`, tags `.aentry-tags a` — with the older `.entry-*` classes kept only as fallbacks. Real post pages embed a server-rendered `Site.entry = {…}` JSON object carrying the post's `journal` (the canonical account name), `poster` (the authoring account, distinct from the journal), and `eventtime` (a Unix-epoch publication timestamp). `src/parser.rs` anchors on `Site.entry` (the page also carries many unrelated `"poster"` values in sidebar JSON), deserializes the single leading object with `serde_json`, and trusts it only when its `ditemid` equals the resolved post id. `poster` populates `:lj-poster:`; `eventtime` populates `:lj-date:` via `DateTime::from_timestamp`, falling back to the older `time.entry-date[datetime]` selector when `Site.entry` is absent. Post pages otherwise have no `datetime` attribute, meta tag, or JSON-LD date. `eventtime` is recorded as a UTC instant, but LiveJournal's event time historically reflects the poster's local wall-clock, so treat `:lj-date:` as a best-effort date (reliable to the day). The journal is sourced from `Site.entry.journal` when its `ditemid` matches (then `data-journal`, then the URL host), so `:lj-author:` is the canonical account name; the poster is a separate `:lj-poster:` attribute.

On a bulk journal export, rendered bodies are staged on disk as non-public `posts/*.pending` files while the fetch stream remains bounded by `RunOptions::concurrency`. Finalization first resolves every staged link to its canonical external URL and atomically writes the public `.adoc`; only those successful writes plus resume-skipped files form the final exported ID set. A best-effort atomic second pass localizes links to IDs in that set, so a failed upgrade retains the complete external fallback and public files never contain staging tokens. Resume-skipped posts have stale `xref:<id>.adoc[…]` links to missing IDs repaired back to canonical external URLs. Pending files are removed on both finalization success and failure, and failed posts stay out of `index.adoc` and `book.adoc`. Journal names are matched with `-`/`_` folded together (LiveJournal serves an underscore username `a_b` at the hyphen subdomain `a-b.livejournal.com`, so the two spellings denote one journal). Downloaded media is stored once in a single `assets/` folder at the archive root (filenames are a URL hash, so a reused image is stored once and referenced as `../assets/<hash>.<ext>`).

## HTTP And Media Safety

Keep network behavior conservative.

Do not weaken redirect validation or address blocking in `src/media.rs`.

Media fetching must continue to reject non-HTTP(S) URLs, URLs without hosts, literal `localhost` hosts, and restricted IP literal hosts. Restricted literals include loopback, private IPv4, link-local, unspecified, IPv6 unique-local/link-local, and IPv6-embedded IPv4 forms (IPv4-mapped, NAT64, 6to4, and IPv4-compatible) that decode to a restricted address. Redirects are re-validated per hop, so a redirect to a blocked address is rejected.

The current media checks do not resolve DNS names to block private networks. Do not document or rely on DNS-based private-network blocking unless that behavior is implemented and tested.

Downloads are size-capped and enforced while streaming — 64 MiB per page fetch, 50 MiB per media asset — and a response exceeding its cap is abandoned mid-stream, never truncated into the archive. The page-fetch client retries transient 5xx responses and transport errors a couple of times before surfacing a failure.

When changing HTTP or media behavior, cover redirects and blocked-address cases with local test servers.

Every `reqwest::Client` must be built with the shared TLS config from `src/tls.rs` (`.use_preconfigured_tls(rustls_config())`). Because the `rustls-no-provider` feature is used, a client that omits it panics at the TLS handshake — not at compile time — and the `rustls` dependency version must match reqwest's. Tests use HTTP-only local servers, so none exercises TLS; verify HTTPS manually after changing this or the TLS dependencies.

Every production `reqwest::Client` installs `net::affirmation_cookie_jar()` — a cookie jar scoped to `livejournal.com` holding the static, tokenless age-affirmation cookies (`adult_explicit=1; adult_concepts=1`) — and by default sends **no** `User-Agent`; the optional `--user-agent` flag (carried as `RunOptions::user_agent` and applied through `net::configured_client_builder` on both the page-fetch and media clients) sets one on every request. A jar rather than a static `Cookie` header, because reqwest strips a manually-set `Cookie` on a cross-host redirect, and underscore journals are fetched via `users.livejournal.com/<journal>/<id>.html`, which 301-redirects to `<journal>.livejournal.com`; the jar is re-consulted per hop so the affirmation survives that redirect, and its `livejournal.com` scope keeps the cookie off non-LiveJournal hosts (e.g. third-party media). This is what lets the archiver read adult-flagged *public* posts: LiveJournal otherwise serves an age-affirmation interstitial (a form posting to `misc/adult_explicit.bml`) instead of the post body. Sending the cookie is equivalent to the interstitial's "Yes, I am at least 18" click-through; it is harmless on non-gated pages, and LiveJournal serves content with no `User-Agent`. As a safety net, `parser::parse_post_page` returns `LjError::AgeGated` when it still sees the interstitial (detected via the `misc/adult_explicit.bml` marker), so a future change to this mechanism surfaces as a clear error rather than a generic "missing post body".

## Progress Output

`src/progress.rs` renders archiving progress to stderr (stdout is unused). A `Progress` handle picks one of three modes at startup from `--quiet` and whether stderr is a TTY: Interactive (an `indicatif` bar/spinner driven through `tracing-indicatif` progress spans), Plain (stderr status lines when piped/redirected), and Quiet (`--quiet`, silent except errors). Discovery shows a spinner with a running total; archiving shows a determinate bar (count + ETA); both degrade to plain lines when piped.

Logging and progress share stderr, so `init_logging` wires `tracing-indicatif` deliberately — keep these invariants if you touch it:

- Progress spans use a private tracing target (`PROGRESS_TARGET`), and the `IndicatifLayer` is filtered to that target alone, so only our spans draw bars — never dependency spans (which would otherwise show up as stray bars under `-v`).
- The `EnvFilter` (log level) is attached to the fmt layer only, not to the registry globally. That is what keeps progress independent of the log level: progress spans are `info`-level yet still render at the default `warn` level and under any `RUST_LOG`.
- Bars must be layer-managed (created via the `tracing-indicatif` span extensions), because the fmt layer writes through the layer's `get_stderr_writer()`, which suspends exactly that layer's own bars around each log line. A bar owned by a separate `MultiProgress` would be clobbered by `-v` logs. This is why the crate is used span-based rather than hand-rolling a writer.
- `init_logging` installs the subscriber with `try_init` (ignoring the already-initialized error), so repeated in-process calls from tests do not panic.

Testing limitation (hard-won): because `init_logging` installs a process-global subscriber via `try_init`, a test that installs a real subscriber would win the global slot and pollute every other test in the shared test binary. So the Interactive bar and the `-v`-keeps-the-bar-clean coexistence are **not** unit-tested — they are covered by the Plain-mode `cli_smoke` tests (per-post lines, `--quiet` silence, empty stdout) plus manual smoke: run `cargo run -- --no-media <journal>/<id>` in a real terminal to see the bar, and a piped run to confirm stdout stays empty. If you change the layering, verify the interactive bar and `-v` coexistence by hand — no automated test will catch a regression there.

## Known Limitations

- Comments on modern LiveJournal are JavaScript-loaded and absent from static HTML, so a static fetch cannot archive comment threads.
- `:lj-author:` is the journal name, not the poster — the canonical account name from `Site.entry.journal` when its `ditemid` matches (e.g. `evo_lutio`, not the `evo-lutio` subdomain spelling, and never `users` for `users.livejournal.com` posts). The real poster is captured separately as `:lj-poster:` from `Site.entry`; for personal journals the poster equals the journal name.
- Image `alt` text is not comma-safe: a comma in an `alt` shifts the display label across an AsciiDoc attribute boundary, for both block images (`image::`) and inline linked images (`image:…[alt,link=href]`). Only the linked-image `link=` href is comma-hardened (commas percent-encoded), so the link still resolves — this is a display-fidelity limit, not a broken link. `sanitize_macro_label` neutralizes brackets/newlines/macro-like text but leaves commas.

## Documentation Rules

**After each iteration, write hard-won knowledge into committed docs before you finish.** Assume your working context can be wiped at any moment — anything not captured in committed docs, code, or tests is lost. Whenever you learn something non-obvious (a subsystem's behavior or invariants, why an approach was chosen or rejected, a gotcha, a dependency quirk, a testing limitation, a debugging insight), record it in this file or under `docs/` as part of the same change — not after "one more task." Do not leave durable knowledge only in chat, a commit message, or agent-private memory; put it where the next contributor (human or AI) will find it. Prefer updating the relevant existing section over appending a stray note.

Do not include machine-specific paths, private source names, or one-off development context in repository docs.

Document implemented behavior only. Avoid promises about unimplemented behavior in user-facing docs.

Keep README examples copy-pasteable for normal users of the compiled binary.

Record durable project and technical knowledge — subsystem behavior, LiveJournal and domain structure, gotchas, and conventions — in this file or under `docs/` so it is versioned and shared with everyone working the repo, not in agent-private memory.
