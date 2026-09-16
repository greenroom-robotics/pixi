use crate::{
    download_verify_reporter::BuildDownloadVerifyReporter,
    main_progress_bar::{MainProgressBar, Tracker},
};
use futures::{FutureExt, Stream, StreamExt};
use indicatif::MultiProgress;
use parking_lot::Mutex;
use pixi_command_dispatcher::{BackendSourceBuildSpec, reporter::BackendSourceBuildReporter};
use pixi_compute_reporters::{OperationId, OperationRegistry};
use pixi_progress::ProgressBarPlacement;
use rattler::install::Transaction;
use rattler_conda_types::{PrefixRecord, RepoDataRecord};
use std::{
    cmp::Ordering,
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use uv_configuration::initialize_rayon_once;

/// Number of trailing log lines shown on stderr when a build fails. The full
/// log is always written to disk.
const FAILED_BUILD_TAIL_LINES: usize = 30;

/// Where a build's backend output goes while the build runs.
enum BuildOutput {
    /// Printed to stderr as it arrives by a task owning the stream.
    Streamed,
    /// Held until the build finishes, then discarded or reported.
    Buffered(Box<dyn Stream<Item = String> + Unpin + Send>),
}

/// Per-build reporter state, keyed by `OperationId`.
struct BuildProgress {
    /// Bar slot in `preparing_progress_bar`.
    bar: usize,
    package: String,
    output: Option<BuildOutput>,
}

#[derive(Clone)]
pub struct SyncReporter {
    registry: Arc<OperationRegistry>,
    multi_progress: MultiProgress,
    combined_inner: Arc<Mutex<CombinedInstallReporterInner>>,
    builds: Arc<Mutex<HashMap<OperationId, BuildProgress>>>,
    /// Directory that failed builds' full logs are written to. `None` when the
    /// cache directory could not be resolved.
    build_log_dir: Option<PathBuf>,
}

impl SyncReporter {
    pub fn new(
        registry: Arc<OperationRegistry>,
        multi_progress: MultiProgress,
        progress_bar_placement: ProgressBarPlacement,
    ) -> Self {
        let combined_inner = Arc::new(Mutex::new(CombinedInstallReporterInner::new(
            multi_progress.clone(),
            progress_bar_placement,
        )));
        Self {
            registry,
            multi_progress,
            combined_inner,
            builds: Arc::new(Mutex::new(HashMap::new())),
            build_log_dir: pixi_config::get_cache_dir()
                .ok()
                .map(|cache_dir| cache_dir.join(pixi_consts::consts::BUILD_LOGS_CACHE_DIR)),
        }
    }

    pub fn clear(&self) {
        let mut inner = self.combined_inner.lock();
        inner.preparing_progress_bar.clear();
        inner.install_progress_bar.clear();
    }

    /// Creates a new InstallReporter that shares this SyncReporter instance
    pub fn create_reporter(&self) -> InstallReporter {
        let id = self
            .combined_inner
            .lock()
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        // Installing a pixi environment uses rayon. We only want to initialize the
        // rayon thread pool when we absolutely need it.
        initialize_rayon_once();

        InstallReporter {
            id: TransactionId::new(id),
            combined: Arc::clone(&self.combined_inner),
        }
    }
}

impl BackendSourceBuildReporter for SyncReporter {
    fn on_queued(&self, env: &BackendSourceBuildSpec) -> OperationId {
        // Drive the "building <pkg>" progress entry directly from the
        // backend-build event.
        let id = self.registry.allocate();
        let package = env.name.as_source().to_owned();
        let bar = self
            .combined_inner
            .lock()
            .preparing_progress_bar
            .on_build_queued(&package);
        self.builds.lock().insert(
            id,
            BuildProgress {
                bar,
                package,
                output: None,
            },
        );
        id
    }

    fn on_started(
        &self,
        id: OperationId,
        mut backend_output_stream: Box<dyn Stream<Item = String> + Unpin + Send>,
    ) {
        let stream_to_screen = tracing::event_enabled!(tracing::Level::WARN);

        let mut builds = self.builds.lock();
        let Some(build) = builds.get_mut(&id) else {
            return;
        };
        self.combined_inner
            .lock()
            .preparing_progress_bar
            .on_build_start(build.bar);

        if !stream_to_screen {
            build.output = Some(BuildOutput::Buffered(backend_output_stream));
            return;
        }

        build.output = Some(BuildOutput::Streamed);
        let progress_bar = self.multi_progress.clone();
        let package = build.package.clone();
        tokio::spawn(async move {
            while let Some(line) = backend_output_stream.next().await {
                progress_bar.suspend(|| eprintln!("[{package}] {line}"));
            }
        });
    }

    fn on_finished(&self, id: OperationId, failed: bool) {
        let Some(build) = self.builds.lock().remove(&id) else {
            return;
        };
        self.combined_inner
            .lock()
            .preparing_progress_bar
            .on_build_finished(build.bar);

        let Some(BuildOutput::Buffered(stream)) = build.output else {
            return;
        };
        if !failed {
            return;
        }

        // The backend's log sink is dropped before this callback runs, so the
        // stream yields everything it still holds without ever pending.
        let lines = drain_ready(stream);
        let log_path = self
            .build_log_dir
            .as_deref()
            .and_then(|dir| write_build_log(dir, &build.package, &lines));

        self.multi_progress.suspend(|| {
            eprintln!("build of {} failed:", build.package);
            let tail = lines.len().saturating_sub(FAILED_BUILD_TAIL_LINES);
            for line in &lines[tail..] {
                eprintln!("  {line}");
            }
            if let Some(log_path) = log_path {
                eprintln!("full log: {}", log_path.display());
            }
        });
    }
}

fn drain_ready(mut stream: Box<dyn Stream<Item = String> + Unpin + Send>) -> Vec<String> {
    let mut lines = Vec::new();
    while let Some(Some(line)) = stream.next().now_or_never() {
        lines.push(line);
    }
    lines
}

fn write_build_log(dir: &Path, package: &str, lines: &[String]) -> Option<PathBuf> {
    let path = dir.join(format!("{package}.log"));
    fs_err::create_dir_all(dir).ok()?;
    let mut contents = lines.join("\n");
    contents.push('\n');
    fs_err::write(&path, contents).ok()?;
    Some(path)
}

pub struct CombinedInstallReporterInner {
    next_id: std::sync::atomic::AtomicUsize,

    operation_link_id: HashMap<(TransactionId, usize), usize>,
    cache_entry_id: HashMap<(TransactionId, usize), usize>,

    preparing_progress_bar: BuildDownloadVerifyReporter,
    install_progress_bar: MainProgressBar<PackageWithSize>,
}

#[derive(PartialEq, Eq)]
pub struct PackageWithSize {
    pub name: String,
    pub size: u64,
}

impl Tracker for PackageWithSize {
    fn name(&self) -> &str {
        self.name.as_str()
    }

    fn size(&self) -> u64 {
        self.size
    }
}

impl Ord for PackageWithSize {
    fn cmp(&self, other: &Self) -> Ordering {
        self.size.cmp(&other.size).reverse()
    }
}

impl PartialOrd for PackageWithSize {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl CombinedInstallReporterInner {
    pub fn new(
        multi_progress: MultiProgress,
        progress_bar_placement: ProgressBarPlacement,
    ) -> Self {
        let preparing_progress_bar = BuildDownloadVerifyReporter::new(
            multi_progress.clone(),
            progress_bar_placement.clone(),
            "preparing packages".to_owned(),
        );
        let link_progress_bar = MainProgressBar::new(
            multi_progress.clone(),
            ProgressBarPlacement::After(preparing_progress_bar.progress_bar()),
            "installing".to_owned(),
        )
        .with_osc_report();

        Self {
            next_id: std::sync::atomic::AtomicUsize::new(0),
            preparing_progress_bar,
            install_progress_bar: link_progress_bar,
            operation_link_id: HashMap::new(),
            cache_entry_id: HashMap::new(),
        }
    }

    fn on_transaction_start(
        &mut self,
        id: TransactionId,
        transaction: &Transaction<PrefixRecord, RepoDataRecord>,
    ) {
        for (operation_id, operation) in transaction.operations.iter().enumerate() {
            if let Some(record) = operation
                .record_to_install()
                .or_else(|| operation.record_to_remove().map(|r| &r.repodata_record))
            {
                self.operation_link_id.insert(
                    (id, operation_id),
                    self.install_progress_bar.queued(PackageWithSize {
                        name: record.package_record.name.as_normalized().to_string(),
                        size: record.package_record.size.unwrap_or(1),
                    }),
                );
            }
            if let Some(record) = operation.record_to_install() {
                self.cache_entry_id.insert(
                    (id, operation_id),
                    self.preparing_progress_bar.on_entry_start(record),
                );
            }
        }
    }

    fn on_transaction_operation_start(&mut self, _id: TransactionId, _operation: usize) {}

    fn on_populate_cache_start(
        &mut self,
        id: TransactionId,
        operation: usize,
        _record: &RepoDataRecord,
    ) -> usize {
        *self
            .cache_entry_id
            .get(&(id, operation))
            .expect("missing operation link")
    }

    fn on_validate_start(&mut self, _id: TransactionId, cache_entry: usize) -> usize {
        self.preparing_progress_bar.on_validation_start(cache_entry);
        cache_entry
    }

    fn on_validate_complete(&mut self, _id: TransactionId, validation_id: usize) {
        self.preparing_progress_bar
            .on_validation_complete(validation_id);
    }

    fn on_download_start(&mut self, _id: TransactionId, cache_entry: usize) -> usize {
        self.preparing_progress_bar.on_download_start(cache_entry);
        cache_entry
    }

    fn on_download_progress(
        &mut self,
        _id: TransactionId,
        cache_entry: usize,
        progress: u64,
        total: Option<u64>,
    ) {
        self.preparing_progress_bar
            .on_download_progress(cache_entry, progress, total);
    }

    fn on_download_completed(&mut self, _id: TransactionId, cache_entry: usize) {
        self.preparing_progress_bar
            .on_download_complete(cache_entry);
    }

    fn on_populate_cache_complete(&mut self, _id: TransactionId, cache_entry: usize) {
        self.preparing_progress_bar.on_entry_finished(cache_entry);
    }

    fn on_unlink_start(
        &mut self,
        id: TransactionId,
        operation: usize,
        _record: &PrefixRecord,
    ) -> usize {
        if let Some(&link_id) = self.operation_link_id.get(&(id, operation)) {
            self.install_progress_bar.start(link_id)
        };
        operation
    }

    fn on_unlink_complete(&mut self, _id: TransactionId, _index: usize) {}

    fn on_link_start(
        &mut self,
        id: TransactionId,
        operation: usize,
        _record: &RepoDataRecord,
    ) -> usize {
        if let Some(&link_id) = self.operation_link_id.get(&(id, operation)) {
            self.install_progress_bar.start(link_id)
        };
        operation
    }

    fn on_link_complete(&mut self, _id: TransactionId, _index: usize) {}

    fn on_transaction_operation_complete(&mut self, id: TransactionId, operation: usize) {
        if let Some(link_id) = self.operation_link_id.remove(&(id, operation)) {
            self.install_progress_bar.finish(link_id);
        }
    }

    fn on_transaction_complete(&mut self, _id: TransactionId) {}
}

pub struct InstallReporter {
    id: TransactionId,
    combined: Arc<Mutex<CombinedInstallReporterInner>>,
}

impl rattler::install::Reporter for InstallReporter {
    fn on_transaction_start(&self, transaction: &Transaction<PrefixRecord, RepoDataRecord>) {
        self.combined
            .lock()
            .on_transaction_start(self.id, transaction)
    }

    fn on_transaction_operation_start(&self, operation: usize) {
        self.combined
            .lock()
            .on_transaction_operation_start(self.id, operation)
    }

    fn on_populate_cache_start(&self, operation: usize, record: &RepoDataRecord) -> usize {
        self.combined
            .lock()
            .on_populate_cache_start(self.id, operation, record)
    }

    fn on_validate_start(&self, cache_entry: usize) -> usize {
        self.combined.lock().on_validate_start(self.id, cache_entry)
    }

    fn on_validate_complete(&self, validate_idx: usize) {
        self.combined
            .lock()
            .on_validate_complete(self.id, validate_idx)
    }

    fn on_download_start(&self, cache_entry: usize) -> usize {
        self.combined.lock().on_download_start(self.id, cache_entry)
    }

    fn on_download_progress(&self, download_idx: usize, progress: u64, total: Option<u64>) {
        self.combined
            .lock()
            .on_download_progress(self.id, download_idx, progress, total)
    }

    fn on_download_completed(&self, download_idx: usize) {
        self.combined
            .lock()
            .on_download_completed(self.id, download_idx)
    }

    fn on_populate_cache_complete(&self, cache_entry: usize) {
        self.combined
            .lock()
            .on_populate_cache_complete(self.id, cache_entry)
    }

    fn on_unlink_start(&self, operation: usize, record: &PrefixRecord) -> usize {
        self.combined
            .lock()
            .on_unlink_start(self.id, operation, record)
    }

    fn on_unlink_complete(&self, index: usize) {
        self.combined.lock().on_unlink_complete(self.id, index)
    }

    fn on_link_start(&self, operation: usize, record: &RepoDataRecord) -> usize {
        self.combined
            .lock()
            .on_link_start(self.id, operation, record)
    }

    fn on_link_complete(&self, index: usize) {
        self.combined.lock().on_link_complete(self.id, index)
    }

    fn on_post_link_start(&self, _package_name: &str, _script_path: &str) -> usize {
        // Return a dummy index since we don't track post-link scripts
        0
    }

    fn on_post_link_complete(&self, _index: usize, _success: bool) {
        // No-op since we don't track post-link scripts
    }

    fn on_pre_unlink_start(&self, _package_name: &str, _script_path: &str) -> usize {
        // Return a dummy index since we don't track pre-unlink scripts
        0
    }

    fn on_pre_unlink_complete(&self, _index: usize, _success: bool) {
        // No-op since we don't track pre-unlink scripts
    }

    fn on_transaction_operation_complete(&self, operation: usize) {
        self.combined
            .lock()
            .on_transaction_operation_complete(self.id, operation)
    }

    fn on_transaction_complete(&self) {
        self.combined.lock().on_transaction_complete(self.id)
    }
}

/// A type-safe identifier for transactions to avoid confusion with other IDs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TransactionId(pub usize);

impl TransactionId {
    pub fn new(id: usize) -> Self {
        TransactionId(id)
    }
}
