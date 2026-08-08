//! Shell updates behavior.

use super::*;

impl Shell {
    pub(super) fn notify_harness_updates(
        &mut self,
        state: &Entity<AppState>,
        cx: &mut Context<Self>,
    ) {
        let state = state.read(cx);
        let local = state.harness_updates.clone();
        let remotes = state.remote_harness_updates.clone();
        let device_names = state.remote_harness_update_device_names.clone();

        for status in local {
            self.notify_harness_update(status, None, None, cx);
        }
        for (device_id, statuses) in remotes {
            let device_name = device_names
                .get(&device_id)
                .cloned()
                .unwrap_or_else(|| device_id.clone());
            for status in statuses {
                self.notify_harness_update(status, Some(device_id.clone()), Some(&device_name), cx);
            }
        }
    }

    fn notify_harness_update(
        &mut self,
        status: HarnessUpdateStatus,
        target_device_id: Option<String>,
        device_name: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let name = harness_label(status.harness);
        let device_suffix = device_name.map_or_else(String::new, |name| format!(" on {name}"));
        let request_key = (target_device_id.clone(), status.harness);
        let toast_scope = target_device_id.as_deref().unwrap_or("local");
        match status.state {
            HarnessUpdateState::UpdateAvailable | HarnessUpdateState::Manual => {
                let Some(latest) = status.latest_version.clone() else {
                    return;
                };
                let notice_key = format!(
                    "{toast_scope}:{:?}:{latest}:{:?}",
                    status.harness, status.state
                );
                if !self.notified_harness_updates.insert(notice_key) {
                    return;
                }
                let mut toast = Toast::new(
                    format!("harness-update-{toast_scope}-{:?}-{latest}", status.harness),
                    format!("{name} update available{device_suffix}"),
                    ToastKind::Info,
                )
                .persistent()
                .body(format!(
                    "{} → {latest}",
                    status.current_version.as_deref().unwrap_or("Installed")
                ));
                if status.can_apply {
                    let shell = cx.entity().downgrade();
                    let harness = status.harness;
                    toast = toast.action(ToastAction::new("Update", move |cx| {
                        let target_device_id = target_device_id.clone();
                        shell
                            .update(cx, |shell, cx| {
                                shell.apply_harness_update(harness, target_device_id, cx)
                            })
                            .ok();
                    }));
                } else if let Some(detail) = status.detail {
                    toast = toast.body(detail);
                }
                crate::toast::show(toast, cx);
            }
            HarnessUpdateState::WaitingForIdle | HarnessUpdateState::Updating
                if self.requested_harness_updates.contains(&request_key) =>
            {
                let phase = if status.state == HarnessUpdateState::WaitingForIdle {
                    "waiting"
                } else {
                    "installing"
                };
                let notice_key = format!("{toast_scope}:{:?}:{phase}", status.harness);
                if self.notified_harness_updates.insert(notice_key) {
                    crate::toast::show(
                        Toast::new(
                            format!("harness-update-progress-{toast_scope}-{:?}", status.harness),
                            format!("Updating {name}{device_suffix}"),
                            ToastKind::Info,
                        )
                        .persistent()
                        .body(status.detail.unwrap_or_else(|| "Update in progress".into())),
                        cx,
                    );
                }
            }
            HarnessUpdateState::Updated if self.requested_harness_updates.remove(&request_key) => {
                crate::toast::show(
                    Toast::new(
                        format!("harness-update-progress-{toast_scope}-{:?}", status.harness),
                        format!("{name} updated{device_suffix}"),
                        ToastKind::Success,
                    )
                    .body(status.detail.unwrap_or_else(|| "Update completed".into())),
                    cx,
                );
            }
            HarnessUpdateState::Failed if self.requested_harness_updates.remove(&request_key) => {
                let shell = cx.entity().downgrade();
                let harness = status.harness;
                crate::toast::show(
                    Toast::new(
                        format!("harness-update-progress-{toast_scope}-{harness:?}"),
                        format!("{name} update failed{device_suffix}"),
                        ToastKind::Error,
                    )
                    .persistent()
                    .body(status.detail.unwrap_or_else(|| "Update failed".into()))
                    .action(ToastAction::new("Retry", move |cx| {
                        let target_device_id = target_device_id.clone();
                        shell
                            .update(cx, |shell, cx| {
                                shell.apply_harness_update(harness, target_device_id, cx)
                            })
                            .ok();
                    })),
                    cx,
                );
            }
            _ => {}
        }
    }

    pub(super) fn apply_harness_update(
        &mut self,
        harness: HarnessId,
        target_device_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let key = (target_device_id.clone(), harness);
        if !self.requested_harness_updates.insert(key.clone()) {
            return;
        }
        let Some(handle) = self.state.read(cx).engine().cloned() else {
            self.requested_harness_updates.remove(&key);
            return;
        };
        let device_suffix = target_device_id
            .as_deref()
            .and_then(|device_id| {
                self.state
                    .read(cx)
                    .remote_harness_update_device_names
                    .get(device_id)
                    .map(|name| format!(" on {name}"))
            })
            .unwrap_or_default();
        let toast_scope = target_device_id.clone().unwrap_or_else(|| "local".into());
        crate::toast::show(
            Toast::new(
                format!("harness-update-progress-{toast_scope}-{harness:?}"),
                format!("Updating {}{device_suffix}", harness_label(harness)),
                ToastKind::Info,
            )
            .persistent()
            .body("Idle harness processes will retire immediately; active work can finish."),
            cx,
        );
        let request = ApplyHarnessUpdate {
            harness,
            target_device_id: target_device_id.clone(),
        };
        let task_key = key.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = call_api(handle.client(), &request).await;
            if let Err(error) = result {
                this.update(cx, |shell, cx| {
                    shell.requested_harness_updates.remove(&key);
                    let shell_handle = cx.entity().downgrade();
                    crate::toast::show(
                        Toast::new(
                            format!("harness-update-progress-{toast_scope}-{harness:?}"),
                            format!("{} update failed{device_suffix}", harness_label(harness)),
                            ToastKind::Error,
                        )
                        .persistent()
                        .body(error.to_string())
                        .action(ToastAction::new("Retry", move |cx| {
                            let target_device_id = target_device_id.clone();
                            shell_handle
                                .update(cx, |shell, cx| {
                                    shell.apply_harness_update(harness, target_device_id, cx)
                                })
                                .ok();
                        })),
                        cx,
                    );
                })
                .ok();
            }
        });
        self.harness_update_tasks.insert(task_key, task);
    }

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
