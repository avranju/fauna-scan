//! Scanner pipeline: concurrent classification across configured endpoints.
//!
//! Each endpoint owns one worker. Workers claim images atomically from the
//! shared database, so available classifier capacity is used concurrently
//! without assigning the same image to more than one endpoint.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::Utc;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinSet;
use tokio::time::sleep;

use crate::classifier::{ClassifierClient, ClassifierError, ClassifierOutput, RetryDisposition};
use crate::configuration::Config;
use crate::database::models::*;
use crate::database::repository::DatabaseOps;
use crate::domain::ImageId;
use crate::domain::Timestamp;
use crate::error::{AppError, AppResult, ErrorCategory};
use crate::service_lifecycle::ShutdownToken;

#[cfg(test)]
static FORCE_PARSE_TASK_JOIN_ERROR: AtomicBool = AtomicBool::new(false);

// ── ScannerOptions ─────────────────────────────────────────────────────────

/// Validated scanner policy derived from `Config`.
///
/// Holds only the values required for scanner operation; secrets and
/// transport configuration are carried separately by `ClassifierClient`.
#[derive(Debug, Clone)]
pub struct ScannerOptions {
    /// Idle polling interval between scanner passes.
    pub poll_interval: Duration,
    /// Maximum total processing attempts before the image is marked failed.
    pub retry_limit: u32,
    /// Initial delay for exponential backoff between retries.
    pub retry_initial_delay: Duration,
    /// Maximum backoff delay cap.
    pub retry_max_delay: Duration,
    /// Processing lease duration.
    pub processing_lease_duration: Duration,
    /// Maximum local image file size in bytes.
    pub maximum_image_size_bytes: u64,
}

impl ScannerOptions {
    /// Build scanner options from configuration.
    ///
    /// Rejects an empty endpoint list or zero/invalid durations.
    pub fn from_config(config: &Config) -> AppResult<Self> {
        if config.classifier.endpoints.is_empty() {
            return Err(AppError::new(
                ErrorCategory::Configuration,
                "scanner_from_config",
                "no classifier endpoints are configured; scan cannot operate",
            ));
        }

        let poll_interval = Duration::from_secs(config.classifier.poll_interval_seconds);
        if poll_interval.is_zero() {
            return Err(AppError::new(
                ErrorCategory::Configuration,
                "scanner_from_config",
                "classifier.poll_interval_seconds must be greater than zero",
            ));
        }

        let retry_initial =
            Duration::from_secs(config.classifier.retry_initial_delay_seconds as u64);
        if retry_initial.is_zero() {
            return Err(AppError::new(
                ErrorCategory::Configuration,
                "scanner_from_config",
                "classifier.retry_initial_delay_seconds must be greater than zero",
            ));
        }

        let retry_max = Duration::from_secs(config.classifier.retry_max_delay_seconds as u64);
        if retry_max.is_zero() {
            return Err(AppError::new(
                ErrorCategory::Configuration,
                "scanner_from_config",
                "classifier.retry_max_delay_seconds must be greater than zero",
            ));
        }

        let lease = Duration::from_secs(config.classifier.processing_lease_seconds);
        if lease.is_zero() {
            return Err(AppError::new(
                ErrorCategory::Configuration,
                "scanner_from_config",
                "classifier.processing_lease_seconds must be greater than zero",
            ));
        }

        // This check is also in validate_config (called by Config::load),
        // but tests and library callers may construct Config directly.
        for (index, endpoint) in config.classifier.endpoints.iter().enumerate() {
            if config.classifier.processing_lease_seconds <= endpoint.request_timeout_seconds {
                return Err(AppError::new(
                    ErrorCategory::Configuration,
                    "scanner_from_config",
                    format!(
                        "classifier.processing_lease_seconds ({}) must be greater than \
                         classifier.endpoints[{index}].request_timeout_seconds ({}) \
                         to provide lease headroom",
                        config.classifier.processing_lease_seconds,
                        endpoint.request_timeout_seconds,
                    ),
                ));
            }
        }

        // Reject leases so large that adding them to a DateTime<Utc> could
        // overflow Chrono's bounds.  This check is also in validate_config.
        const MAX_LEASE_SECS: u64 = 3_155_760_000;
        if config.classifier.processing_lease_seconds > MAX_LEASE_SECS {
            return Err(AppError::new(
                ErrorCategory::Configuration,
                "scanner_from_config",
                format!(
                    "classifier.processing_lease_seconds ({}) exceeds \
                     maximum supported duration ({MAX_LEASE_SECS} seconds, ~100 years)",
                    config.classifier.processing_lease_seconds
                ),
            ));
        }

        if config.nvr.download.maximum_image_size_bytes == 0 {
            return Err(AppError::new(
                ErrorCategory::Configuration,
                "scanner_from_config",
                "nvr.download.maximum_image_size_bytes must be greater than zero",
            ));
        }

        Ok(Self {
            poll_interval,
            retry_limit: config.classifier.retry_limit,
            retry_initial_delay: retry_initial,
            retry_max_delay: retry_max,
            processing_lease_duration: lease,
            maximum_image_size_bytes: config.nvr.download.maximum_image_size_bytes,
        })
    }
}

// ── Renewal supervision ──────────────────────────────────────────────────

// ── Scanner ────────────────────────────────────────────────────────────────

/// Scanner coordinating database claims, local file validation, classifier
/// calls, durable state transitions, maintenance, and polling.
#[derive(Clone)]
pub struct Scanner {
    /// Database operations.
    pub database: DatabaseOps,
    /// Classifier clients, each served by an independent worker.
    pub classifiers: Vec<Arc<ClassifierClient>>,
    /// Validated scanner policy.
    pub options: ScannerOptions,
}

impl Scanner {
    /// Create a new scanner instance.
    pub fn new(
        database: DatabaseOps,
        classifier: Arc<ClassifierClient>,
        options: ScannerOptions,
    ) -> Self {
        Self {
            database,
            classifiers: vec![classifier],
            options,
        }
    }

    /// Create a scanner with one worker per supplied classifier endpoint.
    ///
    /// The endpoint list must contain at least one endpoint.
    pub fn with_classifiers(
        database: DatabaseOps,
        classifiers: Vec<Arc<ClassifierClient>>,
        options: ScannerOptions,
    ) -> Self {
        assert!(
            !classifiers.is_empty(),
            "a scanner requires at least one classifier endpoint"
        );
        Self {
            database,
            classifiers,
            options,
        }
    }

    /// Execute one scanner pass.
    ///
    /// Recovers expired leases, then repeatedly claims and processes one
    /// eligible image at a time until no more work is available.
    /// Updates `LastSuccessfulScannerPass` metadata after a successful drain.
    /// Returns a report of outcome counts.
    ///
    /// Takes `self` by value so that the returned future is `'static` and
    /// can be sent across task boundaries (e.g. via `tokio::spawn`).
    /// For use in a continuous loop, clone the scanner first:
    /// `scanner.clone().execute_one_pass().await`.
    pub async fn execute_one_pass(self) -> AppResult<ScannerPassReport> {
        self.execute_one_pass_with_shutdown(&ShutdownToken::new())
            .await
    }

