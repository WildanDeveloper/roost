use bollard::container::AttachContainerResults;
use futures_util::StreamExt;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

use crate::server::Server;

/// Attach to a container's console: spawn a task that forwards stdout to
/// the server's log pipeline and forwards stdin commands into the
/// container. The server keeps the command sender to talk to the task.
pub async fn start_console(server: Arc<Server>, attach: AttachContainerResults) {
    let (mut output, mut input) = (attach.output, attach.input);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(256);
    server.set_console_tx(Some(tx)).await;

    tokio::spawn(async move {
        loop {
            tokio::select! {
                item = output.next() => {
                    match item {
                        Some(Ok(line)) => {
                            let bytes = line.into_bytes();
                            server.push_console_bytes(&bytes).await;
                        }
                        Some(Err(e)) => {
                            tracing::warn!(uuid = ?server.uuid, error = %e, "console read error");
                            break;
                        }
                        None => break,
                    }
                }
                cmd = rx.recv() => {
                    match cmd {
                        Some(command) => {
                            if let Err(e) = input.write_all(format!("{command}\r").as_bytes()).await {
                                tracing::warn!(uuid = ?server.uuid, error = %e, "stdin write failed");
                            }
                            let _ = input.flush().await;
                        }
                        None => break,
                    }
                }
            }
        }

        server.set_console_tx(None).await;
    });
}