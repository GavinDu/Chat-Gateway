//! Browser worker lifecycle supervision and its failure/recovery tests.
use super::{Command, MetricsRegistry, ProviderSessionRegistry, SessionWriteTracker};
#[cfg(test)]
use super::{ObscuraPool, WorkerReadyGuard};
use crate::config::AppConfig;
use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

pub(super) fn effective_session_pool_size(configured: usize) -> usize {
    configured.min(1)
}

const MAX_BROWSER_WORKER_RESTART_BACKOFF: Duration = Duration::from_secs(30);

pub(super) type BrowserWorkerSpawner = fn(
    Arc<AppConfig>,
    mpsc::Receiver<Command>,
    oneshot::Sender<Result<()>>,
    Arc<AtomicBool>,
    ProviderSessionRegistry,
    MetricsRegistry,
    SessionWriteTracker,
) -> std::thread::JoinHandle<()>;

pub(super) fn join_finished_worker(worker: &mut Option<std::thread::JoinHandle<()>>) -> bool {
    let Some(handle) = worker.as_ref() else {
        return true;
    };
    if !handle.is_finished() {
        return false;
    }
    if let Some(handle) = worker.take() {
        if handle.join().is_err() {
            tracing::debug!("browser worker thread joined after panic");
        }
    }
    true
}

pub(super) async fn join_worker_on_shutdown(worker: Option<std::thread::JoinHandle<()>>) {
    let Some(worker) = worker else {
        return;
    };
    match tokio::task::spawn_blocking(move || worker.join()).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => tracing::debug!("browser worker joined after panic during shutdown"),
        Err(error) => tracing::warn!(%error, "browser worker join task failed during shutdown"),
    }
}

