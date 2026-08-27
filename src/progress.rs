use std::{
    io::IsTerminal,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures::channel::mpsc::Sender;
use indicatif::ProgressStyle;
use tracing::Span;
use tracing_indicatif::span_ext::IndicatifSpanExt;

/// Tracing target for progress spans only. The `IndicatifLayer` is filtered to
/// this target so it renders bars for our spans alone, never dependency spans.
pub(crate) const PROGRESS_TARGET: &str = "lj_archiver::progress";

/// A semantic progress event emitted to a channel sink for non-stderr
/// consumers (the GUI). Each variant mirrors a `Progress` method call.
#[derive(Debug, Clone, PartialEq)]
pub enum ProgressEvent {
    DiscoveryStarted,
    DiscoveryTick {
        label: String,
        found: usize,
    },
    ArchivingStarted {
        total: usize,
    },
    PostDone {
        id: u64,
        title: String,
    },
    PostSkipped {
        id: u64,
    },
    PostFailed {
        id: u64,
    },
    ArchivingFinished,
    Summary {
        archived: usize,
        skipped: usize,
        failed: usize,
        elapsed: Duration,
    },
    SingleStarted {
        id: u64,
    },
    SingleDone {
        id: u64,
        path: PathBuf,
    },
}

/// Installs the tracing subscriber. Uses `tracing-indicatif` so that, in
/// Interactive mode, `-v` log lines print above the progress bar without
/// clobbering it. Idempotent (`try_init`) so repeated in-process calls from
/// tests do not panic.
pub fn init_logging(verbose: bool, quiet: bool) {
    use tracing_indicatif::IndicatifLayer;
    use tracing_subscriber::{
        EnvFilter, Layer, filter::filter_fn, fmt, layer::SubscriberExt, util::SubscriberInitExt,
    };

    let default_level = if quiet {
        "error"
    } else if verbose {
        "debug"
    } else {
        "warn"
    };
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));

    let indicatif_layer = IndicatifLayer::new();
    let writer = indicatif_layer.get_stderr_writer();

    // The level filter is attached to the fmt layer only (it gates printed
    // logs). The indicatif layer is filtered by target instead of level, so
    // progress spans are always created regardless of RUST_LOG / verbosity.
    let _ = tracing_subscriber::registry()
        .with(fmt::layer().with_writer(writer).with_filter(env_filter))
        .with(indicatif_layer.with_filter(filter_fn(|meta| meta.target() == PROGRESS_TARGET)))
        .try_init();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Interactive,
    Plain,
    Quiet,
}

impl Mode {
    fn detect(is_terminal: bool, quiet: bool) -> Self {
        if quiet {
            Mode::Quiet
        } else if is_terminal {
            Mode::Interactive
        } else {
            Mode::Plain
        }
    }
}

#[derive(Clone)]
pub struct Progress {
    inner: Arc<Inner>,
}

struct Inner {
    mode: Mode,
    total: AtomicUsize,
    completed: AtomicUsize,
    /// The currently active progress span (discovery spinner, then archiving
    /// bar, or single-post spinner). `Some` only in Interactive mode. Dropping
    /// it (setting `None`) closes the span and clears its bar.
    span: Mutex<Option<Span>>,
    /// When set, semantic events are forwarded here (GUI channel mode).
    sink: Option<Mutex<Sender<ProgressEvent>>>,
}

impl Progress {
    pub fn detect(quiet: bool) -> Self {
        Progress::with_mode(Mode::detect(std::io::stderr().is_terminal(), quiet))
    }

    /// A progress handle that emits nothing. Used by tests and by discovery
    /// call sites that do not report progress.
    pub fn hidden() -> Self {
        Progress::with_mode(Mode::Quiet)
    }

    fn with_mode(mode: Mode) -> Self {
        Progress {
            inner: Arc::new(Inner {
                mode,
                total: AtomicUsize::new(0),
                completed: AtomicUsize::new(0),
                span: Mutex::new(None),
                sink: None,
            }),
        }
    }

    /// A progress handle that forwards semantic events to `tx` and writes
    /// nothing to stderr. Used by the GUI.
    pub fn channel(tx: Sender<ProgressEvent>) -> Self {
        Progress {
            inner: Arc::new(Inner {
                mode: Mode::Quiet,
                total: AtomicUsize::new(0),
                completed: AtomicUsize::new(0),
                span: Mutex::new(None),
                sink: Some(Mutex::new(tx)),
            }),
        }
    }

    fn emit(&self, event: ProgressEvent) {
        if let Some(tx) = &self.inner.sink {
            let _ = tx.lock().unwrap().try_send(event);
        }
    }