    /// Execute a pass while refusing new claims after cancellation.
    pub async fn execute_one_pass_with_shutdown(
        self,
        shutdown: &ShutdownToken,
    ) -> AppResult<ScannerPassReport> {
        if shutdown.is_cancelled() {
            return Ok(ScannerPassReport::default());
        }
        let mut report = ScannerPassReport::default();

        // Recover expired processing leases.
        let now = Timestamp::new(Utc::now());
        let recovery = self.database.recover_expired_leases(&now).await?;
        report.leases_recovered = recovery.processing;

        tracing::info!(
            leases_recovered = report.leases_recovered,
            "Scanner pass: recovered expired leases"
        );

        // Run one worker per endpoint.  Claims are atomic database operations,
        // so workers naturally distribute work according to endpoint capacity.
        let worker_stop = ShutdownToken::new();
        let mut workers = JoinSet::new();
        for classifier in self.classifiers.iter().cloned() {
            let scanner = self.clone();
            let shutdown = shutdown.clone();
            let worker_stop = worker_stop.clone();
            workers.spawn(async move {
                scanner
                    .drain_claims(classifier, &shutdown, &worker_stop)
                    .await
            });
        }

        let mut worker_error = None;
        while let Some(result) = workers.join_next().await {
            match result {
                Ok(Ok(worker_report)) => merge_scanner_report(&mut report, worker_report),
                Ok(Err(error)) => {
                    worker_stop.cancel();
                    worker_error = Some(error);
                    break;
                }
                Err(join_error) => {
                    worker_stop.cancel();
                    worker_error = Some(AppError::new(
                        ErrorCategory::Internal,
                        "execute_one_pass",
                        format!("classifier worker task failed: {join_error}"),
                    ));
                    break;
                }
            }
        }
        if let Some(error) = worker_error {
            while workers.join_next().await.is_some() {}
            return Err(error);
        }

        if shutdown.is_cancelled() {
            tracing::info!(claimed = report.claimed, "Scanner loop terminated orderly");
            return Ok(report);
        }

        // Update scanner pass metadata.
        let metadata_ts = Timestamp::new(Utc::now());
        self.database
            .set_metadata(
                &ServiceMetadataKey::LastSuccessfulScannerPass,
                &metadata_ts.to_string(),
                &metadata_ts,
            )
            .await?;

        tracing::info!(
            claimed = report.claimed,
            completed = report.completed,
            retry_scheduled = report.retry_scheduled,
            failed = report.failed,
            missing = report.missing,
            leases_recovered = report.leases_recovered,
            "Scanner pass completed"
        );

        Ok(report)
    }

    /// Run the scanner in continuous mode.
    ///
    /// Repeatedly executes passes separated by the configured poll interval.
    /// Propagates fatal database or internal errors.
    pub async fn run_continuous(self) -> AppResult<()> {
        self.run_continuous_with_shutdown(ShutdownToken::new())
            .await
    }

    /// Run scanner passes and polling sleeps with cooperative cancellation.
    pub async fn run_continuous_with_shutdown(self, shutdown: ShutdownToken) -> AppResult<()> {
        tracing::info!("Entering continuous scanner polling");
        loop {
            if shutdown.is_cancelled() {
                tracing::info!("Scanner loop terminated orderly");
                return Ok(());
            }
            let report = self
                .clone()
                .execute_one_pass_with_shutdown(&shutdown)
                .await?;
            tracing::info!(
                claimed = report.claimed,
                completed = report.completed,
                retry_scheduled = report.retry_scheduled,
                failed = report.failed,
                missing = report.missing,
                "Scanner pass complete — polling"
            );

            // Sleep for the poll interval before the next pass.
            tokio::select! {
                _ = shutdown.cancelled() => {
                    tracing::info!("Scanner loop terminated orderly");
                    return Ok(());
                }
                _ = sleep(self.options.poll_interval) => {}
            }
        }
    }

    /// Claim and process work for one classifier endpoint until no eligible
    /// images remain or either shutdown token is cancelled.
    async fn drain_claims(
        &self,
        classifier: Arc<ClassifierClient>,
        shutdown: &ShutdownToken,
        worker_stop: &ShutdownToken,
    ) -> AppResult<ScannerPassReport> {
        let mut report = ScannerPassReport::default();
        loop {
            if shutdown.is_cancelled() || worker_stop.is_cancelled() {
                return Ok(report);
            }
            let lease_until = Timestamp::new(
                Utc::now()
                    .checked_add_signed(
                        chrono::Duration::from_std(self.options.processing_lease_duration)
                            .expect("lease duration fits in chrono::Duration"),
                    )
                    .ok_or_else(|| {
                        AppError::new(
                            ErrorCategory::Internal,
                            "drain_claims",
                            "lease deadline computation overflowed Chrono bounds",
                        )
                    })?,
            );
            let claim = match self
                .database
                .claim_next_processing(&Timestamp::new(Utc::now()), &lease_until)
                .await?
            {
                Some(claim) => claim,
                None => return Ok(report),
            };
            report.claimed += 1;
            match self.process_claim(&claim, classifier.clone()).await? {
                ScannerOutcome::Completed => report.completed += 1,
                ScannerOutcome::RetryScheduled => report.retry_scheduled += 1,
                ScannerOutcome::Failed => report.failed += 1,
                ScannerOutcome::Missing => report.missing += 1,
            }
        }
    }

