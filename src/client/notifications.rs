use std::io;
use std::process::Stdio;

use tracing::{debug, warn};

use crate::protocol::NotifyKind;

use super::shell;

#[cfg(not(windows))]
use crate::platform::show_desktop_notification as show_untargeted_system_notification;

#[cfg(windows)]
fn queue_system_notification(
    task: impl FnOnce() -> io::Result<bool> + Send + 'static,
) -> io::Result<bool> {
    type Task = Box<dyn FnOnce() -> io::Result<bool> + Send>;
    static QUEUE: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<Task>> =
        std::sync::OnceLock::new();
    let queue = QUEUE.get_or_init(|| {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<Task>();
        // Preserve replacement order while keeping native waits off the client loop.
        tokio::spawn(async move {
            while let Some(task) = receiver.recv().await {
                let result = tokio::task::spawn_blocking(task)
                    .await
                    .unwrap_or_else(|err| Err(io::Error::other(err)));
                if let Err(err) = result {
                    warn!(err = %err, "failed to emit system notification");
                }
            }
        });
        sender
    });
    queue.send(Box::new(task)).map_err(|_| {
        io::Error::new(
            io::ErrorKind::BrokenPipe,
            "notification delivery queue closed",
        )
    })?;
    Ok(true)
}

#[cfg(windows)]
fn show_untargeted_system_notification(title: &str, body: Option<&str>) -> io::Result<bool> {
    let title = title.to_owned();
    let body = body.map(str::to_owned);
    queue_system_notification(move || {
        crate::platform::show_desktop_notification(&title, body.as_deref())
    })
}

pub(super) fn handle_shell_notification_effects(
    effects: Vec<shell::ClientShellNotificationEffect>,
    sound_config: &crate::config::SoundConfig,
    #[cfg(windows)] event_tx: &tokio::sync::mpsc::Sender<super::events::ClientLoopEvent>,
) {
    for effect in effects {
        match effect {
            shell::ClientShellNotificationEffect::Sound { sound, agent } => {
                let agent = agent.as_deref().and_then(crate::detect::parse_agent_label);
                if sound_config.allows(agent) {
                    crate::sound::play(sound, sound_config);
                }
            }
            shell::ClientShellNotificationEffect::Terminal { title, body } => {
                if let Err(err) = crate::terminal_notify::show_notification(&title, body.as_deref())
                {
                    warn!(err = %err, "failed to emit terminal notification");
                }
            }
            shell::ClientShellNotificationEffect::System {
                title,
                body,
                #[cfg(windows)]
                target,
            } => {
                #[cfg(windows)]
                let result = show_system_notification(&title, body.as_deref(), target, event_tx);
                #[cfg(not(windows))]
                let result = crate::platform::show_desktop_notification(&title, body.as_deref());
                if let Err(err) = result {
                    warn!(err = %err, "failed to emit system notification");
                }
            }
            shell::ClientShellNotificationEffect::SystemCommand { command, env } => {
                if let Err(err) = spawn_notification_command(&command, env) {
                    warn!(err = %err, "failed to run ui.toast.system.command");
                }
            }
        }
    }
}

#[cfg(windows)]
fn show_system_notification(
    title: &str,
    body: Option<&str>,
    target: Option<shell::ClientSystemNotificationTarget>,
    event_tx: &tokio::sync::mpsc::Sender<super::events::ClientLoopEvent>,
) -> io::Result<bool> {
    let Some(target) = target else {
        return show_untargeted_system_notification(title, body);
    };
    let key = serde_json::to_string(&(
        &target.endpoint_id.storage_key(),
        &target.boot_id,
        &target.pane_id,
    ))
    .map_err(io::Error::other)?;
    let event_tx = event_tx.clone();
    let runtime = tokio::runtime::Handle::current();
    let title = title.to_owned();
    let body = body.map(str::to_owned);
    queue_system_notification(move || {
        crate::platform::show_actionable_desktop_notification(
            &title,
            body.as_deref(),
            key,
            std::sync::Arc::new(move || {
                let event_tx = event_tx.clone();
                let target = target.clone();
                // Never block a native callback: WinRT can dispatch it while Show is awaited.
                runtime.spawn(async move {
                    let _ = event_tx
                        .send(super::events::ClientLoopEvent::NotificationActivated(
                            target,
                        ))
                        .await;
                });
            }),
        )
    })
}