    pub fn discovery_started(&self) {
        self.emit(ProgressEvent::DiscoveryStarted);
        match self.inner.mode {
            Mode::Interactive => {
                let span = tracing::info_span!(target: PROGRESS_TARGET, "discovery");
                span.pb_set_style(&spinner_style());
                span.pb_set_message("discovering posts…");
                span.pb_start();
                *self.inner.span.lock().unwrap() = Some(span);
            }
            Mode::Plain => eprintln!("discovering posts…"),
            Mode::Quiet => {}
        }
    }

    pub fn discovery_tick(&self, label: &str, found: usize) {
        self.emit(ProgressEvent::DiscoveryTick {
            label: label.to_string(),
            found,
        });
        if self.inner.mode == Mode::Interactive
            && let Some(span) = self.inner.span.lock().unwrap().as_ref()
        {
            span.pb_set_message(&format!("discovering posts… {label} ({found} found)"));
        }
    }

    pub fn archiving_started(&self, total: usize) {
        self.emit(ProgressEvent::ArchivingStarted { total });
        self.inner.total.store(total, Ordering::Relaxed);
        match self.inner.mode {
            Mode::Interactive => {
                let span = tracing::info_span!(target: PROGRESS_TARGET, "archiving");
                span.pb_set_style(&bar_style());
                span.pb_set_length(total as u64);
                span.pb_start();
                // Replaces (and drops) the discovery spinner span.
                *self.inner.span.lock().unwrap() = Some(span);
            }
            Mode::Plain => eprintln!("{}", discovered_line(total)),
            Mode::Quiet => {}
        }
    }

    pub fn post_done(&self, id: u64, title: &str) {
        // Sanitize once, at the source, so every sink — the channel (a GUI) as
        // well as the terminal — receives a title free of control and bidi
        // spoofing characters, never the raw remote value.
        let title = sanitize_display_text(title);
        self.emit(ProgressEvent::PostDone {
            id,
            title: title.clone(),
        });
        let done = self.inner.completed.fetch_add(1, Ordering::Relaxed) + 1;
        match self.inner.mode {
            Mode::Interactive => self.bar_step(&format!("{id}  \"{title}\"")),
            Mode::Plain => {
                let total = self.inner.total.load(Ordering::Relaxed);
                eprintln!("{}", plain_post_line(done, total, id, &title));
            }
            Mode::Quiet => {}
        }
    }

    pub fn post_skipped(&self, id: u64) {
        self.emit(ProgressEvent::PostSkipped { id });
        let done = self.inner.completed.fetch_add(1, Ordering::Relaxed) + 1;
        match self.inner.mode {
            Mode::Interactive => self.bar_step(&format!("skipped {id}")),
            Mode::Plain => {
                let total = self.inner.total.load(Ordering::Relaxed);
                eprintln!("{}", plain_skip_line(done, total, id));
            }
            Mode::Quiet => {}
        }
    }

    pub fn post_failed(&self, id: u64) {
        self.emit(ProgressEvent::PostFailed { id });
        let done = self.inner.completed.fetch_add(1, Ordering::Relaxed) + 1;
        match self.inner.mode {
            Mode::Interactive => self.bar_step(&format!("FAILED {id}")),
            Mode::Plain => {
                let total = self.inner.total.load(Ordering::Relaxed);
                eprintln!("{}", plain_fail_line(done, total, id));
            }
            Mode::Quiet => {}
        }
    }

    /// Advance the archiving bar by one and update its trailing message.
    /// Interactive mode only. The lock is never held across an `.await`.
    fn bar_step(&self, message: &str) {
        if let Some(span) = self.inner.span.lock().unwrap().as_ref() {
            span.pb_inc(1);
            span.pb_set_message(message);
        }
    }

    pub fn archiving_finished(&self) {
        self.emit(ProgressEvent::ArchivingFinished);
        // Drop the bar span so its bar is cleared before any eprintln! output.
        *self.inner.span.lock().unwrap() = None;
    }

    pub fn summary(&self, archived: usize, skipped: usize, failed: usize, elapsed: Duration) {
        self.emit(ProgressEvent::Summary {
            archived,
            skipped,
            failed,
            elapsed,
        });
        match self.inner.mode {
            Mode::Interactive | Mode::Plain => {
                eprintln!("{}", summary_line(archived, skipped, failed, elapsed));
            }
            Mode::Quiet => {}
        }
    }