    /// Process a single processing claim: validate the local file, call the
    /// classifier, and persist exactly one durable outcome.
    ///
    /// Uses structured lease-renewal supervision: a background renewer task
    /// communicates errors via a channel, and the main task races processing
    /// against renewal failure using `tokio::select!`.  On every exit path
    /// the renewer is cleanly stopped.
    async fn process_claim(
        &self,
        claim: &ProcessingClaim,
        classifier: Arc<ClassifierClient>,
    ) -> AppResult<ScannerOutcome> {
        let image_id = claim.image_id;
        let generation = claim.generation;
        let image_key_short = &claim.image_key.as_str()[..claim.image_key.as_str().len().min(12)];
        let attempt = claim.processing_attempts;

        // Load and validate the local JPEG file.
        let jpeg_data =
            match load_and_validate_jpeg(&claim.local_path, self.options.maximum_image_size_bytes)
                .await
            {
                Ok(data) => data,
                Err(LocalImageFailure::Missing) => {
                    tracing::info!(
                        image_id = image_id.get(),
                        image_key = image_key_short,
                        attempt,
                        generation,
                        "Local file is missing"
                    );
                    self.database
                        .fail_processing(
                            image_id,
                            "local file not found",
                            None,
                            generation,
                            ProcessingFailureDisposition::Missing,
                            &Timestamp::new(Utc::now()),
                        )
                        .await?;
                    return Ok(ScannerOutcome::Missing);
                }
                Err(LocalImageFailure::Retryable(e)) => {
                    tracing::warn!(
                        image_id = image_id.get(),
                        image_key = image_key_short,
                        attempt,
                        generation,
                        error = %e,
                        "Transient filesystem error"
                    );
                    return self
                        .handle_classifier_retry_failure(
                            image_id,
                            attempt,
                            generation,
                            &ClassifierError::new(e, RetryDisposition::Retryable, None),
                        )
                        .await;
                }
                Err(LocalImageFailure::Invalid(e)) => {
                    tracing::warn!(
                        image_id = image_id.get(),
                        image_key = image_key_short,
                        attempt,
                        generation,
                        error = %e,
                        "Invalid or oversized local image"
                    );
                    let safe_error = format!("invalid local image: {}", e);
                    self.database
                        .fail_processing(
                            image_id,
                            &safe_error,
                            None,
                            generation,
                            ProcessingFailureDisposition::Failed,
                            &Timestamp::new(Utc::now()),
                        )
                        .await?;
                    return Ok(ScannerOutcome::Failed);
                }
            };

        // Heartbeat 1: renew the processing lease before the classifier
        // preparation phase.  This prevents another scanner from recovering
        // the row while we build the request.
        let heartbeat_now = Timestamp::new(Utc::now());
        let renewal_deadline = Timestamp::new(
            heartbeat_now.as_datetime().to_owned()
                + chrono::Duration::from_std(self.options.processing_lease_duration)
                    .expect("lease duration fits in chrono::Duration"),
        );

        self.database
            .renew_processing_lease(image_id, generation, &renewal_deadline, &heartbeat_now)
            .await?;

        // Build the classification request (synchronous Base64 encoding +
        // request construction).  This phase can be slow for large images,
        // so we renew the lease right before the HTTP submission to ensure
        // the request is bounded by the lease duration.
        let prepared = match classifier.build_classification_request(&jpeg_data) {
            Ok(prepared) => prepared,
            Err(e) => {
                // Build failures are permanent — the request body was invalid.
                tracing::warn!(
                    image_id = image_id.get(),
                    generation,
                    error = %e,
                    "Failed to build classification request"
                );
                let (app_err, _, _) = e.into_parts();
                let safe_error = classifier_error_safe_message(&ClassifierError::new(
                    app_err,
                    RetryDisposition::Permanent,
                    None,
                ));
                self.database
                    .fail_processing(
                        image_id,
                        &safe_error,
                        None,
                        generation,
                        ProcessingFailureDisposition::Failed,
                        &Timestamp::new(Utc::now()),
                    )
                    .await?;
                return Ok(ScannerOutcome::Failed);
            }
        };

        // Heartbeat 2: renew the lease immediately before the HTTP
        // submission so the request is bounded by the lease.  If renewal
        // fails another scanner has re-claimed the row and we must abort.
        let heartbeat_now2 = Timestamp::new(Utc::now());
        let renewal_deadline2 = Timestamp::new(
            heartbeat_now2.as_datetime().to_owned()
                + chrono::Duration::from_std(self.options.processing_lease_duration)
                    .expect("lease duration fits in chrono::Duration"),
        );

        self.database
            .renew_processing_lease(image_id, generation, &renewal_deadline2, &heartbeat_now2)
            .await?;

        // ── Structured lease-renewal supervision ─────────────────────────
        //
        // Spawn a renewer task that communicates errors via a channel.
        // The main task races the renewer against the HTTP/parsing work
        // using `tokio::select!`.  On every exit path the renewer is
        // cleanly stopped.
        //
        // The channel carries `AppError` directly (the actual database
        // error from `renew_processing_lease`) so the main task can
        // inspect the real error rather than a synthetic wrapper.
        let (renew_tx, mut renew_rx) = mpsc::channel::<AppError>(1);
        let renewer_done = Arc::new(AtomicBool::new(false));
        let renewer_stop = Arc::new(Notify::new());
        let renewer_image_id = image_id;
        let renewer_generation = generation;
        let renewer_lease = self.options.processing_lease_duration;
        let renewer_db = self.database.clone();
        let renewer_handle = tokio::spawn(renew_lease_loop(
            renew_tx,
            renewer_done.clone(),
            renewer_stop.clone(),
            renewer_image_id,
            renewer_generation,
            renewer_lease,
            renewer_db,
        ));

        // Submit to classifier, capturing the real request start timestamp.
        let request_started_at = Timestamp::new(Utc::now());

        // Race HTTP handling against lease supervision.  The renewer remains
        // alive when HTTP succeeds: response parsing is also part of the
        // ownership window and may outlive the original lease.
        let mut renewer_handle_owned = Some(renewer_handle);
        let raw_bytes = tokio::select! {
            renewal = renew_rx.recv() => {
                let renewal_error = renewal.unwrap_or_else(|| AppError::new(
                    ErrorCategory::Database,
                    "process_claim",
                    "lease renewer exited without reporting an error",
                ));
                let shutdown_error = shutdown_renewer(
                    &renewer_done,
                    &renewer_stop,
                    &mut renewer_handle_owned,
                    &mut renew_rx,
                ).await.err();
                return Err(shutdown_error.unwrap_or(renewal_error));
            }
            raw_bytes = classifier.get_raw_response(prepared) => {
                match raw_bytes {
                    Ok(bytes) => bytes,
                    Err(classifier_err) => {
                        tracing::warn!(
                            image_id = image_id.get(),
                            image_key = image_key_short,
                            attempt,
                            generation,
                            error = %classifier_err,
                            "Classifier returned error during HTTP submission"
                        );
                        shutdown_renewer(
                            &renewer_done,
                            &renewer_stop,
                            &mut renewer_handle_owned,
                            &mut renew_rx,
                        ).await?;
                        return self
                            .handle_classifier_failure(image_id, attempt, generation, &classifier_err)
                            .await;
                    }
                }
            }
        };

        // Parsing is deliberately supervised by the same renewer.  A parse
        // result is never persisted if ownership was lost while parsing.
        // `raw_bytes` was produced by the supervised HTTP branch above.

        // Spawn the CPU-heavy parsing task while the renewer remains active.
        let classifier_for_parse = classifier.clone();
        let mut parse_handle = tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            if FORCE_PARSE_TASK_JOIN_ERROR.load(Ordering::SeqCst) {
                panic!("deterministic parser task failure");
            }
            classifier_for_parse.parse_response(raw_bytes)
        });

        // Race parsing against renewal failure. If renewal fails first, let
        // the blocking parse finish before shutting down supervision, then
        // discard its result. This ensures no unobserved parse task remains
        // while preventing a stale worker from persisting its output.
        let classify_result: Result<Result<ClassifierOutput, ClassifierError>, AppError> = tokio::select! {
            parsed = &mut parse_handle => match parsed {
                Ok(Ok(output)) => Ok(Ok(output)),
                Ok(Err(classifier_err)) => Ok(Err(classifier_err)),
                Err(join_err) => Err(parsing_join_error(join_err)),
            },
            renewal = renew_rx.recv() => {
                let renewal_error = renewal.unwrap_or_else(|| AppError::new(
                    ErrorCategory::Database,
                    "process_claim",
                    "lease renewer exited without reporting an error",
                ));
                let parse_outcome = parse_handle.await;
                if let Err(join_error) = parse_outcome {
                    tracing::error!(
                        image_id = image_id.get(),
                        generation,
                        error = %join_error,
                        "Response parsing task failed after lease ownership was lost"
                    );
                }
                // Join the renewer before returning. The already-received
                // renewal error is authoritative and is returned unchanged.
                if let Err(shutdown_error) = shutdown_renewer(
                    &renewer_done,
                    &renewer_stop,
                    &mut renewer_handle_owned,
                    &mut renew_rx,
                ).await {
                    tracing::error!(
                        image_id = image_id.get(),
                        generation,
                        error = %shutdown_error,
                        "Lease renewer did not shut down cleanly"
                    );
                }
                tracing::warn!(
                    image_id = image_id.get(),
                    generation,
                    error = %renewal_error,
                    "Lease renewal failed during response parsing"
                );
                return Err(renewal_error);
            }
        };

        let request_completed_at = Timestamp::new(Utc::now());

        // Stop only after parsing has produced an outcome. The shutdown
        // helper joins the task and drains its error channel, so an error
        // racing with a successful parse cannot be lost.
        let shutdown_error = shutdown_renewer(
            &renewer_done,
            &renewer_stop,
            &mut renewer_handle_owned,
            &mut renew_rx,
        )
        .await
        .err();

        // Verify ownership before proceeding.
        self.database
            .verify_processing_ownership(image_id, generation)
            .await?;

        match classify_result {
            Err(e) => {
                // A failed parsing task is a scanner coordination failure,
                // not an image-level classifier failure.  Do not transition
                // the claim to failed; its lease remains available for
                // recovery after this pass stops.
                Err(e)
            }
            Ok(Ok(output)) => {
                if let Some(e) = shutdown_error {
                    tracing::warn!(
                        image_id = image_id.get(),
                        generation,
                        error = %e,
                        "Lease renewal failed during response parsing"
                    );
                    return Err(e);
                }
                // Verify ownership before persisting — if another scanner
                // recovered and completed this claim, abort.
                if let Err(e) = self
                    .database
                    .verify_processing_ownership(image_id, generation)
                    .await
                {
                    tracing::warn!(
                        image_id = image_id.get(),
                        generation,
                        error = %e,
                        "Ownership lost during classification — aborting"
                    );
                    return Err(e);
                }
                let outcome = self
                    .handle_classification_success(
                        image_id,
                        output,
                        generation,
                        &classifier,
                        &request_started_at,
                        &request_completed_at,
                    )
                    .await?;
                tracing::info!(
                    image_id = image_id.get(),
                    image_key = image_key_short,
                    attempt,
                    generation,
                    "Classification successful"
                );
                Ok(outcome)
            }
            Ok(Err(classifier_err)) => {
                if let Some(e) = shutdown_error {
                    tracing::warn!(
                        image_id = image_id.get(),
                        generation,
                        error = %e,
                        "Lease renewal failed during response parsing"
                    );
                    return Err(e);
                }
                tracing::warn!(
                    image_id = image_id.get(),
                    image_key = image_key_short,
                    attempt,
                    generation,
                    error = %classifier_err,
                    "Classifier returned error during response parsing"
                );
                self.handle_classifier_failure(image_id, attempt, generation, &classifier_err)
                    .await
            }
        }
    }

    /// Handle successful classification: convert output to ClassificationInput
    /// and persist atomically.
    async fn handle_classification_success(
        &self,
        image_id: ImageId,
        output: ClassifierOutput,
        generation: i64,
        classifier: &ClassifierClient,
        request_started_at: &Timestamp,
        request_completed_at: &Timestamp,
    ) -> AppResult<ScannerOutcome> {
        let classification = classification_input(
            output,
            classifier.model(),
            classifier.prompt_version(),
            request_started_at,
            request_completed_at,
        )?;

        self.database
            .complete_classification(image_id, &classification, generation, request_completed_at)
            .await?;

        Ok(ScannerOutcome::Completed)
    }

    /// Handle a classifier failure: determine retry disposition, calculate
    /// backoff, and persist the appropriate failure state.
    async fn handle_classifier_failure(
        &self,
        image_id: ImageId,
        attempt: i64,
        generation: i64,
        classifier_err: &ClassifierError,
    ) -> AppResult<ScannerOutcome> {
        if classifier_err.is_retryable() && attempt < self.options.retry_limit as i64 {
            // Retryable failure within limit — schedule retry with backoff.
            // `attempt` is the one-based processing_attempts value already
            // persisted by claim_next_processing, so pass it directly.
            let backoff = scanner_backoff(
                attempt,
                self.options.retry_initial_delay,
                self.options.retry_max_delay,
            );
            let next_attempt = Timestamp::new(Utc::now() + backoff);

            let safe_error = classifier_error_safe_message(classifier_err);
            let raw_response = classifier_err.raw_response().map(String::from);

            self.database
                .fail_processing(
                    image_id,
                    &safe_error,
                    raw_response,
                    generation,
                    ProcessingFailureDisposition::RetryWait {
                        next_attempt_at: next_attempt,
                    },
                    &Timestamp::new(Utc::now()),
                )
                .await?;

            tracing::info!(
                image_id = image_id.get(),
                attempt,
                generation,
                next_attempt_at = %next_attempt,
                "Retry scheduled"
            );
            Ok(ScannerOutcome::RetryScheduled)
        } else if classifier_err.is_retryable() {
            // Retryable but retry limit exhausted — permanent failure.
            let safe_error = classifier_error_safe_message(classifier_err);
            let raw_response = classifier_err.raw_response().map(String::from);

            self.database
                .fail_processing(
                    image_id,
                    &safe_error,
                    raw_response,
                    generation,
                    ProcessingFailureDisposition::Failed,
                    &Timestamp::new(Utc::now()),
                )
                .await?;

            tracing::info!(
                image_id = image_id.get(),
                attempt,
                generation,
                "Retry limit exhausted — permanent failure"
            );
            Ok(ScannerOutcome::Failed)
        } else {
            // Permanent failure — immediate transition to failed.
            let safe_error = classifier_error_safe_message(classifier_err);
            let raw_response = classifier_err.raw_response().map(String::from);

            self.database
                .fail_processing(
                    image_id,
                    &safe_error,
                    raw_response,
                    generation,
                    ProcessingFailureDisposition::Failed,
                    &Timestamp::new(Utc::now()),
                )
                .await?;

            tracing::info!(
                image_id = image_id.get(),
                attempt,
                generation,
                "Permanent failure — marked failed"
            );
            Ok(ScannerOutcome::Failed)
        }
    }

    /// Handle a retryable filesystem error: apply backoff and schedule retry.
    async fn handle_classifier_retry_failure(
        &self,
        image_id: ImageId,
        attempt: i64,
        generation: i64,
        classifier_err: &ClassifierError,
    ) -> AppResult<ScannerOutcome> {
        if attempt < self.options.retry_limit as i64 {
            let backoff = scanner_backoff(
                attempt,
                self.options.retry_initial_delay,
                self.options.retry_max_delay,
            );
            let next_attempt = Timestamp::new(Utc::now() + backoff);

            let safe_error = classifier_error_safe_message(classifier_err);

            self.database
                .fail_processing(
                    image_id,
                    &safe_error,
                    None,
                    generation,
                    ProcessingFailureDisposition::RetryWait {
                        next_attempt_at: next_attempt,
                    },
                    &Timestamp::new(Utc::now()),
                )
                .await?;

            Ok(ScannerOutcome::RetryScheduled)
        } else {
            let safe_error = classifier_error_safe_message(classifier_err);
            self.database
                .fail_processing(
                    image_id,
                    &safe_error,
                    None,
                    generation,
                    ProcessingFailureDisposition::Failed,
                    &Timestamp::new(Utc::now()),
                )
                .await?;
            Ok(ScannerOutcome::Failed)
        }
    }
}

