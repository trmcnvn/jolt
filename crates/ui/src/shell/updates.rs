//! Shell updates behavior.

use super::*;

impl Shell {
    pub(super) fn check_for_update(&mut self, cx: &mut Context<Self>) {
        self.user_menu_open = false;
        if self.update_checking {
            cx.notify();
            return;
        }
        self.update_checking = true;
        let edge_url = self.boot.edge_url.clone();
        let check = Tokio::spawn(
            cx,
            async move { jolt_update::fetch_latest(&edge_url).await },
        );
        self.update_check_task = Some(cx.spawn(async move |this, cx| {
            let outcome = match check.await {
                Ok(Ok(manifest)) => Ok(manifest),
                Ok(Err(error)) => Err(format!("{error:#}")),
                Err(error) => Err(error.to_string()),
            };
            this.update(cx, |shell, cx| {
                shell.update_checking = false;
                match outcome {
                    Ok(manifest)
                        if jolt_update::version_newer(
                            &manifest.version,
                            jolt_update::current_version(),
                        ) =>
                    {
                        shell.notified_update_version = Some(manifest.version.clone());
                        shell.show_jolt_update_available(manifest.version, cx);
                    }
                    Ok(_) => crate::toast::show(
                        Toast::new(
                            "jolt-update-check",
                            "Jolt is up to date",
                            ToastKind::Success,
                        )
                        .body(format!(
                            "Version {} is the latest available release.",
                            jolt_update::current_version()
                        )),
                        cx,
                    ),
                    Err(message) => {
                        let shell_handle = cx.entity().downgrade();
                        crate::toast::show(
                            Toast::new(
                                "jolt-update-check",
                                "Update check failed",
                                ToastKind::Error,
                            )
                            .body(message)
                            .action(ToastAction::new(
                                "Retry",
                                move |cx| {
                                    shell_handle
                                        .update(cx, |shell, cx| shell.check_for_update(cx))
                                        .ok();
                                },
                            )),
                            cx,
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Fetch and stage a new macOS bundle. Progress and outcomes are app-wide
    /// notifications so delivery follows the user's notification preference.
    pub(super) fn begin_update_download(&mut self, cx: &mut Context<Self>) {
        if matches!(self.update_flow, UpdateFlow::Downloading) {
            return;
        }
        let edge_url = self.boot.edge_url.clone();
        let data_dir = self.data_dir.clone();
        self.update_flow = UpdateFlow::Downloading;
        crate::toast::show(
            Toast::new(
                "jolt-update-download",
                "Downloading Jolt update",
                ToastKind::Info,
            )
            .body("The update will be ready to restart shortly."),
            cx,
        );
        let download = Tokio::spawn(cx, async move {
            let manifest = jolt_update::fetch_latest(&edge_url).await?;
            jolt_update::stage_mac_app(&edge_url, &manifest, &data_dir).await
        });
        self.update_task = Some(cx.spawn(async move |this, cx| {
            let outcome = match download.await {
                Ok(Ok(staged)) => Ok(staged),
                Ok(Err(err)) => Err(format!("{err:#}")),
                Err(join_err) => Err(join_err.to_string()),
            };
            this.update(cx, |shell, cx| match outcome {
                Ok(staged) => {
                    shell.update_flow = UpdateFlow::Ready(staged);
                    let shell_handle = cx.entity().downgrade();
                    crate::toast::show(
                        Toast::new("jolt-update-ready", "Jolt update ready", ToastKind::Success)
                            .persistent()
                            .body("Restart Jolt to apply the update.")
                            .action(ToastAction::new("Restart", move |cx| {
                                shell_handle
                                    .update(cx, |shell, cx| shell.apply_ready_update(cx))
                                    .ok();
                            })),
                        cx,
                    );
                }
                Err(message) => {
                    tracing::warn!(%message, "update download failed");
                    shell.update_flow = UpdateFlow::Failed;
                    shell.show_update_error(message, cx);
                }
            })
            .ok();
        }));
        cx.notify();
    }

    pub(super) fn apply_ready_update(&mut self, cx: &mut Context<Self>) {
        let flow = std::mem::replace(&mut self.update_flow, UpdateFlow::Idle);
        match flow {
            UpdateFlow::Ready(staged) => self.apply_staged_update(staged, cx),
            other => self.update_flow = other,
        }
    }

    pub(super) fn show_update_error(&mut self, message: String, cx: &mut Context<Self>) {
        let shell = cx.entity().downgrade();
        crate::toast::show(
            Toast::new("jolt-update-error", "Jolt update failed", ToastKind::Error)
                .persistent()
                .body(message)
                .action(ToastAction::new("Retry", move |cx| {
                    shell
                        .update(cx, |shell, cx| shell.begin_update_download(cx))
                        .ok();
                })),
            cx,
        );
    }

    /// Swap the staged bundle over the installed one, restart an installed
    /// background engine during graceful app teardown, then relaunch the new
    /// bundle once this process (and its engine lock / IPC port) is gone.
    pub(super) fn apply_staged_update(&mut self, staged: PathBuf, cx: &mut Context<Self>) {
        let jolt_update::InstallKind::MacApp { bundle } = self.install.clone() else {
            return;
        };
        match jolt_update::apply_mac_app(&staged, &bundle) {
            Ok(()) => {
                if self.background_service.enabled() {
                    self.background_service.request_restart();
                }
                jolt_update::relaunch_app_after_exit(&bundle);
                cx.quit();
            }
            Err(err) => {
                let message = format!("{err:#}");
                tracing::error!(error = %err, "update apply failed");
                self.update_flow = UpdateFlow::Failed;
                self.show_update_error(message, cx);
            }
        }
    }
}