    pub fn single_started(&self, id: u64) {
        self.emit(ProgressEvent::SingleStarted { id });
        match self.inner.mode {
            Mode::Interactive => {
                let span = tracing::info_span!(target: PROGRESS_TARGET, "single");
                span.pb_set_style(&spinner_style());
                span.pb_set_message(&format!("archiving {id}…"));
                span.pb_start();
                *self.inner.span.lock().unwrap() = Some(span);
            }
            Mode::Plain => eprintln!("archiving {id}…"),
            Mode::Quiet => {}
        }
    }

    pub fn single_done(&self, id: u64, path: &Path) {
        self.emit(ProgressEvent::SingleDone {
            id,
            path: path.to_path_buf(),
        });
        *self.inner.span.lock().unwrap() = None; // clear the spinner first
        match self.inner.mode {
            Mode::Interactive | Mode::Plain => eprintln!("{}", single_done_line(id, path)),
            Mode::Quiet => {}
        }
    }
}

fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner} {msg}").expect("static template is valid")
}

fn bar_style() -> ProgressStyle {
    ProgressStyle::with_template("archiving {bar:24} {pos}/{len} ({eta}) {msg}")
        .expect("static template is valid")
}

fn plain_post_line(done: usize, total: usize, id: u64, title: &str) -> String {
    format!("[{done}/{total}] archived {id}  \"{title}\"")
}

/// Strips control characters and bidi/zero-width formatting from `text`, leaving
/// printable content intact. Medium-agnostic: no terminal escape sequence
/// survives and no bidi-spoofing or invisible character does either, so every
/// progress sink — the terminal and the semantic channel a GUI consumes — can
/// share one sanitized title.
fn sanitize_display_text(text: &str) -> String {
    text.chars()
        .filter(|character| {
            !character.is_control()
                && !matches!(
                    *character,
                    '\u{061c}'
                        | '\u{200b}'..='\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2060}'..='\u{206f}'
                        | '\u{feff}'
                )
        })
        .collect()
}

fn plain_skip_line(done: usize, total: usize, id: u64) -> String {
    format!("[{done}/{total}] skipped {id} (exists)")
}

fn plain_fail_line(done: usize, total: usize, id: u64) -> String {
    format!("[{done}/{total}] FAILED {id}")
}

fn discovered_line(total: usize) -> String {
    format!("discovered {total} posts")
}

fn single_done_line(id: u64, path: &Path) -> String {
    format!("archived {id} -> {}", path.display())
}

fn summary_line(archived: usize, skipped: usize, failed: usize, elapsed: Duration) -> String {
    format!(
        "done: {archived} archived, {skipped} skipped, {failed} failed ({})",
        format_elapsed(elapsed)
    )
}

fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs >= 60 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_detect_truth_table() {
        assert_eq!(Mode::detect(true, false), Mode::Interactive);
        assert_eq!(Mode::detect(false, false), Mode::Plain);
        // quiet wins regardless of terminal
        assert_eq!(Mode::detect(true, true), Mode::Quiet);
        assert_eq!(Mode::detect(false, true), Mode::Quiet);
    }

    #[test]
    fn plain_line_formats() {
        assert_eq!(
            plain_post_line(12, 342, 166511, "Title"),
            "[12/342] archived 166511  \"Title\""
        );
        assert_eq!(
            plain_skip_line(3, 5, 166511),
            "[3/5] skipped 166511 (exists)"
        );
        assert_eq!(plain_fail_line(4, 5, 166512), "[4/5] FAILED 166512");
        assert_eq!(discovered_line(342), "discovered 342 posts");
        assert_eq!(
            single_done_line(166511, Path::new("out/posts/166511.adoc")),
            "archived 166511 -> out/posts/166511.adoc"
        );
    }

    #[test]
    fn display_text_sanitizer_removes_controls_and_bidi_formatting_only() {
        let hostile = "safe\u{1b}]0;owned\u{7} text\u{85}\u{202e}spoof\u{2066}x\u{2069}";

        assert_eq!(sanitize_display_text(hostile), "safe]0;owned textspoofx");
    }

    #[test]
    fn summary_and_elapsed_formats() {
        assert_eq!(format_elapsed(Duration::from_secs(8)), "8s");
        assert_eq!(format_elapsed(Duration::from_secs(188)), "3m8s");
        assert_eq!(format_elapsed(Duration::from_secs(60)), "1m0s");
        assert_eq!(
            summary_line(340, 0, 2, Duration::from_secs(188)),
            "done: 340 archived, 0 skipped, 2 failed (3m8s)"
        );
    }

    #[test]
    fn drives_every_method_and_counts_completions() {
        // No subscriber is installed here, so Interactive span/pb_* calls are
        // no-ops; this exercises the control flow (locks, atomics) in every
        // mode and asserts the completed-counter advances once per post.
        for progress in [
            Progress::with_mode(Mode::Interactive),
            Progress::with_mode(Mode::Plain),
            Progress::with_mode(Mode::Quiet),
        ] {
            progress.discovery_started();
            progress.discovery_tick("/2024/", 0);
            progress.archiving_started(2);
            progress.post_done(1, "one");
            progress.post_skipped(2);
            progress.post_failed(3);
            progress.archiving_finished();
            progress.summary(1, 1, 1, Duration::from_secs(1));
            progress.single_started(9);
            progress.single_done(9, Path::new("out/posts/9.adoc"));

            // post_done + post_skipped + post_failed each advance the counter.
            assert_eq!(progress.inner.completed.load(Ordering::Relaxed), 3);
        }
    }

    #[test]
    fn channel_sink_emits_semantic_events() {
        use futures::StreamExt;

        let (tx, rx) = futures::channel::mpsc::channel(16);
        let progress = Progress::channel(tx);
        progress.discovery_started();
        progress.archiving_started(2);
        progress.post_done(166511, "One");
        progress.post_failed(166512);
        progress.archiving_finished();
        progress.summary(1, 0, 1, Duration::from_secs(3));
        drop(progress); // closes the channel so `collect` terminates

        let events: Vec<ProgressEvent> = futures::executor::block_on(rx.collect());
        assert_eq!(
            events,
            vec![
                ProgressEvent::DiscoveryStarted,
                ProgressEvent::ArchivingStarted { total: 2 },
                ProgressEvent::PostDone {
                    id: 166511,
                    title: "One".to_string()
                },
                ProgressEvent::PostFailed { id: 166512 },
                ProgressEvent::ArchivingFinished,
                ProgressEvent::Summary {
                    archived: 1,
                    skipped: 0,
                    failed: 1,
                    elapsed: Duration::from_secs(3)
                },
            ]
        );
    }

    #[test]
    fn channel_sink_sanitizes_the_post_title_before_emitting() {
        use futures::StreamExt;

        let (tx, rx) = futures::channel::mpsc::channel(16);
        let progress = Progress::channel(tx);
        // A hostile remote title carrying a terminal OSC escape and a bidi
        // override must not reach the channel sink (a future GUI renderer) raw:
        // it is sanitized at the source, once, for every sink.
        progress.post_done(7, "a\u{1b}]0;pwn\u{7}b\u{202e}c");
        drop(progress);

        let events: Vec<ProgressEvent> = futures::executor::block_on(rx.collect());
        let ProgressEvent::PostDone { title, .. } = &events[0] else {
            panic!("expected PostDone, got {:?}", events[0]);
        };
        assert!(
            !title
                .chars()
                .any(|c| c.is_control() || matches!(c, '\u{202a}'..='\u{202e}')),
            "raw control/bidi characters reached the channel sink: {title:?}"
        );
        assert_eq!(title, "a]0;pwnbc");
    }

    #[test]
    fn bounded_channel_drops_excess_without_blocking() {
        use futures::StreamExt;

        let (tx, rx) = futures::channel::mpsc::channel(1);
        let progress = Progress::channel(tx);
        for found in 0..1_024 {
            progress.discovery_tick("/2024/", found);
        }
        drop(progress);

        let events: Vec<_> = futures::executor::block_on(rx.collect());
        assert!(!events.is_empty());
        assert!(
            events.len() <= 2,
            "capacity 1 plus one sender-readiness slot must stay bounded: {events:?}"
        );
    }

    #[test]
    fn dropped_bounded_receiver_is_harmless() {
        let (tx, rx) = futures::channel::mpsc::channel(1);
        let progress = Progress::channel(tx);
        drop(rx);

        progress.discovery_started();
        progress.discovery_tick("/2024/", 1);
        progress.archiving_started(1);
        progress.post_done(1, "One");
        progress.archiving_finished();

        assert_eq!(progress.inner.completed.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn cloned_progress_handles_share_one_bounded_sender_budget() {
        use futures::StreamExt;

        let (tx, rx) = futures::channel::mpsc::channel(1);
        let progress = Progress::channel(tx);
        let producers = vec![progress.clone(); 32];
        for (found, producer) in producers.iter().enumerate() {
            producer.discovery_tick("/2024/", found);
        }
        drop(producers);
        drop(progress);

        let events: Vec<_> = futures::executor::block_on(rx.collect());
        assert!(!events.is_empty());
        assert!(
            events.len() <= 2,
            "Progress clones must share capacity 1 plus one sender-readiness slot: {events:?}"
        );
    }
}