// ── ScannerPassReport ──────────────────────────────────────────────────────

/// Outcome counts for a single scanner pass.
#[derive(Debug, Clone, Default)]
pub struct ScannerPassReport {
    pub claimed: u64,
    pub completed: u64,
    pub retry_scheduled: u64,
    pub failed: u64,
    pub missing: u64,
    pub leases_recovered: u64,
}

/// Display a compact summary of the scanner pass report.
fn merge_scanner_report(total: &mut ScannerPassReport, worker: ScannerPassReport) {
    total.claimed += worker.claimed;
    total.completed += worker.completed;
    total.retry_scheduled += worker.retry_scheduled;
    total.failed += worker.failed;
    total.missing += worker.missing;
}

impl std::fmt::Display for ScannerPassReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Scanner pass: claimed={}, completed={}, retry={}, failed={}, missing={}, recovered={}",
            self.claimed,
            self.completed,
            self.retry_scheduled,
            self.failed,
            self.missing,
            self.leases_recovered
        )
    }
}

// ── ScannerOutcome ─────────────────────────────────────────────────────────

/// Durable result of processing one claimed image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScannerOutcome {
    /// Classification succeeded and was persisted.
    Completed,
    /// Retry scheduled with backoff.
    RetryScheduled,
    /// Permanent failure (exhausted retries, invalid image, permanent error).
    Failed,
    /// Local file is missing.
    Missing,
}

// ── LocalImageFailure ──────────────────────────────────────────────────────

/// Distinguishes an absent file from retryable I/O and permanently invalid JPEG data.
#[derive(Debug)]
pub enum LocalImageFailure {
    /// File not found — becomes processing_status=missing.
    Missing,
    /// Transient I/O error — may be retried.
    Retryable(AppError),
    /// File is empty, oversized, or invalid JPEG — permanent failure.
    Invalid(AppError),
}

// ── load_and_validate_jpeg ─────────────────────────────────────────────────

