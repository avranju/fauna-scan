//! Service supervision, shutdown handling, and signal management.
//!
//! The service owns one cancellation token and supervises both primary
//! pipelines.  Cancellation is deliberately small and cooperative: durable
//! leases remain the recovery mechanism for work which cannot finish before
//! the shutdown deadline.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing;

use crate::database::repository::DatabaseOps;
use crate::error::{AppError, AppResult, ErrorCategory};

static PANIC_HOOK_INSTALLED: Once = Once::new();

/// Install the service panic hook.
///
/// Panic payloads can contain classifier responses, credentials, or other
/// request data. The default Rust hook prints those payloads before Tokio can
/// convert a task panic into an `AppError`, so the service installs a hook
/// which reports only that a panic occurred.
pub fn install_sanitized_panic_hook() {
    PANIC_HOOK_INSTALLED.call_once(|| {
        std::panic::set_hook(Box::new(|_| {
            eprintln!("fatal: a service task panicked; panic details suppressed");
        }));
    });
}

/// Shared cooperative cancellation state.
#[derive(Clone, Debug)]
pub struct ShutdownToken {
    cancelled: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl ShutdownToken {
    /// Create a token in the active state.
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Request cancellation.  Repeated requests are harmless.
    pub fn cancel(&self) {
        if !self.cancelled.swap(true, Ordering::SeqCst) {
            self.notify.notify_waiters();
        } else {
            // A waiter may have been created after the first notification.
            // Waking waiters again makes repeated cancellation race-safe.
            self.notify.notify_waiters();
        }
    }

    /// Return whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Wait until cancellation is requested.
    pub async fn cancelled(&self) {
        // Register before checking the flag.  If cancel() runs between the
        // check and await, Notify retains the wakeup for this waiter.
        let notified = self.notify.notified();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

impl Default for ShutdownToken {
    fn default() -> Self {
        Self::new()
    }
}

/// Reason for a normal service shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownReason {
    SigInt,
    SigTerm,
}

impl std::fmt::Display for ShutdownReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::SigInt => "SIGINT",
            Self::SigTerm => "SIGTERM",
        })
    }
}

/// Tunable lifecycle timing policy.
#[derive(Debug, Clone, Copy)]
pub struct ServiceLifecycleOptions {
    pub shutdown_timeout: Duration,
    pub summary_interval: Duration,
}

impl Default for ServiceLifecycleOptions {
    fn default() -> Self {
        Self {
            shutdown_timeout: Duration::from_secs(30),
            summary_interval: Duration::from_secs(5 * 60),
        }
    }
}