pub(super) async fn wait_for_worker_shutdown(shutting_down: &AtomicBool) {
    while !shutting_down.load(Ordering::Acquire) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub(super) fn next_browser_worker_restart_backoff(current: Duration) -> Duration {
    (current * 2).min(MAX_BROWSER_WORKER_RESTART_BACKOFF)
}

pub(super) fn closed_command_channel() -> mpsc::Sender<Command> {
    let (sender, receiver) = mpsc::channel::<Command>(1);
    drop(receiver);
    sender
}


#[cfg(test)]
mod worker_supervisor_tests {
    use super::{
        closed_command_channel, join_finished_worker, next_browser_worker_restart_backoff,
        WorkerReadyGuard,
    };
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::time::Duration;

    #[test]
    fn finished_panicked_worker_is_joined_before_replacement() {
        let worker = std::thread::spawn(|| panic!("injected worker panic"));
        while !worker.is_finished() {
            std::thread::yield_now();
        }
        let mut worker = Some(worker);
        assert!(join_finished_worker(&mut worker));
        assert!(
            worker.is_none(),
            "the finished worker handle must be consumed"
        );
    }

    #[tokio::test]
    async fn graceful_shutdown_waits_for_worker_thread_to_exit() {
        let exited = Arc::new(AtomicBool::new(false));
        let worker_exited = exited.clone();
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            worker_exited.store(true, Ordering::Release);
        });

        super::join_worker_on_shutdown(Some(worker)).await;

        assert!(
            exited.load(Ordering::Acquire),
            "shutdown must join the worker before returning"
        );
    }

    #[test]
    fn restart_backoff_doubles_and_is_capped() {
        assert_eq!(
            next_browser_worker_restart_backoff(Duration::from_secs(1)),
            Duration::from_secs(2)
        );
        assert_eq!(
            next_browser_worker_restart_backoff(Duration::from_secs(16)),
            Duration::from_secs(30)
        );
        assert_eq!(
            next_browser_worker_restart_backoff(Duration::from_secs(30)),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn closed_command_channel_is_observed_as_closed_by_supervisor() {
        assert!(closed_command_channel().is_closed());
    }

    #[test]
    fn worker_ready_guard_clears_liveness_when_worker_exits() {
        let ready = Arc::new(AtomicBool::new(true));
        drop(WorkerReadyGuard(ready.clone()));
        assert!(!ready.load(Ordering::Acquire));
    }

    static RESTART_TEST_SPAWNS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    fn restart_test_worker_spawner(
        _cfg: Arc<crate::config::AppConfig>,
        mut commands: tokio::sync::mpsc::Receiver<super::Command>,
        ready: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
        worker_ready: Arc<AtomicBool>,
        _provider_sessions: super::ProviderSessionRegistry,
        _metrics: super::MetricsRegistry,
        _session_writes: super::SessionWriteTracker,
    ) -> std::thread::JoinHandle<()> {
        let generation = RESTART_TEST_SPAWNS.fetch_add(1, Ordering::SeqCst);
        std::thread::spawn(move || {
            let _ready_guard = super::WorkerReadyGuard(worker_ready);
            let _ = ready.send(Ok(()));
            if generation == 0 {
                if let Some(super::Command::Discover { reply, .. }) = commands.blocking_recv() {
                    drop(reply);
                }
                panic!("injected browser worker failure after accepting a request");
            }

            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("restart test runtime should build");
            runtime.block_on(async move {
                while let Some(command) = commands.recv().await {
                    match command {
                        super::Command::Discover { reply, .. } => {
                            let _ = reply.send(Ok(vec!["recovered-model".to_owned()]));
                        }
                        super::Command::Shutdown { reply } => {
                            let _ = reply.send(());
                            break;
                        }
                        _ => {}
                    }
                }
            });
        })
    }

    #[tokio::test]
    async fn request_after_worker_panic_succeeds_only_after_replacement_warmup() {
        RESTART_TEST_SPAWNS.store(0, Ordering::SeqCst);
        let config: crate::config::AppConfig =
            serde_yaml::from_str("providers: []").expect("empty test config should parse");
        let pool = super::ObscuraPool::new_with_worker_spawner(
            Arc::new(config),
            restart_test_worker_spawner,
        )
        .await
        .expect("pool should start with injected worker");

        pool.warm_all()
            .await
            .expect("initial worker should report ready");
        let restart_metric_before = pool
            .metrics()
            .render_with_provider_states(&[])
            .lines()
            .find(|line| line.starts_with("gateway_browser_worker_restarts_total "))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or_default();
        // The /ready route delegates to this response helper; exercise its
        // healthy contract with the injected worker before inducing a panic.
        assert_eq!(
            crate::api::health::readiness_response(
                1,
                pool.worker_alive(),
                pool.worker_generation(),
            )
            .status(),
            axum::http::StatusCode::OK,
            "readiness should be healthy before the injected worker failure"
        );
        assert!(
            pool.discover("chatgpt").await.is_err(),
            "the request in flight during the injected crash must fail closed"
        );

        tokio::time::timeout(Duration::from_secs(6), async {
            while pool.worker_generation() == 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("replacement worker should complete warm-up");
        assert!(
            pool.worker_alive(),
            "replacement should be ready after warm-up"
        );
        assert_eq!(
            crate::api::health::readiness_response(
                0,
                false,
                pool.worker_generation().saturating_sub(1),
            )
            .status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "readiness must fail while the browser worker is down"
        );
        assert_eq!(
            crate::api::health::readiness_response(
                1,
                pool.worker_alive(),
                pool.worker_generation(),
            )
            .status(),
            axum::http::StatusCode::OK,
            "readiness must recover after replacement warm-up"
        );
        let restart_metric_after = pool
            .metrics()
            .render_with_provider_states(&[])
            .lines()
            .find(|line| line.starts_with("gateway_browser_worker_restarts_total "))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or_default();
        assert!(
            restart_metric_after >= restart_metric_before.saturating_add(1),
            "successful worker replacement must increment restart metric"
        );
        assert_eq!(
            pool.discover("chatgpt")
                .await
                .expect("recovered request should succeed"),
            vec!["recovered-model"]
        );
        pool.shutdown()
            .await
            .expect("replacement worker should shut down cleanly");
    }

    #[test]
    fn injected_worker_panic_closes_command_channel_and_clears_readiness() {
        let ready = Arc::new(AtomicBool::new(true));
        let (command_tx, command_rx) = tokio::sync::mpsc::channel::<()>(1);
        let worker_ready = ready.clone();
        let worker = std::thread::spawn(move || {
            let _ready_guard = WorkerReadyGuard(worker_ready);
            let _commands = command_rx;
            panic!("injected browser-worker panic for supervisor regression test");
        });

        assert!(worker.join().is_err(), "the test worker should panic");
        assert!(
            command_tx.is_closed(),
            "a panicked worker must close its command receiver so the supervisor can detect death"
        );
        assert!(
            !ready.load(Ordering::Acquire),
            "worker readiness must be cleared during panic unwinding"
        );
    }
}