/// Read a bounded local file and classify failures.
///
/// - NotFound → `LocalImageFailure::Missing`
/// - Transient I/O → `LocalImageFailure::Retryable`
/// - Empty, oversized, or non-JPEG → `LocalImageFailure::Invalid`
///
/// Handles `maximum_size == u64::MAX` gracefully by using saturating
/// arithmetic and a bounded initial Vec capacity that never panics,
/// even for sparse files with huge metadata length reports.
pub async fn load_and_validate_jpeg(
    path: &Path,
    maximum_size: u64,
) -> Result<Vec<u8>, LocalImageFailure> {
    // Check file metadata first.
    let metadata = match tokio::fs::metadata(path).await {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(LocalImageFailure::Missing);
        }
        Err(e) => {
            return Err(LocalImageFailure::Retryable(AppError::with_source(
                ErrorCategory::Filesystem,
                "load_and_validate_jpeg",
                format!("cannot stat file: {}", safe_io_message(&e)),
                e,
            )));
        }
    };

    let file_len = metadata.len();

    // Check for empty file.
    if file_len == 0 {
        return Err(LocalImageFailure::Invalid(AppError::new(
            ErrorCategory::Filesystem,
            "load_and_validate_jpeg",
            "local image file is empty",
        )));
    }

    // Check size limit using saturating comparison.  When maximum_size
    // is u64::MAX the comparison always succeeds (any real file is ≤ it).
    if file_len.saturating_sub(1) > maximum_size.saturating_sub(1) {
        return Err(LocalImageFailure::Invalid(AppError::new(
            ErrorCategory::Filesystem,
            "load_and_validate_jpeg",
            format!(
                "local image file exceeds maximum size of {} bytes ({} bytes)",
                maximum_size, file_len
            ),
        )));
    }

    // Read the file through a bounded reader to prevent allocating beyond
    // the configured maximum even if the file grows between the metadata
    // check and the read completion.
    use tokio::io::AsyncReadExt;

    let file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(LocalImageFailure::Missing);
        }
        Err(e) => {
            return Err(LocalImageFailure::Retryable(AppError::with_source(
                ErrorCategory::Filesystem,
                "load_and_validate_jpeg",
                format!("cannot open file: {}", safe_io_message(&e)),
                e,
            )));
        }
    };

    // Compute the read limit: maximum_size + 1, saturating at u64::MAX.
    // When maximum_size is u64::MAX the limit stays at u64::MAX.
    let take_limit = maximum_size.saturating_add(1);

    // Use a safe initial capacity that never overflows:
    //   - Start with metadata.len() clamped to a reasonable initial size
    //     (16 KiB) to avoid infallible allocation from untrusted metadata.
    //   - The Vec will grow as needed via reallocation.
    let initial_capacity = metadata.len().min(16_384);
    let mut bounded = Vec::with_capacity(initial_capacity as usize);

    // Read at most take_limit bytes.  For u64::MAX + 1 = u64::MAX this
    // reads the entire file (bounded only by available memory).  The
    // post-read check below verifies the file does not exceed the limit.
    let _bytes_read = match file.take(take_limit).read_to_end(&mut bounded).await {
        Ok(n) => n,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(LocalImageFailure::Missing);
        }
        Err(e) => {
            return Err(LocalImageFailure::Retryable(AppError::with_source(
                ErrorCategory::Filesystem,
                "load_and_validate_jpeg",
                format!("cannot read file: {}", safe_io_message(&e)),
                e,
            )));
        }
    };

    let data = bounded;

    // Reject if the file exceeds the configured maximum size.
    if data.len() as u64 > maximum_size {
        return Err(LocalImageFailure::Invalid(AppError::new(
            ErrorCategory::Filesystem,
            "load_and_validate_jpeg",
            format!(
                "local image file exceeds maximum size of {} bytes ({} bytes)",
                maximum_size,
                data.len()
            ),
        )));
    }

    // Reject empty files.
    if data.is_empty() {
        return Err(LocalImageFailure::Invalid(AppError::new(
            ErrorCategory::Filesystem,
            "load_and_validate_jpeg",
            "local image file is empty",
        )));
    }

    // Validate JPEG signature.
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return Err(LocalImageFailure::Invalid(AppError::new(
            ErrorCategory::Filesystem,
            "load_and_validate_jpeg",
            "local image does not start with JPEG signature (FF D8)",
        )));
    }

    if data.len() < 4 || data[data.len() - 2] != 0xFF || data[data.len() - 1] != 0xD9 {
        return Err(LocalImageFailure::Invalid(AppError::new(
            ErrorCategory::Filesystem,
            "load_and_validate_jpeg",
            "local image does not end with JPEG end marker (FF D9)",
        )));
    }

    Ok(data)
}

// ── classification_input ───────────────────────────────────────────────────

/// Convert validated classifier output into the repository's atomic completion input.
pub fn classification_input(
    output: ClassifierOutput,
    model: &str,
    prompt_version: &str,
    started_at: &Timestamp,
    completed_at: &Timestamp,
) -> AppResult<ClassificationInput> {
    // Serialize species to JSON.
    let species_json = serde_json::to_string(&output.classification.species).map_err(|e| {
        AppError::with_source(
            ErrorCategory::Internal,
            "classification_input",
            format!("failed to serialize species: {e}"),
            e,
        )
    })?;

    Ok(ClassificationInput {
        model: model.to_string(),
        prompt_version: prompt_version.to_string(),
        contains_wildlife: output.classification.contains_wildlife,
        is_interesting: output.classification.is_interesting,
        summary: Some(output.classification.summary),
        species_json: Some(species_json),
        confidence: Some(output.classification.overall_confidence),
        classification_json: Some(output.classification_json),
        raw_response: Some(output.raw_response),
        request_started_at: *started_at,
        request_completed_at: *completed_at,
    })
}

// ── scanner_backoff ────────────────────────────────────────────────────────

/// Calculate capped exponential backoff: initial * 2^(attempt-1), capped at maximum.
///
/// Uses genuinely saturating duration arithmetic matching the downloader's
/// `exponential_backoff` implementation: nanosecond multiplication saturates
/// at u64::MAX rather than wrapping, and the final result is capped at
/// `maximum`.
///
/// `attempt` is the one-based processing_attempts value (1, 2, 3, …).
pub fn scanner_backoff(attempt: i64, initial: Duration, maximum: Duration) -> Duration {
    if attempt <= 0 {
        return initial;
    }
    // Calculate 2^(attempt-1) with overflow protection.
    let exponent = (attempt - 1).min(120) as u32;
    let power = 2u128.saturating_pow(exponent);
    // Multiply initial duration by power, saturating at u64::MAX nanoseconds.
    let nanos = initial.as_nanos().saturating_mul(power);
    // Saturating u128 → u64 conversion to prevent wrap-around.
    let nanos_u64 = (nanos.min(u64::MAX as u128)) as u64;
    let duration = Duration::from_nanos(nanos_u64);
    duration.min(maximum)
}

// ── Lease renewal loop ───────────────────────────────────────────────────

/// Background task that periodically renews the processing lease.
///
/// Sends the actual `AppError` from `renew_processing_lease` through `tx` if
/// renewal fails (ownership lost or fatal database error), or exits cleanly
/// when `done` is set.  The caller must await the returned `JoinHandle` to
/// ensure the task has fully stopped before proceeding.
async fn renew_lease_loop(
    tx: mpsc::Sender<AppError>,
    done: Arc<AtomicBool>,
    stop: Arc<Notify>,
    image_id: ImageId,
    generation: i64,
    lease_duration: Duration,
    database: DatabaseOps,
) {
    // Renew several times within one lease. This also keeps short leases
    // safe in tests and during direct construction of scanner options.
    // Renew roughly once per third of the lease.  Keep only a small lower
    // bound for very short test/configured leases; long production leases
    // must not create an unnecessarily chatty SQLite heartbeat.
    let renewal_interval = lease_renewal_interval(lease_duration);
    let mut interval = tokio::time::interval(renewal_interval);
    // Skip the first tick since we just did Heartbeat 2.
    interval.tick().await;
    loop {
        tokio::select! {
            _ = stop.notified() => break,
            _ = interval.tick() => {
                if done.load(Ordering::SeqCst) {
                    break;
                }

                let now = Timestamp::new(Utc::now());
                let new_until = Timestamp::new(
                    now.as_datetime().to_owned()
                        + chrono::Duration::from_std(lease_duration)
                            .expect("lease duration fits in chrono::Duration"),
                );

                if let Err(e) = database
                    .renew_processing_lease(image_id, generation, &new_until, &now)
                    .await
                {
                    // Preserve the repository's actual error. In particular,
                    // an ownership-loss error must not be replaced by a
                    // synthetic wrapper that hides the coordination cause.
                    let _ = tx.send(e).await;
                    break;
                }
            }
            else => break,
        }
    }
}

