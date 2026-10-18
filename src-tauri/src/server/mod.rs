//! The local gateway: axum on 127.0.0.1, start/stop control.

mod handlers;
mod relay;

use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use axum::Router;
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::i18n::t;
use crate::state::{Core, Notice};

/// Request bodies with images and long histories get big; Anthropic itself accepts 32 MB.
const BODY_LIMIT: usize = 128 * 1024 * 1024;

/// Silence towards the client longer than this gets an `event: ping`.
pub const PING_INTERVAL: Duration = Duration::from_secs(10);
/// How long a streaming request waits for upstream headers before committing to a 200 SSE
/// response. Errors that arrive within it keep their real HTTP status.
pub const HEADER_WAIT: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub ping: Duration,
    pub header_wait: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self { ping: PING_INTERVAL, header_wait: HEADER_WAIT }
    }
}

#[derive(Clone)]
pub struct Ctx {
    pub core: Arc<Core>,
    /// Cancelled on stop so open streams end instead of holding the shutdown.
    pub cancel: CancellationToken,
    pub timing: Timing,
}

pub fn router(ctx: Ctx) -> Router {
    Router::new()
        .route("/", get(handlers::root))
        .route("/v1/messages", post(handlers::messages))
        .route("/v1/messages/count_tokens", post(handlers::count_tokens))
        .route("/v1/models", get(handlers::list_models))
        .route("/v1/models/{*id}", get(handlers::get_model))
        .route("/api/hello", get(handlers::hello))
        .fallback(handlers::not_found)
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .with_state(ctx)
}

#[derive(Debug, Clone, Serialize, Default, PartialEq, Eq)]
pub struct ServerStatus {
    pub running: bool,
    pub port: u16,
    pub error: Option<String>,
}

struct Running {
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

#[derive(Default)]
pub struct Server {
    running: tokio::sync::Mutex<Option<Running>>,
    status: Arc<RwLock<ServerStatus>>,
}

impl Server {
    pub fn status(&self) -> ServerStatus {
        self.status.read().unwrap().clone()
    }

    fn set_status(&self, core: &Core, status: ServerStatus) {
        *self.status.write().unwrap() = status;
        core.notify(Notice::State);
    }

    /// Binds 127.0.0.1:<port from config>. Starting a running server is a no-op.
    pub async fn start(&self, core: Arc<Core>) -> Result<(), String> {
        let mut slot = self.running.lock().await;
        if slot.as_ref().is_some_and(|r| !r.task.is_finished()) {
            return Ok(());
        }
        let port = core.config().port;
        let listener = match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(l) => l,
            Err(e) => {
                let message = if e.kind() == std::io::ErrorKind::AddrInUse {
                    t(
                        format!("Порт {port} занят другой программой. Выберите другой во вкладке «Сервер»."),
                        format!("Port {port} is used by another program. Pick another one on the Server tab."),
                    )
                } else {
                    t(format!("Не удалось открыть 127.0.0.1:{port}: {e}"), format!("Cannot open 127.0.0.1:{port}: {e}"))
                };
                self.set_status(&core, ServerStatus { running: false, port, error: Some(message.clone()) });
                return Err(message);
            }
        };
        let cancel = CancellationToken::new();
        let app = router(Ctx { core: core.clone(), cancel: cancel.clone(), timing: Timing::default() });
        let shutdown = cancel.clone();
        let status = self.status.clone();
        let task_core = core.clone();
        let task = tokio::spawn(async move {
            let result = axum::serve(listener, app).with_graceful_shutdown(shutdown.clone().cancelled_owned()).await;
            if !shutdown.is_cancelled() {
                // Died on its own: show it instead of pretending to run.
                let error = result.err().map(|e| e.to_string()).unwrap_or_else(|| t("сервер остановился", "the server stopped").into());
                *status.write().unwrap() = ServerStatus { running: false, port, error: Some(error) };
                task_core.notify(Notice::State);
            }
        });
        *slot = Some(Running { cancel, task });
        drop(slot);
        self.set_status(&core, ServerStatus { running: true, port, error: None });
        Ok(())
    }

    pub async fn stop(&self, core: &Core) {
        let running = self.running.lock().await.take();
        if let Some(Running { cancel, mut task }) = running {
            cancel.cancel();
            if tokio::time::timeout(Duration::from_secs(3), &mut task).await.is_err() {
                task.abort();
            }
        }
        let port = core.config().port;
        self.set_status(core, ServerStatus { running: false, port, error: None });
    }

    pub async fn restart(&self, core: Arc<Core>) -> Result<(), String> {
        self.stop(&core).await;
        self.start(core).await
    }
}

#[cfg(test)]
mod tests;
