# lj-archiver

`lj-archiver` archives public LiveJournal journals and posts into local AsciiDoc files.

It can archive a full public journal or a single public post. By default it includes comments, downloads inline media, and writes one `.adoc` file per post.

## Install

Install from the repository checkout with Cargo:

```sh
cargo install --path .
```

You can also build a release binary without installing it:

```sh
cargo build --release
```

The binary will be written to:

```text
target/release/lj-archiver
```

## Usage

Archive a full public journal:

```sh
lj-archiver news
```

Archive one post by journal and post ID:

```sh
lj-archiver news/166511
```

Archive one post by URL:

```sh
lj-archiver https://news.livejournal.com/166511.html
```

Archive a bare post ID with an explicit journal:

```sh
lj-archiver --journal news @166511
```

Write the archive to a specific directory:

```sh
lj-archiver --output-dir ./news-archive news
```

Skip comments:

```sh
lj-archiver --no-comments news
```

Leave media URLs remote instead of downloading media:

```sh
lj-archiver --no-media news
```

Resume a full journal archive without refetching validated posts that already exist:

```sh
lj-archiver --resume news
```

Increase or reduce concurrent post processing for journal archives:

```sh
lj-archiver --concurrency 2 news
```

Send a custom `User-Agent` header (by default no `User-Agent` is sent):

```sh
lj-archiver --user-agent "my-archiver/1.0" news
```

## GUI

A desktop GUI is available alongside the command-line tool. It lets you enter a
target (a journal name, `journal/id`, or a post URL), choose an output folder,
toggle comments and media, optionally generate a single-file `book.adoc`, watch
folder validation and archiving progress, and open the finished archive folder.
Folder validation runs in the background and appears as a visible validating
stage. It shows a count
warning when archive pages fail, lists failed posts with clickable links, and
lists integrity warnings such as preserved post bytes or unavailable trusted
link metadata in the final completion view.
Choose either a truly empty folder or a valid existing archive for the same
journal; the GUI automatically resumes a matching archive.
Delayed updates from an earlier run cannot change a later run. Intermediate
progress updates are bounded and may be dropped when the GUI is busy, but the
final result supplies the exact counts, failure details, and integrity warnings.
If opening the archive folder or a failed-post link does not launch, the GUI
shows a non-terminal error without replacing those completion details.

Download an installer for your platform from the project's GitHub Releases:

- **macOS (Apple Silicon):** `.dmg`
- **Linux:** `.deb` or AppImage
- **Windows:** installer (`.exe`) or `.msi`

The installers are **unsigned**. On macOS, right-click the app and choose *Open*
the first time to bypass Gatekeeper. On Windows, choose *More info → Run anyway*
if SmartScreen appears.

To run the GUI from source: `cargo run -p lj-archiver-gui`.

## Targets

`TARGET` can be one of:

| Form | Meaning |
| --- | --- |
| `news` | Archive the public `news` journal |
| `news/166511` | Archive one post from a journal |
| `@166511` with `--journal news` | Archive one post when the journal is provided separately |
| `https://news.livejournal.com/166511.html` | Archive one post by URL |

LiveJournal targets are always fetched over HTTPS. A bare journal name, a
`*.livejournal.com` hostname, and the `journal/id` and `@id` shorthands all
generate `https://` URLs, and an explicit `http://` LiveJournal URL is rejected
rather than silently upgraded, so a copied plaintext link can never send a
request in the clear.

Journal names that differ only by underscores and hyphens refer to the same
public journal.

LiveJournal domain targets may include one trailing DNS root-label dot; it is
removed before requests so the age-affirmation cookies match the canonical host.

## Options

| Option | Description |
| --- | --- |
| `--output-dir <DIR>` | Write the archive to `DIR` instead of `archive/<journal>`. A nonempty directory must be a valid `lj-archiver` archive for the requested journal |
| `--no-comments` | Do not include comments |
| `--no-media` | Do not download inline media; keep remote URLs in the output |
| `--concurrency <N>` | Set the number of posts processed concurrently during journal archives. Defaults to `4` |
| `--resume` | During full journal archives, do not refetch discovered posts present in the validated existing archive; trusted metadata may still upgrade their local links |
| `--book` | Also write `book.adoc` at the archive root: one AsciiDoc document that `include::`s `index.adoc` and every archived post (`leveloffset=+1`), under `:doctype: book` with a generated `:toc:` |
| `--journal <NAME>` | Journal name for bare `@id` targets |
| `--user-agent <STRING>` | Send this `User-Agent` header on every request. By default no `User-Agent` is sent |
| `-v`, `--verbose` | Enable more detailed logging |
| `-q`, `--quiet` | Suppress progress output; print only errors |
| `-h`, `--help` | Show command help |
| `-V`, `--version` | Show version |

## Archive Layout

By default, a journal archive is written under `archive/<journal>/`:

```text
archive/<journal>/
  index.adoc
  posts/
    166511.adoc
  assets/
    1d3802f42770.jpg
```