/// Wait for either of the signals handled by the service.
pub async fn wait_for_shutdown_signal() -> AppResult<ShutdownReason> {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut terminate = tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate(),
        )
        .map_err(|_| {
            AppError::new(
                ErrorCategory::Shutdown,
                "wait_for_shutdown_signal",
                "failed to install SIGTERM handler",
            )
        })?;

        tokio::pin!(ctrl_c);
        tokio::select! {
            result = &mut ctrl_c => result.map(|_| ShutdownReason::SigInt).map_err(|_| {
                AppError::new(ErrorCategory::Shutdown, "wait_for_shutdown_signal", "failed to wait for SIGINT")
            }),
            result = terminate.recv() => {
                if result.is_some() {
                    Ok(ShutdownReason::SigTerm)
                } else {
                    Err(AppError::new(ErrorCategory::Shutdown, "wait_for_shutdown_signal", "SIGTERM handler closed"))
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        ctrl_c.await.map(|_| ShutdownReason::SigInt).map_err(|_| {
            AppError::new(
                ErrorCategory::Shutdown,
                "wait_for_shutdown_signal",
                "failed to wait for SIGINT",
            )
        })
    }
}

/// Type alias useful to callers which need to box an injected signal future.
pub type ShutdownSignalFuture = Pin<Box<dyn Future<Output = AppResult<ShutdownReason>> + Send>>;

fn task_exit(
    name: &'static str,
    result: Result<AppResult<()>, tokio::task::JoinError>,
) -> AppResult<()> {
    match result {
        Ok(Ok(())) => Err(AppError::new(
            ErrorCategory::Internal,
            "supervise_service",
            format!("{name} pipeline terminated unexpectedly"),
        )),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(AppError::new(
            ErrorCategory::Internal,
            "supervise_service",
            format!("{name} pipeline task failed"),
        )),
    }
}

async fn drain_task(handle: &mut JoinHandle<AppResult<()>>, timeout: Duration) -> bool {
    match tokio::time::timeout(timeout, &mut *handle).await {
        Ok(Ok(_)) => true,
        Ok(Err(_)) => true,
        Err(_) => {
            handle.abort();
            let _ = handle.await;
            false
        }
    }
}

async fn drain_two(
    downloader: &mut JoinHandle<AppResult<()>>,
    scanner: &mut JoinHandle<AppResult<()>>,
    timeout: Duration,
) -> bool {
    let result = tokio::time::timeout(timeout, async {
        let _ = tokio::join!(&mut *downloader, &mut *scanner);
    })
    .await;
    if result.is_ok() {
        true
    } else {
        downloader.abort();
        scanner.abort();
        let _ = tokio::join!(downloader, scanner);
        false
    }
}

/// Supervise the downloader and scanner primary tasks.
///
/// The signal future is injectable so lifecycle behavior can be tested
/// without sending an operating-system signal.
pub async fn supervise_service<S>(
    mut downloader: JoinHandle<AppResult<()>>,
    mut scanner: JoinHandle<AppResult<()>>,
    shutdown: ShutdownToken,
    database: DatabaseOps,
    options: ServiceLifecycleOptions,
    signal: S,
) -> AppResult<()>
where
    S: Future<Output = AppResult<ShutdownReason>> + Send,
{
    install_sanitized_panic_hook();
    let mut signal = Box::pin(signal);
    let mut summary_tick =
        tokio::time::interval(options.summary_interval.max(Duration::from_millis(1)));
    // Do not emit a summary immediately at startup; the startup log is the
    // initial health indication.
    summary_tick.tick().await;

    loop {
        tokio::select! {
            signal_result = &mut signal => {
                match signal_result {
                    Ok(reason) => {
                        tracing::info!(reason = %reason, "Shutdown signal received");
                        shutdown.cancel();
                        let graceful = drain_two(&mut downloader, &mut scanner, options.shutdown_timeout).await;
                        if graceful {
                            tracing::info!(reason = %reason, "Graceful shutdown completed");
                        } else {
                            tracing::warn!(reason = %reason, "Shutdown deadline reached; remaining pipelines aborted");
                        }
                        return Ok(());
                    }
                    Err(error) => {
                        shutdown.cancel();
                        let _ = drain_two(&mut downloader, &mut scanner, options.shutdown_timeout).await;
                        return Err(error);
                    }
                }
            }
            result = &mut downloader => {
                let exit = task_exit("downloader", result);
                if shutdown.is_cancelled() {
                    let _ = drain_task(&mut scanner, options.shutdown_timeout).await;
                    return Ok(());
                }
                tracing::error!(pipeline = "downloader", "Primary pipeline terminated");
                shutdown.cancel();
                let _ = drain_task(&mut scanner, options.shutdown_timeout).await;
                return exit;
            }
            result = &mut scanner => {
                let exit = task_exit("scanner", result);
                if shutdown.is_cancelled() {
                    let _ = drain_task(&mut downloader, options.shutdown_timeout).await;
                    return Ok(());
                }
                tracing::error!(pipeline = "scanner", "Primary pipeline terminated");
                shutdown.cancel();
                let _ = drain_task(&mut downloader, options.shutdown_timeout).await;
                return exit;
            }
            _ = summary_tick.tick() => {
                match database.operational_summary().await {
                    Ok(summary) => tracing::info!(
                        cameras_active = summary.cameras_active,
                        images_discovered = summary.images_discovered,
                        images_downloaded = summary.images_downloaded,
                        downloads_pending = summary.downloads_pending,
                        images_awaiting_classification = summary.images_awaiting_classification,
                        classifications_completed = summary.classifications_completed,
                        retryable_failures = summary.retryable_failures,
                        permanent_failures = summary.permanent_failures,
                        "Operational summary"
                    ),
                    Err(error) => tracing::warn!(category = %error.category, operation = error.operation, "Operational summary query failed"),
                }
            }
        }
    }
}

/// Run one continuous pipeline until a signal or a pipeline failure.
pub async fn run_single_pipeline_until_signal<F, S>(
    name: &'static str,
    pipeline: F,
    shutdown: ShutdownToken,
    signal: S,
    shutdown_timeout: Duration,
) -> AppResult<()>
where
    F: Future<Output = AppResult<()>> + Send + 'static,
    S: Future<Output = AppResult<ShutdownReason>> + Send,
{
    install_sanitized_panic_hook();
    let mut task = tokio::spawn(pipeline);
    let mut signal = Box::pin(signal);
    tokio::select! {
        signal_result = &mut signal => {
            let reason = match signal_result {
                Ok(reason) => reason,
                Err(error) => {
                    shutdown.cancel();
                    let _ = drain_task(&mut task, shutdown_timeout).await;
                    return Err(error);
                }
            };
            tracing::info!(pipeline = name, reason = %reason, "Shutdown signal received");
            shutdown.cancel();
            if drain_task(&mut task, shutdown_timeout).await {
                tracing::info!(pipeline = name, reason = %reason, "Graceful shutdown completed");
            } else {
                tracing::warn!(pipeline = name, reason = %reason, "Shutdown deadline reached; pipeline aborted");
            }
            Ok(())
        }
        result = &mut task => {
            let exit = task_exit(name, result);
            if shutdown.is_cancelled() {
                Ok(())
            } else {
                shutdown.cancel();
                exit
            }
        }
    }
}
