//! Scoped idle inhibition; no settings changes and no unlocking/input injection.
use anyhow::Result;
pub struct PowerLease {
    #[cfg(target_os = "macos")]
    child: std::process::Child,
    #[cfg(target_os = "linux")]
    connection: zbus::Connection,
    #[cfg(target_os = "linux")]
    cookie: u32,
    #[cfg(target_os = "linux")]
    gnome: bool,
}
impl PowerLease {
    pub async fn acquire() -> Result<Self> {
        #[cfg(target_os = "macos")]
        {
            let child = std::process::Command::new("/usr/bin/caffeinate")
                .args(["-d", "-i", "-w", &std::process::id().to_string()])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()?;
            return Ok(Self { child });
        }
        #[cfg(target_os = "linux")]
        {
            let connection = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                zbus::Connection::session(),
            )
            .await??;
            let result = async {
                let proxy = zbus::Proxy::new(
                    &connection,
                    "org.freedesktop.ScreenSaver",
                    "/org/freedesktop/ScreenSaver",
                    "org.freedesktop.ScreenSaver",
                )
                .await?;
                proxy
                    .call::<_, _, u32>("Inhibit", &("AirBridge", "Video stream active"))
                    .await
            };
            if let Ok(Ok(cookie)) =
                tokio::time::timeout(std::time::Duration::from_secs(3), result).await
            {
                return Ok(Self {
                    connection,
                    cookie,
                    gnome: false,
                });
            }
            let proxy = zbus::Proxy::new(
                &connection,
                "org.gnome.SessionManager",
                "/org/gnome/SessionManager",
                "org.gnome.SessionManager",
            )
            .await?;
            let cookie = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                proxy.call::<_, _, u32>(
                    "Inhibit",
                    &("AirBridge", 0u32, "Video stream active", 12u32),
                ),
            )
            .await??;
            return Ok(Self {
                connection,
                cookie,
                gnome: true,
            });
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        anyhow::bail!("Idle inhibition is unsupported on this platform")
    }
    pub async fn release(self) {
        #[cfg(target_os = "linux")]
        {
            let (dest, path, iface, method) = if self.gnome {
                (
                    "org.gnome.SessionManager",
                    "/org/gnome/SessionManager",
                    "org.gnome.SessionManager",
                    "Uninhibit",
                )
            } else {
                (
                    "org.freedesktop.ScreenSaver",
                    "/org/freedesktop/ScreenSaver",
                    "org.freedesktop.ScreenSaver",
                    "UnInhibit",
                )
            };
            if let Ok(proxy) = zbus::Proxy::new(&self.connection, dest, path, iface).await {
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    proxy.call::<_, _, ()>(method, &(self.cookie,)),
                )
                .await;
            }
        }
    }
}
#[cfg(target_os = "macos")]
impl Drop for PowerLease {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
/// Runs off the media/control executor; each state transition is applied once.
pub struct PowerController {
    sender: tokio::sync::watch::Sender<bool>,
}
impl PowerController {
    pub fn new(report: impl Fn(String) + Send + 'static) -> Self {
        Self::with_backend(report, PowerLease::acquire, |lease| lease.release())
    }
    fn with_backend<L, F, U>(
        report: impl Fn(String) + Send + 'static,
        mut acquire: impl FnMut() -> F + Send + 'static,
        mut release: impl FnMut(L) -> U + Send + 'static,
    ) -> Self
    where
        L: Send + 'static,
        F: std::future::Future<Output = Result<L>> + 'static,
        U: std::future::Future<Output = ()> + 'static,
    {
        let (sender, mut receiver) = tokio::sync::watch::channel(false);
        std::thread::spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                report("Keep awake unavailable: executor initialization failed".into());
                return;
            };
            runtime.block_on(async move {
                let mut lease = None;
                while receiver.changed().await.is_ok() {
                    let wanted = *receiver.borrow_and_update();
                    if wanted {
                        if lease.is_none() {
                            match acquire().await {
                                Ok(value) => {
                                    lease = Some(value);
                                    report(
                                        "Keep awake active · desktop lock policy applies".into(),
                                    );
                                }
                                Err(error) => report(format!("Keep awake unavailable: {error}")),
                            }
                        }
                    } else if let Some(value) = lease.take() {
                        release(value).await;
                        report("Keep awake released".into());
                    }
                }
                if let Some(value) = lease {
                    release(value).await;
                }
            });
        });
        Self { sender }
    }
    pub fn set_active(&self, active: bool) {
        self.sender.send_if_modified(|value| {
            if *value == active {
                false
            } else {
                *value = active;
                true
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn power_demand_is_idempotent_and_releases_on_disable_and_owner_drop() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        };
        let acquired = Arc::new(AtomicUsize::new(0));
        let released = Arc::new(AtomicUsize::new(0));
        let (send, receive) = mpsc::channel();
        let a = acquired.clone();
        let r = released.clone();
        let controller = PowerController::with_backend(
            move |line| {
                let _ = send.send(line);
            },
            move || {
                a.fetch_add(1, Ordering::SeqCst);
                async { Ok(()) }
            },
            move |_| {
                r.fetch_add(1, Ordering::SeqCst);
                async {}
            },
        );
        controller.set_active(true);
        receive
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        for _ in 0..10 {
            controller.set_active(true);
        }
        assert_eq!(acquired.load(Ordering::SeqCst), 1);
        controller.set_active(false);
        receive
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert_eq!(released.load(Ordering::SeqCst), 1);
        controller.set_active(true);
        receive
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        drop(controller);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while released.load(Ordering::SeqCst) != 2 && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(released.load(Ordering::SeqCst), 2);
    }
    #[test]
    fn unavailable_backend_is_soft_and_not_retried_every_frame() {
        let (send, receive) = std::sync::mpsc::channel();
        let controller = PowerController::with_backend(
            move |line| {
                let _ = send.send(line);
            },
            || async { Err::<(), _>(anyhow::anyhow!("Desktop denied idle inhibition")) },
            |_| async {},
        );
        controller.set_active(true);
        assert!(
            receive
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap()
                .contains("unavailable")
        );
        controller.set_active(true);
        assert!(receive.try_recv().is_err());
        controller.set_active(false);
    }
}