`index.adoc` is a table of contents that links to archived posts.

New posts are written to `posts/<id>.adoc`. When updating a validated existing
archive, a positive numeric filename spelling such as `01.adoc` is preserved
consistently for replacement, link repair, index/book entries, and local text or
image links. The post file contains document metadata, the post body, and
comments when comment archiving is enabled.

`.lj-archiver/links/*.json` is implementation-owned metadata used to upgrade
cross-references safely when a post is retained or skipped by `--resume`. Keep
this directory with `posts/` if you want those upgrades on later runs. Each
post file remains independently usable without its metadata. Older, missing,
stale, or user-invalidated metadata never triggers a broad scan of the post
source: the post stays unchanged and the run reports an integrity warning.

During a completed journal export, links to other exported posts become local
cross-references. Other root-relative, path-relative, and protocol-relative
HTTP(S) references are resolved against the individual post's delivered final
URL and written as absolute external targets. Non-HTTP(S) link and image
schemes are kept as inert visible text rather than active AsciiDoc macros.
Plain text from posts and comments is also kept literal when it resembles an
AsciiDoc macro, attribute, heading, list (including hyphen lists and
description lists), comment, page/thematic break, block title, open-block or
table delimiter, or other active syntax. Explicit non-HTTP(S) image schemes
are never emitted as image macros.
Unsupported HTML is preserved as raw HTML only when every element, attribute,
and URL belongs to a reviewed inert subset. Active HTML and custom, namespaced,
or otherwise unknown capabilities remain visible in source listings instead of
being emitted as raw HTML. Preserved HTML is always the post's original bytes,
never a re-serialization of the parsed document, so markup the archive cannot
delimit in the original — an element the post never closes — also stays a
visible source listing rather than being emitted with parser-invented tags.
Post text that would read as an AsciiDoc preprocessor directive — a line
starting with `include::`, `ifdef::`, `ifndef::`, `ifeval::`, or `endif::` — is
escaped everywhere it is preserved, in source listings and raw HTML alike;
otherwise converting an archive would let a post read a local file into the
output, or silently discard the rest of the document.
Inline code and preserved source may use deliberate inert AsciiDoc encodings;
when converted, the visible result retains the text authored in the post.
Archive paths written into AsciiDoc always use portable forward slashes.
An image that links to another exported post also stays local: its click target
resolves correctly both from a standalone file in `posts/` and when that post
is included in `book.adoc`.

Downloaded inline media is stored in a single `assets/` folder at the archive root. Each file is named by a hash of its source URL (`<hash>.<ext>`), so an image reused across posts is stored only once, and post files reference it with a relative path like `../assets/1d3802f42770.jpg`. Recognized image suffixes are normalized to a safe allowlist; unknown or unsafe suffixes become `.bin`.

### Single-file book (`--book`)

With `--book`, `lj-archiver` also writes a `book.adoc` at the archive root that
assembles the whole archive into one AsciiDoc document. It `include::`s
`index.adoc` and every archived post with `leveloffset=+1`, under `:doctype: book`
with a generated `:toc:`:

```adoc
= <journal> Archive
:doctype: book
:toc:
:lj-archiver-book:
:imagesdir: posts
:lj-archiver-post-link-prefix: posts/

include::index.adoc[leveloffset=+1]

include::posts/166511.adoc[leveloffset=+1]
```

After a book has been created, later successful updates regenerate it even when
`--book` is omitted, so it cannot become stale. Otherwise book generation
remains opt-in. A regenerated book (including during `--resume`) lists exactly
the posts `index.adoc` lists. The `:imagesdir: posts` attribute preserves
the `../assets/...` references used by included standalone posts. The internal
post-link prefix similarly preserves click targets on images that link to
other archived posts.

Tables, embedded frames, and other structures the converter preserves as raw
HTML have their links and image sources localized for wherever they are read.
A link to another archived post of the same journal becomes `<id>.html` in a
standalone post page and `posts/<id>.html` in the assembled book; a downloaded
image stays `../assets/<name>` standalone and becomes `assets/<name>` in the
book. Both variants are written into the post file and `book.adoc` selects
between them with the `lj-archiver-book` attribute above, so converting either
file resolves to the same asset on disk. Relative and protocol-relative links to
anything not in the archive become absolute `https://` URLs.

To keep downloaded media and those post targets resolvable, run the converter
from the archive root and leave its output beside `book.adoc`:

```sh
cd archive/news
asciidoctor book.adoc
# Or: asciidoctor-pdf book.adoc
```

If `--output-dir` selected a different archive root, change into that directory
instead. Arbitrary converter output directories outside the archive are not
guaranteed to preserve relative media paths.

## Behavior

Before using a nonempty output directory, `lj-archiver` verifies that it is a
controlled archive whose `index.adoc` identifies the requested journal (with
the normal hyphen/underscore aliasing). Missing or malformed archive identity,
a different journal, or malformed public post files cause the run to fail
before networking or writes. A missing or truly empty directory can start a new
archive.