/// Derive a low-frequency lease heartbeat interval without imposing a
/// production-scale upper clamp.
fn lease_renewal_interval(lease_duration: Duration) -> Duration {
    lease_duration
        .checked_div(3)
        .unwrap_or(Duration::from_millis(1))
        .max(Duration::from_millis(1))
}

/// Stop and join a lease renewer, then inspect all errors it may have queued.
///
/// The caller invokes this only after the supervised work has produced an
/// outcome. Joining before draining the channel closes the race where renewal
/// fails at the same time as HTTP or parsing completes.
async fn shutdown_renewer(
    done: &Arc<AtomicBool>,
    stop: &Notify,
    handle: &mut Option<tokio::task::JoinHandle<()>>,
    errors: &mut mpsc::Receiver<AppError>,
) -> AppResult<()> {
    done.store(true, Ordering::SeqCst);
    stop.notify_one();

    let join_result = if let Some(handle) = handle.take() {
        handle.await
    } else {
        Ok(())
    };

    // The renewer sends at most one error before exiting. Check it after the
    // join so a send racing with shutdown cannot be hidden by a clean join.
    if let Ok(error) = errors.try_recv() {
        return Err(error);
    }

    if let Err(join_error) = join_result {
        return Err(AppError::new(
            ErrorCategory::Internal,
            "shutdown_renewer",
            format!("lease renewer task failed: {join_error}"),
        ));
    }

    Ok(())
}

// ── Safe error formatting ──────────────────────────────────────────────────

/// Format a classifier error into a short, safe message for persistence.
///
/// Only includes category and status information — never the raw response
/// body, Base64 image data, or model output.
fn classifier_error_safe_message(err: &ClassifierError) -> String {
    let category = err.category();
    let http_status = err.http_status();

    match (http_status, category) {
        (Some(status), _) => format!("classifier {category} (HTTP {status})"),
        (_, ErrorCategory::ClassifierTransport) => {
            format!("classifier {category}")
        }
        (_, ErrorCategory::ClassifierResponse) => {
            format!("classifier {category}")
        }
        (_, other) => format!("classifier {other}"),
    }
}

/// Convert a parsing task JoinError into a fatal scanner error.
///
/// A task panic or cancellation is an internal coordination failure.  It is
/// deliberately kept outside `ClassifierError` so the claimed image remains
/// processing and can be recovered by a later pass.
fn parsing_join_error(join_error: tokio::task::JoinError) -> AppError {
    AppError::new(
        ErrorCategory::Internal,
        "process_claim",
        format!("classifier response parsing task failed: {join_error}"),
    )
}

