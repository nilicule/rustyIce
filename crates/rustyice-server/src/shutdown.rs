use tokio_util::sync::CancellationToken;
use tracing::info;

/// Resolves on SIGTERM or Ctrl-C. Cancels `token` so all tasks drain cleanly.
///
/// # Panics
/// Panics if the OS signal handler cannot be registered.
pub async fn shutdown_signal(token: CancellationToken) {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to listen for Ctrl-C");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to register SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c    => info!("Ctrl-C received, shutting down"),
        () = terminate => info!("SIGTERM received, shutting down"),
    }

    token.cancel();
}