Before writing a fetched post, `lj-archiver` requires its parsed post ID to
match the requested ID. Any recognizable post ID and journal in the delivered
final URL must agree as well, preventing a redirect or returned page from being
archived under the wrong identity.

Validated existing posts seed an additive archive set for both single-post and
full-journal updates. A successful current fetch replaces or adds its post;
posts omitted by partial rediscovery, and existing posts whose refetch fails,
remain in `index.adoc` and an existing or requested `book.adoc`. Progress and
summary counters describe current-run work only, so merely retained posts are
not counted as archived or skipped. Current post and discovery-page failures
remain explicit even when the validated snapshot keeps the manifests intact.

After the final post set is known, discovered posts skipped by `--resume` may
have recorded cross-references upgraded from their trusted metadata. Posts
retained only because partial rediscovery omitted them or their current refetch
failed are not visited for an upgrade during that run. For eligible skips,
ordinary cross-references and linked-image click targets are retargeted to each
surviving post's exact validated filename; targets absent from the final set
revert to canonical external journal post URLs. Missing or invalid metadata,
or a failed upgrade, preserves the existing post unchanged and prints a
warning in the CLI and in the GUI's completion view. There is no fallback scan
for link-like text. A successfully finalized
current post always has complete external fallback targets rather than internal
staging markers.

While archiving, `lj-archiver` prints progress to standard error. On an
interactive terminal it shows a discovery spinner and then a progress bar with a
running count and ETA; when output is piped or redirected it prints plain status
lines instead (`discovered N posts`, `[i/N] archived <id>`, and a final
`done: … archived, … skipped, … failed` summary). Use `-q`/`--quiet` to suppress
progress entirely (errors are still reported). `-v`/`--verbose` keeps the
progress bar and prints debug logs above it; standard output is not used.
Remote titles are stripped of terminal control and bidi-formatting characters
only for this progress display; the archived title is not changed by that
display sanitizer.

Adult-flagged public posts are archived automatically. LiveJournal gates such
posts behind an age-affirmation interstitial; `lj-archiver` sends the
corresponding affirmation (`adult_explicit=1`) on every LiveJournal request so
the real post body is retrieved. This asserts on your behalf that you meet
LiveJournal's minimum age for that content. Only public posts are affected —
the tool still cannot reach friends-only or private content.

Comments are included by default when they are present in the fetched page. Use `--no-comments` to write only the post body. Note that current LiveJournal renders comment threads dynamically (JavaScript), so they are usually not present in the archived HTML — see Current Limits.

Inline media is downloaded by default. Use `--no-media` to keep images remote;
their URLs are still resolved to absolute HTTP(S) targets from the delivered
post URL. Media files larger than 50 MiB are skipped; the post is still written
with that absolute remote media URL.

If a media download cannot be used because of an HTTP or transport failure, a
redirect or address rejection, or a size limit, the post can still be written
with the resolved absolute remote media URL. Only a successful download is
referenced through the local `assets/` folder.

For full journal archives, individual post failures do not stop the entire run. Successfully archived posts are still written, and the command exits with a nonzero status if any post failed. The same resilience applies to month-page discovery: individual page failures do not stop the full journal archive, but the command exits with a nonzero status if any month page failed, matching the per-post behavior.

Page fetches retry transient 5xx responses, transport errors, and interrupted
response bodies a couple of times. Partial body bytes from a failed attempt are
discarded before retrying. Unusable media responses are not turned into broken
local paths; the post retains the resolved absolute remote target instead.

With `--resume` during full journal archives, discovered posts already present
in the validated snapshot are not refetched. Those resume-skipped posts are
eligible for recorded cross-references to be upgraded with a controlled rewrite
after the final post set is known; if no upgrade is needed, or the metadata
cannot be trusted, their bytes stay unchanged.

## Current Limits

`lj-archiver` archives public content only.

Adult-flagged public posts are included: `lj-archiver` automatically confirms
the age affirmation LiveJournal requires before serving them.

It does not log in to LiveJournal, access friends-only or private posts, publish, sync, or edit LiveJournal content, or reproduce journal themes exactly.

Comment threads are not always present in the fetched page: current LiveJournal renders comments dynamically (via JavaScript), so an archived post may have no comments. The post body, title, tags, author, poster, and publication date are captured. The poster (written to `:lj-poster:`) and the publication date (written to `:lj-date:`) come from the post page's embedded metadata. The recorded date is a best-effort publication timestamp — it reflects LiveJournal's event time for the post, which historically corresponds to the poster's local time. The author (written to `:lj-author:`) is the journal's canonical account name from the post's embedded metadata.

Fetched pages are capped at 64 MiB. Pathologically deeply-nested HTML (beyond
256 levels) is preserved as raw HTML when it passes the reviewed inert policy,
or as a visible source listing otherwise, rather than causing the archive to
fail.