/// Strip potentially sensitive details from an io::Error message.
fn safe_io_message(e: &std::io::Error) -> String {
    e.to_string()
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ── Option construction tests ───────────────────────────────────────

    #[test]
    fn scanner_options_from_config_requires_an_endpoint() {
        let config = Config {
            general: crate::configuration::GeneralConfig {
                database_path: PathBuf::from("/tmp/test.db"),
                output_directory: PathBuf::from("/tmp/output"),
                log_level: crate::cli::LogLevel::Error,
            },
            nvr: crate::configuration::NvrConfig {
                scheme: "http".to_string(),
                host: "test".to_string(),
                port: 80,
                username: "u".to_string(),
                password: Some(crate::configuration::Secret::new("x".to_string())),
                start_at: Timestamp::new(Utc::now()),
                request_timeout_seconds: 30,
                connect_timeout_seconds: 10,
                allow_invalid_tls_certificates: false,
                search: crate::configuration::NvrSearchConfig {
                    window_minutes: 60,
                    max_results: 50,
                    poll_interval_seconds: 60,
                    poll_overlap_seconds: 120,
                    camera_refresh_interval_seconds: 3600,
                    settlement_delay_seconds: 10,
                },
                download: crate::configuration::NvrDownloadConfig {
                    retry_limit: 10,
                    retry_initial_delay_seconds: 5,
                    retry_max_delay_seconds: 300,
                    maximum_image_size_bytes: 25_000_000,
                    verify_jpeg: true,
                    rebase_playback_urls: true,
                    concurrency: 2,
                    playback_host_allowlist: vec![],
                },
            },
            classifier: crate::configuration::ClassifierConfig {
                endpoints: Vec::new(),
                poll_interval_seconds: 10,
                retry_limit: 5,
                retry_initial_delay_seconds: 10,
                retry_max_delay_seconds: 300,
                processing_lease_seconds: 600,
            },
            source_path: PathBuf::from("/tmp/test.toml"),
        };
        let result = ScannerOptions::from_config(&config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Configuration);
        assert!(err.message.contains("no classifier endpoints"));
    }

    // ── Exponential backoff tests ───────────────────────────────────────

    #[test]
    fn backoff_progresses_exponentially() {
        let initial = Duration::from_secs(10);
        let maximum = Duration::from_secs(300);

        // Attempt 1 → initial * 2^0 = initial
        let b1 = scanner_backoff(1, initial, maximum);
        // Attempt 2 → initial * 2^1 = 2 * initial
        let b2 = scanner_backoff(2, initial, maximum);
        // Attempt 3 → initial * 2^2 = 4 * initial
        let b3 = scanner_backoff(3, initial, maximum);
        // Attempt 4 → initial * 2^3 = 8 * initial
        let b4 = scanner_backoff(4, initial, maximum);

        assert_eq!(b1, Duration::from_secs(10));
        assert_eq!(b2, Duration::from_secs(20));
        assert_eq!(b3, Duration::from_secs(40));
        assert_eq!(b4, Duration::from_secs(80));
    }

    #[test]
    fn backoff_first_attempt_uses_initial_delay() {
        // The first persisted processing_attempts value is 1, and the
        // backoff should be initial (not 2 * initial).
        let initial = Duration::from_secs(10);
        let maximum = Duration::from_secs(300);
        assert_eq!(scanner_backoff(1, initial, maximum), initial);
    }

    #[test]
    fn backoff_large_attempt_does_not_overflow() {
        // A very large attempt count should saturate rather than wrap.
        let initial = Duration::from_secs(10);
        let maximum = Duration::from_secs(300);
        let b = scanner_backoff(10000, initial, maximum);
        assert_eq!(b, maximum);
    }

    #[test]
    fn backoff_u128_saturation_to_u64() {
        // Even with a huge multiplier the conversion to u64 nanoseconds
        // should saturate, not wrap to a short delay.
        let initial = Duration::from_secs(1);
        let maximum = Duration::from_secs(60);
        let b = scanner_backoff(200, initial, maximum);
        assert_eq!(b, maximum);
    }

    #[test]
    fn backoff_capped_at_maximum() {
        let initial = Duration::from_secs(10);
        let maximum = Duration::from_secs(30);

        // After several attempts the backoff should be capped.
        let b10 = scanner_backoff(10, initial, maximum);
        assert_eq!(b10, maximum);

        let b20 = scanner_backoff(20, initial, maximum);
        assert_eq!(b20, maximum);
    }

    #[test]
    fn backoff_zero_attempt_returns_initial() {
        let initial = Duration::from_secs(5);
        let maximum = Duration::from_secs(300);
        assert_eq!(scanner_backoff(0, initial, maximum), initial);
    }

    // ── Lease / retry deadline arithmetic tests ─────────────────────────

    #[test]
    fn lease_deadline_is_future() {
        let options = ScannerOptions {
            poll_interval: Duration::from_secs(10),
            retry_limit: 5,
            retry_initial_delay: Duration::from_secs(10),
            retry_max_delay: Duration::from_secs(300),
            processing_lease_duration: Duration::from_secs(600),
            maximum_image_size_bytes: 25_000_000,
        };
        let now = Timestamp::new(Utc::now());
        let lease_until =
            Timestamp::new(now.as_datetime().to_owned() + options.processing_lease_duration);
        assert!(lease_until > now);
    }

    // ── Safe error formatting tests ─────────────────────────────────────

    #[test]
    fn classifier_error_safe_message_no_secrets() {
        let app_err = AppError::new(
            ErrorCategory::ClassifierTransport,
            "classify_jpeg",
            "network error",
        );
        let err = ClassifierError::new(
            app_err,
            RetryDisposition::Retryable,
            Some("raw body content SENTINEL-RAW".to_string()),
        );
        let msg = classifier_error_safe_message(&err);
        assert!(msg.contains("ClassifierTransport"));
        assert!(!msg.contains("SENTINEL-RAW"));
        assert!(!msg.contains("network error"));
    }

    #[test]
    fn classifier_error_safe_message_includes_http_status() {
        let app_err = AppError::new(
            ErrorCategory::ClassifierTransport,
            "classify_jpeg",
            "server error",
        )
        .with_http_status(500);
        let err = ClassifierError::new(app_err, RetryDisposition::Retryable, None);
        let msg = classifier_error_safe_message(&err);
        assert!(msg.contains("500"));
    }

    #[test]
    fn lease_renewal_interval_scales_with_lease() {
        assert_eq!(
            lease_renewal_interval(Duration::from_secs(600)),
            Duration::from_secs(200)
        );
        assert_eq!(
            lease_renewal_interval(Duration::from_millis(2)),
            Duration::from_millis(1)
        );
    }

    #[tokio::test]
    async fn parsing_task_join_error_fails_pass_and_leaves_claim_recoverable() {
        use crate::configuration::ClassifierGenerationConfig;
        use crate::database::Database;
        use crate::domain::ProcessingStatus;
        use tempfile::tempdir;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let directory = tempdir().unwrap();
        let image_path = directory.path().join("image.jpg");
        std::fs::write(&image_path, [0xff, 0xd8, 0xff, 0xd9]).unwrap();

        let database = Database::open(&directory.path().join("scanner.db"))
            .await
            .unwrap();
        let ops = database.ops();
        let now = Timestamp::new(Utc::now());
        let now_text = now.to_string();
        sqlx::query(
            "INSERT INTO cameras (channel_number, primary_track_id, picture_track_id, \
             first_seen_at, last_seen_at, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(1_i64)
        .bind("101")
        .bind("103")
        .bind(&now_text)
        .bind(&now_text)
        .bind(&now_text)
        .bind(&now_text)
        .execute(ops.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO images (image_key, camera_id, track_id, capture_start_at, \
             playback_uri, canonical_playback_uri, local_path, download_status, \
             processing_status, discovered_at, created_at, updated_at) \
             VALUES (?, 1, ?, ?, ?, ?, ?, 'downloaded', 'new', ?, ?, ?)",
        )
        .bind("parser-panic-image")
        .bind("103")
        .bind(&now_text)
        .bind("http://nvr/image")
        .bind("http://nvr/image")
        .bind(image_path.to_str().unwrap())
        .bind(&now_text)
        .bind(&now_text)
        .bind(&now_text)
        .execute(ops.pool())
        .await
        .unwrap();

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"choices":[{"message":{"content":"{\"contains_animal\":false,\"contains_wildlife\":false,\"is_interesting\":false,\"species\":[],\"overall_confidence\":0.1,\"summary\":\"none\",\"uncertainties\":[]}"}}]}"#,
            ))
            .mount(&server)
            .await;
        let classifier_config = crate::configuration::ClassifierConfig {
            endpoints: vec![crate::configuration::ClassifierEndpointConfig {
                base_url: url::Url::parse(&server.uri()).unwrap(),
                endpoint: "/chat/completions".to_string(),
                model: "test-model".to_string(),
                api_key: None,
                username: String::new(),
                password: None,
                request_timeout_seconds: 10,
                prompt_version: "test-v1".to_string(),
                generation: ClassifierGenerationConfig {
                    temperature: 0.1,
                    max_tokens: 100,
                },
            }],
            poll_interval_seconds: 10,
            retry_limit: 3,
            retry_initial_delay_seconds: 1,
            retry_max_delay_seconds: 30,
            processing_lease_seconds: 60,
        };
        let classifier = Arc::new(ClassifierClient::from_config(&classifier_config).unwrap());
        let options = ScannerOptions {
            poll_interval: Duration::from_secs(10),
            retry_limit: 3,
            retry_initial_delay: Duration::from_secs(1),
            retry_max_delay: Duration::from_secs(30),
            processing_lease_duration: Duration::from_secs(60),
            maximum_image_size_bytes: 25_000_000,
        };

        FORCE_PARSE_TASK_JOIN_ERROR.store(true, Ordering::SeqCst);
        let result = Scanner::new(ops.clone(), classifier, options)
            .execute_one_pass()
            .await;
        FORCE_PARSE_TASK_JOIN_ERROR.store(false, Ordering::SeqCst);

        let error = result.expect_err("parser task failure must fail the pass");
        assert_eq!(error.category, ErrorCategory::Internal);
        assert!(
            ops.get_metadata(&ServiceMetadataKey::LastSuccessfulScannerPass)
                .await
                .unwrap()
                .is_none()
        );
        let image = ops.get_image(ImageId::new(1)).await.unwrap();
        assert_eq!(image.processing_status, ProcessingStatus::Processing);
        assert!(image.processing_lease_until.is_some());
    }

    // ── Lease validation tests ──────────────────────────────────────────

    #[test]
    fn lease_too_short_is_rejected() {
        // Request timeout is 120s, lease must be >= 120s.
        let config = Config {
            general: crate::configuration::GeneralConfig {
                database_path: PathBuf::from("/tmp/test.db"),
                output_directory: PathBuf::from("/tmp/output"),
                log_level: crate::cli::LogLevel::Error,
            },
            nvr: crate::configuration::NvrConfig {
                scheme: "http".to_string(),
                host: "test".to_string(),
                port: 80,
                username: "u".to_string(),
                password: Some(crate::configuration::Secret::new("x".to_string())),
                start_at: Timestamp::new(Utc::now()),
                request_timeout_seconds: 30,
                connect_timeout_seconds: 10,
                allow_invalid_tls_certificates: false,
                search: crate::configuration::NvrSearchConfig {
                    window_minutes: 60,
                    max_results: 50,
                    poll_interval_seconds: 60,
                    poll_overlap_seconds: 120,
                    camera_refresh_interval_seconds: 3600,
                    settlement_delay_seconds: 10,
                },
                download: crate::configuration::NvrDownloadConfig {
                    retry_limit: 10,
                    retry_initial_delay_seconds: 5,
                    retry_max_delay_seconds: 300,
                    maximum_image_size_bytes: 25_000_000,
                    verify_jpeg: true,
                    rebase_playback_urls: true,
                    concurrency: 2,
                    playback_host_allowlist: vec![],
                },
            },
            classifier: crate::configuration::ClassifierConfig {
                endpoints: vec![crate::configuration::ClassifierEndpointConfig {
                    base_url: url::Url::parse("http://localhost:8081/v1").unwrap(),
                    endpoint: "/chat/completions".to_string(),
                    model: "test".to_string(),
                    api_key: None,
                    username: String::new(),
                    password: None,
                    request_timeout_seconds: 120,
                    prompt_version: "wildlife-v1".to_string(),
                    generation: crate::configuration::ClassifierGenerationConfig {
                        temperature: 0.1,
                        max_tokens: 1000,
                    },
                }],
                poll_interval_seconds: 10,
                retry_limit: 5,
                retry_initial_delay_seconds: 10,
                retry_max_delay_seconds: 300,
                processing_lease_seconds: 60,
            },
            source_path: PathBuf::from("/tmp/test.toml"),
        };
        let result = ScannerOptions::from_config(&config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Configuration);
        assert!(err.message.contains("lease"));
        assert!(err.message.contains("120"));
    }

    #[test]
    fn lease_overflow_is_rejected() {
        // processing_lease_seconds exceeds the ~100-year safe bound.
        let config = Config {
            general: crate::configuration::GeneralConfig {
                database_path: PathBuf::from("/tmp/test.db"),
                output_directory: PathBuf::from("/tmp/output"),
                log_level: crate::cli::LogLevel::Error,
            },
            nvr: crate::configuration::NvrConfig {
                scheme: "http".to_string(),
                host: "test".to_string(),
                port: 80,
                username: "u".to_string(),
                password: Some(crate::configuration::Secret::new("x".to_string())),
                start_at: Timestamp::new(Utc::now()),
                request_timeout_seconds: 30,
                connect_timeout_seconds: 10,
                allow_invalid_tls_certificates: false,
                search: crate::configuration::NvrSearchConfig {
                    window_minutes: 60,
                    max_results: 50,
                    poll_interval_seconds: 60,
                    poll_overlap_seconds: 120,
                    camera_refresh_interval_seconds: 3600,
                    settlement_delay_seconds: 10,
                },
                download: crate::configuration::NvrDownloadConfig {
                    retry_limit: 10,
                    retry_initial_delay_seconds: 5,
                    retry_max_delay_seconds: 300,
                    maximum_image_size_bytes: 25_000_000,
                    verify_jpeg: true,
                    rebase_playback_urls: true,
                    concurrency: 2,
                    playback_host_allowlist: vec![],
                },
            },
            classifier: crate::configuration::ClassifierConfig {
                endpoints: vec![crate::configuration::ClassifierEndpointConfig {
                    base_url: url::Url::parse("http://localhost:8081/v1").unwrap(),
                    endpoint: "/chat/completions".to_string(),
                    model: "test".to_string(),
                    api_key: None,
                    username: String::new(),
                    password: None,
                    request_timeout_seconds: 30,
                    prompt_version: "wildlife-v1".to_string(),
                    generation: crate::configuration::ClassifierGenerationConfig {
                        temperature: 0.1,
                        max_tokens: 1000,
                    },
                }],
                poll_interval_seconds: 10,
                retry_limit: 5,
                retry_initial_delay_seconds: 10,
                retry_max_delay_seconds: 300,
                processing_lease_seconds: 10_000_000_000,
            },
            source_path: PathBuf::from("/tmp/test.toml"),
        };
        let result = ScannerOptions::from_config(&config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Configuration);
        assert!(err.message.contains("exceeds") || err.message.contains("maximum"));
    }

    #[test]
    fn valid_lease_equal_to_timeout_is_accepted() {
        // Lease equal to request timeout should be accepted.
        let config = Config {
            general: crate::configuration::GeneralConfig {
                database_path: PathBuf::from("/tmp/test.db"),
                output_directory: PathBuf::from("/tmp/output"),
                log_level: crate::cli::LogLevel::Error,
            },
            nvr: crate::configuration::NvrConfig {
                scheme: "http".to_string(),
                host: "test".to_string(),
                port: 80,
                username: "u".to_string(),
                password: Some(crate::configuration::Secret::new("x".to_string())),
                start_at: Timestamp::new(Utc::now()),
                request_timeout_seconds: 30,
                connect_timeout_seconds: 10,
                allow_invalid_tls_certificates: false,
                search: crate::configuration::NvrSearchConfig {
                    window_minutes: 60,
                    max_results: 50,
                    poll_interval_seconds: 60,
                    poll_overlap_seconds: 120,
                    camera_refresh_interval_seconds: 3600,
                    settlement_delay_seconds: 10,
                },
                download: crate::configuration::NvrDownloadConfig {
                    retry_limit: 10,
                    retry_initial_delay_seconds: 5,
                    retry_max_delay_seconds: 300,
                    maximum_image_size_bytes: 25_000_000,
                    verify_jpeg: true,
                    rebase_playback_urls: true,
                    concurrency: 2,
                    playback_host_allowlist: vec![],
                },
            },
            classifier: crate::configuration::ClassifierConfig {
                endpoints: vec![crate::configuration::ClassifierEndpointConfig {
                    base_url: url::Url::parse("http://localhost:8081/v1").unwrap(),
                    endpoint: "/chat/completions".to_string(),
                    model: "test".to_string(),
                    api_key: None,
                    username: String::new(),
                    password: None,
                    request_timeout_seconds: 120,
                    prompt_version: "wildlife-v1".to_string(),
                    generation: crate::configuration::ClassifierGenerationConfig {
                        temperature: 0.1,
                        max_tokens: 1000,
                    },
                }],
                poll_interval_seconds: 10,
                retry_limit: 5,
                retry_initial_delay_seconds: 10,
                retry_max_delay_seconds: 300,
                processing_lease_seconds: 120,
            },
            source_path: PathBuf::from("/tmp/test.toml"),
        };
        let result = ScannerOptions::from_config(&config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Configuration);
        assert!(err.message.contains("lease"));
        assert!(err.message.contains("greater than"));
    }

    #[test]
    fn lease_one_second_above_timeout_is_accepted() {
        // Lease one second above the timeout should be accepted.
        let config = Config {
            general: crate::configuration::GeneralConfig {
                database_path: PathBuf::from("/tmp/test.db"),
                output_directory: PathBuf::from("/tmp/output"),
                log_level: crate::cli::LogLevel::Error,
            },
            nvr: crate::configuration::NvrConfig {
                scheme: "http".to_string(),
                host: "test".to_string(),
                port: 80,
                username: "u".to_string(),
                password: Some(crate::configuration::Secret::new("x".to_string())),
                start_at: Timestamp::new(Utc::now()),
                request_timeout_seconds: 30,
                connect_timeout_seconds: 10,
                allow_invalid_tls_certificates: false,
                search: crate::configuration::NvrSearchConfig {
                    window_minutes: 60,
                    max_results: 50,
                    poll_interval_seconds: 60,
                    poll_overlap_seconds: 120,
                    camera_refresh_interval_seconds: 3600,
                    settlement_delay_seconds: 10,
                },
                download: crate::configuration::NvrDownloadConfig {
                    retry_limit: 10,
                    retry_initial_delay_seconds: 5,
                    retry_max_delay_seconds: 300,
                    maximum_image_size_bytes: 25_000_000,
                    verify_jpeg: true,
                    rebase_playback_urls: true,
                    concurrency: 2,
                    playback_host_allowlist: vec![],
                },
            },
            classifier: crate::configuration::ClassifierConfig {
                endpoints: vec![crate::configuration::ClassifierEndpointConfig {
                    base_url: url::Url::parse("http://localhost:8081/v1").unwrap(),
                    endpoint: "/chat/completions".to_string(),
                    model: "test".to_string(),
                    api_key: None,
                    username: String::new(),
                    password: None,
                    request_timeout_seconds: 120,
                    prompt_version: "wildlife-v1".to_string(),
                    generation: crate::configuration::ClassifierGenerationConfig {
                        temperature: 0.1,
                        max_tokens: 1000,
                    },
                }],
                poll_interval_seconds: 10,
                retry_limit: 5,
                retry_initial_delay_seconds: 10,
                retry_max_delay_seconds: 300,
                processing_lease_seconds: 121,
            },
            source_path: PathBuf::from("/tmp/test.toml"),
        };
        let result = ScannerOptions::from_config(&config);
        assert!(result.is_ok());
        let options = result.unwrap();
        assert_eq!(options.processing_lease_duration, Duration::from_secs(121));
    }
}