/// Run `[ui.toast.system] command` detached, like a `[[keys.command]]`
/// shell command. A background thread reaps the child, so a slow command
/// never blocks the client loop.
fn spawn_notification_command(command: &str, env: Vec<(String, String)>) -> io::Result<()> {
    let mut process = crate::platform::detached_custom_command_process(command);
    process
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env(
            crate::api::SOCKET_PATH_ENV_VAR,
            crate::api::socket_path().as_os_str(),
        )
        .envs(env);
    if let Ok(current_exe) = std::env::current_exe() {
        process.env("HERDR_BIN_PATH", current_exe);
    }
    let mut child = process.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

pub(super) fn handle_notify(
    kind: NotifyKind,
    message: &str,
    body: Option<&str>,
    sound_config: &crate::config::SoundConfig,
) {
    handle_notify_with_notifiers(
        kind,
        message,
        body,
        sound_config,
        crate::terminal_notify::show_notification,
        show_untargeted_system_notification,
    );
}

pub(super) fn handle_notify_with_notifiers(
    kind: NotifyKind,
    message: &str,
    body: Option<&str>,
    sound_config: &crate::config::SoundConfig,
    mut show_terminal_notification: impl FnMut(&str, Option<&str>) -> io::Result<bool>,
    mut show_system_notification: impl FnMut(&str, Option<&str>) -> io::Result<bool>,
) {
    match kind {
        NotifyKind::Sound => {
            let Some(sound) = sound_from_notify_message(message) else {
                warn!(
                    message = message,
                    "received unknown sound notification from server"
                );
                return;
            };
            if sound_config.enabled {
                crate::sound::play(sound, sound_config);
            }
        }
        NotifyKind::Toast => {
            debug!(
                message = message,
                "received terminal toast notification from server"
            );
            if let Err(err) = show_terminal_notification(message, body) {
                warn!(err = %err, "failed to emit terminal notification");
            }
        }
        NotifyKind::SystemToast => {
            debug!(
                message = message,
                "received system toast notification from server"
            );
            if let Err(err) = show_system_notification(message, body) {
                warn!(err = %err, "failed to emit system notification");
            }
        }
    }
}

pub(super) fn sound_from_notify_message(message: &str) -> Option<crate::sound::Sound> {
    match message {
        "agent done" => Some(crate::sound::Sound::Done),
        "agent attention" => Some(crate::sound::Sound::Request),
        _ => None,
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn native_delivery_keeps_client_responsive_and_preserves_replacement_order() {
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        queue_system_notification(move || {
            let _ = started.send(());
            wait.recv_timeout(std::time::Duration::from_secs(5))
                .map_err(io::Error::other)?;
            Ok(true)
        })
        .expect("queued first notification");
        ready.await.expect("notification worker started");
        let (finished, mut second) = tokio::sync::oneshot::channel();
        queue_system_notification(move || {
            let _ = finished.send(());
            Ok(true)
        })
        .expect("queued replacement");
        tokio::task::yield_now().await;
        assert_eq!(
            second.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        );
        // This send must run while native delivery is still waiting.
        release.send(()).expect("client loop remained responsive");
        second.await.expect("replacement delivered after original");
    }
}

#[cfg(all(test, unix))]
mod tests {
    #[test]
    fn notification_command_runs_detached_with_its_environment() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let out = std::env::temp_dir().join(format!(
            "herdr-notification-command-{}-{nanos}",
            std::process::id()
        ));
        let command = format!(
            "printf '%s|%s' \"$HERDR_NOTIFICATION_PANE_ID\" \"$HERDR_BIN_PATH\" > '{}'",
            out.display()
        );
        super::spawn_notification_command(
            &command,
            vec![("HERDR_NOTIFICATION_PANE_ID".into(), "w1:p2".into())],
        )
        .unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let written = loop {
            match std::fs::read_to_string(&out) {
                Ok(text) if text.contains('|') => break text,
                _ if std::time::Instant::now() > deadline => panic!("command did not run"),
                _ => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        };
        let _ = std::fs::remove_file(&out);
        let (pane_id, bin_path) = written.split_once('|').unwrap();
        assert_eq!(pane_id, "w1:p2");
        assert!(!bin_path.is_empty());
    }
}
