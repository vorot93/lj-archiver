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

Resume a full journal archive by skipping post files that already exist:

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
archiving progress, and open the finished archive folder. It warns when archive
pages fail and lists them with clickable links. Choose either an empty folder or
an existing archive for the same journal; the GUI automatically resumes a
matching archive.

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

Journal names that differ only by underscores and hyphens refer to the same
public journal.

## Options

| Option | Description |
| --- | --- |
| `--output-dir <DIR>` | Write the archive to `DIR` instead of `archive/<journal>` |
| `--no-comments` | Do not include comments |
| `--no-media` | Do not download inline media; keep remote URLs in the output |
| `--concurrency <N>` | Set the number of posts processed concurrently during journal archives. Defaults to `4` |
| `--resume` | During full journal archives, skip posts whose `.adoc` files already exist |
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

Each post is written to `posts/<id>.adoc`. The post file contains document metadata, the post body, and comments when comment archiving is enabled.

During a completed journal export, links to other exported posts become local
cross-references. Links to posts that did not archive remain canonical external
links. Archive paths written into AsciiDoc always use portable forward slashes.

Downloaded inline media is stored in a single `assets/` folder at the archive root. Each file is named by a hash of its source URL (`<hash>.<ext>`), so an image reused across posts is stored only once, and post files reference it with a relative path like `../assets/1d3802f42770.jpg`.

### Single-file book (`--book`)

With `--book`, `lj-archiver` also writes a `book.adoc` at the archive root that
assembles the whole archive into one AsciiDoc document. It `include::`s
`index.adoc` and every archived post with `leveloffset=+1`, under `:doctype: book`
with a generated `:toc:`:

```adoc
= <journal> Archive
:doctype: book
:toc:
:imagesdir: posts

include::index.adoc[leveloffset=+1]

include::posts/166511.adoc[leveloffset=+1]
```

`book.adoc` is regenerated on every run (including with `--resume`) and lists
exactly the posts `index.adoc` lists. The `:imagesdir: posts` attribute preserves
the `../assets/...` references used by included standalone posts. To keep
downloaded media resolvable, run the converter from the archive root and leave
its output beside `book.adoc`:

```sh
cd archive/news
asciidoctor book.adoc
# Or: asciidoctor-pdf book.adoc
```

If `--output-dir` selected a different archive root, change into that directory
instead. Arbitrary converter output directories outside the archive are not
guaranteed to preserve relative media paths.

## Behavior

While archiving, `lj-archiver` prints progress to standard error. On an
interactive terminal it shows a discovery spinner and then a progress bar with a
running count and ETA; when output is piped or redirected it prints plain status
lines instead (`discovered N posts`, `[i/N] archived <id>`, and a final
`done: … archived, … skipped, … failed` summary). Use `-q`/`--quiet` to suppress
progress entirely (errors are still reported). `-v`/`--verbose` keeps the
progress bar and prints debug logs above it; standard output is not used.

Adult-flagged public posts are archived automatically. LiveJournal gates such
posts behind an age-affirmation interstitial; `lj-archiver` sends the
corresponding affirmation (`adult_explicit=1`) on every LiveJournal request so
the real post body is retrieved. This asserts on your behalf that you meet
LiveJournal's minimum age for that content. Only public posts are affected —
the tool still cannot reach friends-only or private content.

Comments are included by default when they are present in the fetched page. Use `--no-comments` to write only the post body. Note that current LiveJournal renders comment threads dynamically (JavaScript), so they are usually not present in the archived HTML — see Current Limits.

Inline media is downloaded by default. Use `--no-media` to leave media links pointing at their original remote URLs. Media files larger than 50 MiB are skipped; the post is still written with the original remote media URL.

If a media download cannot be used, the post can still be written with the original remote media URL.

For full journal archives, individual post failures do not stop the entire run. Successfully archived posts are still written, and the command exits with a nonzero status if any post failed. The same resilience applies to month-page discovery: individual page failures do not stop the full journal archive, but the command exits with a nonzero status if any month page failed, matching the per-post behavior.

Transient HTTP 5xx responses are retried automatically.

With `--resume` during full journal archives, existing `posts/<id>.adoc` files are not fetched or rewritten.

## Current Limits

`lj-archiver` archives public content only.

Adult-flagged public posts are included: `lj-archiver` automatically confirms
the age affirmation LiveJournal requires before serving them.

It does not log in to LiveJournal, access friends-only or private posts, publish, sync, or edit LiveJournal content, or reproduce journal themes exactly.

Comment threads are not always present in the fetched page: current LiveJournal renders comments dynamically (via JavaScript), so an archived post may have no comments. The post body, title, tags, author, poster, and publication date are captured. The poster (written to `:lj-poster:`) and the publication date (written to `:lj-date:`) come from the post page's embedded metadata. The recorded date is a best-effort publication timestamp — it reflects LiveJournal's event time for the post, which historically corresponds to the poster's local time. The author (written to `:lj-author:`) is the journal's canonical account name from the post's embedded metadata.

Fetched pages are capped at 64 MiB. Pathologically deeply-nested HTML (beyond 256 levels) is preserved as passthrough rather than causing the archive to fail.
