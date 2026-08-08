//! Shell usage behavior.

use super::*;

impl Shell {
    pub(super) fn open_breakdown(&mut self, cx: &mut Context<Self>) {
        self.user_menu_open = false;
        self.breakdown_dialog = Some(BreakdownDialog {
            days: 30,
            data: Loadable::Loading,
            unavailable_devices: 0,
            task: None,
        });
        self.load_breakdown(30, cx);
    }

    pub(super) fn load_breakdown(&mut self, days: u16, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            if let Some(dialog) = &mut self.breakdown_dialog {
                dialog.data = Loadable::Error("Engine not connected".into());
            }
            cx.notify();
            return;
        };
        let (remote_devices, offline_devices) = {
            let state = self.state.read(cx);
            state
                .local_device_id
                .as_ref()
                .map(|local_id| {
                    let remotes: Vec<_> = state
                        .devices
                        .iter()
                        .filter(|device| device.id != *local_id && device.is_engine_host())
                        .collect();
                    let online = remotes
                        .iter()
                        .filter(|device| state.device_online(&device.id, Utc::now()))
                        .map(|device| device.id.clone())
                        .collect::<Vec<_>>();
                    let offline = remotes.len().saturating_sub(online.len());
                    (online, offline)
                })
                .unwrap_or_default()
        };
        if let Some(dialog) = &mut self.breakdown_dialog {
            dialog.days = days;
            // Keep the previous report visible while switching ranges. Replacing
            // it with the shorter loading state made the modal collapse and
            // expand around every request.
            if !matches!(&dialog.data, Loadable::Ready(_)) {
                dialog.data = Loadable::Loading;
                dialog.unavailable_devices = 0;
            }
            dialog.task = Some(cx.spawn(async move |this, cx| {
                let mut targets = vec![None];
                targets.extend(remote_devices.into_iter().map(Some));
                let mut replies = Vec::new();
                let mut unavailable = offline_devices;
                for target in targets {
                    let request = UsageBreakdownRequest {
                        days,
                        target_device_id: target,
                    };
                    match call_api(engine.client(), &request).await {
                        Ok(reply) => replies.push(reply),
                        Err(error) => {
                            tracing::warn!(%error, "usage breakdown device unavailable");
                            unavailable += 1;
                        }
                    }
                }
                this.update(cx, |shell, cx| {
                    let Some(dialog) = &mut shell.breakdown_dialog else {
                        return;
                    };
                    if dialog.days != days {
                        return;
                    }
                    dialog.task = None;
                    dialog.unavailable_devices = unavailable;
                    dialog.data = if replies.is_empty() {
                        Loadable::Error("Usage data is unavailable".into())
                    } else {
                        Loadable::Ready(merge_breakdowns(days, replies))
                    };
                    cx.notify();
                })
                .ok();
            }));
        }
        cx.notify();
    }

    pub(super) fn render_breakdown_dialog(
        &mut self,
        viewport: gpui::Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let dialog = self.breakdown_dialog.as_ref()?;
        let days = dialog.days;
        let data = dialog.data.clone();
        let unavailable = dialog.unavailable_devices;
        let refreshing = dialog.task.is_some() && matches!(&data, Loadable::Ready(_));
        let theme = Theme::of(cx).clone();

        let ranges = div()
            .flex()
            .items_center()
            .rounded(px(7.0))
            .border_1()
            .border_color(theme.border)
            .overflow_hidden()
            .children([7_u16, 30, 90].into_iter().map(|range| {
                div()
                    .id(("breakdown-range", range as usize))
                    .px(px(11.0))
                    .h(px(28.0))
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .text_size(px(11.0))
                    .text_color(if days == range {
                        theme.text
                    } else {
                        theme.text_muted
                    })
                    .bg(if days == range {
                        theme.element_active
                    } else {
                        gpui::transparent_black()
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.load_breakdown(range, cx);
                    }))
                    .child(format!("{range} days"))
            }));

        let mut body = div()
            .id("breakdown-body")
            .h(px(610.0))
            .max_h(viewport.height - px(150.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(24.0))
            .opacity(if refreshing { 0.68 } else { 1.0 });
        match data {
            Loadable::Idle | Loadable::Loading => {
                body = body.child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(12.0))
                        .text_color(theme.text_muted)
                        .child("Loading usage…"),
                );
            }
            Loadable::Error(error) => {
                body = body.child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(12.0))
                        .text_color(theme.danger_muted)
                        .child(error),
                );
            }
            Loadable::Ready(breakdown) => {
                let totals = &breakdown.totals;
                let total_tokens = totals.total_tokens();
                let harness_totals = aggregate_harness_usage(&breakdown.rows);
                let harness_rows = harness_totals.into_iter().map(|harness| {
                    let (share, share_basis) = match (totals.cost_usd, harness.cost_usd) {
                        (Some(total), Some(part)) if total > 0.0 => (
                            (part / total).clamp(0.0, 1.0),
                            if harness.cost_provenance.is_some() {
                                "estimated API equivalent"
                            } else {
                                "API-equivalent cost estimate"
                            },
                        ),
                        _ => (usage_share(harness.tokens, total_tokens), "tokens"),
                    };
                    let color = match harness.harness {
                        HarnessId::ClaudeCode => theme.warning,
                        HarnessId::Codex => theme.text,
                        HarnessId::Pi => theme.accent,
                        HarnessId::Mock => theme.text_muted,
                    };
                    let amount = harness
                        .cost_usd
                        .map(|_| format_usage_cost(harness.cost_usd))
                        .unwrap_or_else(|| compact_number(harness.tokens));
                    div()
                        .mt(px(14.0))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .text_size(px(12.0))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(7.0))
                                        .text_color(theme.text)
                                        .child(div().size(px(7.0)).rounded_full().bg(color))
                                        .child(harness_label(harness.harness)),
                                )
                                .child(div().text_color(theme.text).child(amount)),
                        )
                        .child(
                            div()
                                .mt(px(7.0))
                                .h(px(3.0))
                                .rounded_full()
                                .overflow_hidden()
                                .bg(theme.element_hover)
                                .child(
                                    div()
                                        .h_full()
                                        .w(gpui::relative((share as f32).max(0.01)))
                                        .rounded_full()
                                        .bg(color.opacity(0.85)),
                                ),
                        )
                        .child(
                            div()
                                .mt(px(5.0))
                                .text_size(px(10.0))
                                .text_color(theme.text_muted)
                                .child(format!(
                                    "{:.1}% of {share_basis} · {} tokens",
                                    share * 100.0,
                                    compact_number(harness.tokens)
                                )),
                        )
                });

                let by_day: std::collections::HashMap<_, _> = totals
                    .activity
                    .iter()
                    .map(|day| (day.day.as_str(), day.tokens))
                    .collect();
                let daily: Vec<_> = (0..totals.days)
                    .rev()
                    .map(|offset| {
                        let date = chrono::Local::now().date_naive()
                            - chrono::Duration::days(i64::from(offset));
                        let day = date.format("%Y-%m-%d").to_string();
                        let tokens = by_day.get(day.as_str()).copied().unwrap_or_default();
                        (date.format("%b %d").to_string(), tokens)
                    })
                    .collect();
                let max_tokens = daily
                    .iter()
                    .map(|(_, tokens)| *tokens)
                    .max()
                    .unwrap_or(1)
                    .max(1);
                let bars = daily.iter().map(|(_, tokens)| {
                    let intensity = *tokens as f32 / max_tokens as f32;
                    div()
                        .h_full()
                        .min_w(px(1.0))
                        .flex_1()
                        .flex()
                        .items_end()
                        .child(
                            div()
                                .w_full()
                                .h(px(if *tokens == 0 {
                                    2.0
                                } else {
                                    8.0 + 124.0 * intensity
                                }))
                                .rounded(px(2.0))
                                .bg(if *tokens == 0 {
                                    theme.element_hover.opacity(0.55)
                                } else {
                                    theme.accent.opacity(0.28 + 0.72 * intensity)
                                }),
                        )
                });

                body = body.child(
                    div()
                        .flex()
                        .gap(px(28.0))
                        .child(
                            div()
                                .w(px(300.0))
                                .flex_none()
                                .child(
                                    div()
                                        .text_size(px(10.0))
                                        .text_color(theme.text_muted)
                                        .child("ESTIMATED API EQUIVALENT"),
                                )
                                .child(
                                    div()
                                        .mt(px(4.0))
                                        .text_size(px(32.0))
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .text_color(theme.text)
                                        .child(format_usage_cost(totals.cost_usd)),
                                )
                                .child(
                                    div()
                                        .mt(px(3.0))
                                        .text_size(px(10.0))
                                        .text_color(theme.text_muted)
                                        .child(format!(
                                            "{} threads · {} calls · not a subscription charge",
                                            compact_number(totals.sessions),
                                            compact_number(totals.calls)
                                        )),
                                )
                                .children(harness_rows),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_between()
                                        .child(
                                            div()
                                                .text_size(px(12.0))
                                                .font_weight(gpui::FontWeight::MEDIUM)
                                                .text_color(theme.text)
                                                .child("Daily tokens"),
                                        )
                                        .child(
                                            div()
                                                .px(px(7.0))
                                                .py(px(3.0))
                                                .rounded(px(5.0))
                                                .bg(theme.element_hover)
                                                .text_size(px(9.0))
                                                .text_color(theme.text_muted)
                                                .child(format!(
                                                    "PEAK {}",
                                                    compact_number(max_tokens)
                                                )),
                                        ),
                                )
                                .child(
                                    div()
                                        .mt(px(12.0))
                                        .h(px(148.0))
                                        .border_b_1()
                                        .border_color(theme.border)
                                        .flex()
                                        .items_end()
                                        .gap(px(if totals.days > 30 { 1.0 } else { 3.0 }))
                                        .children(bars),
                                )
                                .child(
                                    div()
                                        .mt(px(6.0))
                                        .flex()
                                        .justify_between()
                                        .text_size(px(9.0))
                                        .text_color(theme.text_muted)
                                        .child(
                                            daily
                                                .first()
                                                .map(|(day, _)| day.clone())
                                                .unwrap_or_default(),
                                        )
                                        .child(
                                            daily
                                                .last()
                                                .map(|(day, _)| day.clone())
                                                .unwrap_or_default(),
                                        ),
                                ),
                        ),
                );

                let prompt_tokens = totals
                    .input_tokens
                    .saturating_add(totals.cache_read_input_tokens)
                    .saturating_add(totals.cache_write_input_tokens);
                let metrics = [
                    (
                        "Processed tokens",
                        compact_number(total_tokens),
                        format!("{} calls", compact_number(totals.calls)),
                    ),
                    (
                        "Cached input",
                        compact_number(totals.cache_read_input_tokens),
                        format!(
                            "{:.1}% of prompt",
                            usage_share(totals.cache_read_input_tokens, prompt_tokens) * 100.0
                        ),
                    ),
                    (
                        "Uncached input",
                        compact_number(totals.input_tokens),
                        "Prompt tokens".to_string(),
                    ),
                    (
                        "Output",
                        compact_number(totals.output_tokens),
                        "Generated tokens".to_string(),
                    ),
                    (
                        "Cache writes",
                        compact_number(totals.cache_write_input_tokens),
                        "Stored prompt tokens".to_string(),
                    ),
                ];
                body = body.child(
                    div()
                        .border_t_1()
                        .border_b_1()
                        .border_color(theme.border)
                        .flex()
                        .children(metrics.into_iter().enumerate().map(
                            |(index, (label, value, detail))| {
                                div()
                                    .when(index != 0, |cell| {
                                        cell.border_l_1().border_color(theme.border)
                                    })
                                    .min_w_0()
                                    .flex_1()
                                    .px(px(14.0))
                                    .py(px(12.0))
                                    .child(
                                        div()
                                            .text_size(px(10.0))
                                            .text_color(theme.text_muted)
                                            .child(label),
                                    )
                                    .child(
                                        div()
                                            .mt(px(5.0))
                                            .text_size(px(17.0))
                                            .font_weight(gpui::FontWeight::MEDIUM)
                                            .text_color(theme.text)
                                            .child(value),
                                    )
                                    .child(
                                        div()
                                            .mt(px(3.0))
                                            .truncate()
                                            .text_size(px(9.0))
                                            .text_color(theme.text_muted.opacity(0.7))
                                            .child(detail),
                                    )
                            },
                        )),
                );

                let device_names: std::collections::HashMap<_, _> = {
                    let state = self.state.read(cx);
                    breakdown
                        .rows
                        .iter()
                        .filter_map(|row| {
                            state
                                .device_display_name(&row.device_id)
                                .map(|name| (row.device_id.clone(), name.to_string()))
                        })
                        .collect()
                };
                let table_header = div()
                    .h(px(30.0))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .text_size(px(9.0))
                    .text_color(theme.text_muted)
                    .child(div().min_w_0().flex_1().child("Model"))
                    .child(div().w(px(92.0)).child("Harness"))
                    .child(div().w(px(100.0)).child("Space"))
                    .child(div().w(px(110.0)).child("Device"))
                    .child(div().w(px(76.0)).text_right().child("Est. API cost"))
                    .child(div().w(px(50.0)).text_right().child("Share"))
                    .child(div().w(px(70.0)).text_right().child("Tokens"));
                let mut table = div()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(table_header);
                for device_row in &breakdown.rows {
                    let row = &device_row.usage;
                    let location = std::path::Path::new(&row.cwd)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or(&row.cwd);
                    let device = device_names
                        .get(&device_row.device_id)
                        .map(String::as_str)
                        .unwrap_or(&device_row.device_id);
                    let color = match row.harness {
                        HarnessId::ClaudeCode => theme.warning,
                        HarnessId::Codex => theme.text,
                        HarnessId::Pi => theme.accent,
                        HarnessId::Mock => theme.text_muted,
                    };
                    table = table.child(
                        div()
                            .h(px(38.0))
                            .border_t_1()
                            .border_color(theme.border)
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .text_size(px(11.0))
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .flex()
                                    .items_center()
                                    .gap(px(7.0))
                                    .truncate()
                                    .text_color(theme.text)
                                    .child(div().size(px(6.0)).flex_none().rounded_full().bg(color))
                                    .child(row.model.clone()),
                            )
                            .child(
                                div()
                                    .w(px(92.0))
                                    .truncate()
                                    .text_color(theme.text_muted)
                                    .child(harness_label(row.harness)),
                            )
                            .child(
                                div()
                                    .w(px(100.0))
                                    .truncate()
                                    .text_color(theme.text_muted)
                                    .child(location.to_string()),
                            )
                            .child(
                                div()
                                    .w(px(110.0))
                                    .truncate()
                                    .text_color(theme.text_muted)
                                    .child(device.to_string()),
                            )
                            .child(
                                div()
                                    .w(px(76.0))
                                    .text_right()
                                    .text_color(theme.text)
                                    .child(format_usage_cost(row.cost_usd)),
                            )
                            .child(
                                div()
                                    .w(px(50.0))
                                    .text_right()
                                    .text_color(theme.text_muted)
                                    .child(format!(
                                        "{:.1}%",
                                        usage_share(row.total_tokens(), total_tokens) * 100.0
                                    )),
                            )
                            .child(
                                div()
                                    .w(px(70.0))
                                    .text_right()
                                    .text_color(theme.text)
                                    .child(compact_number(row.total_tokens())),
                            ),
                    );
                }
                body = body.child(
                    div()
                        .child(
                            div()
                                .mb(px(8.0))
                                .flex()
                                .items_end()
                                .justify_between()
                                .child(
                                    div()
                                        .text_size(px(13.0))
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .text_color(theme.text)
                                        .child("Breakdown"),
                                )
                                .child(
                                    div().text_size(px(9.0)).text_color(theme.text_muted).child(
                                        format!(
                                            "{} model · space · device rows",
                                            breakdown.rows.len()
                                        ),
                                    ),
                                ),
                        )
                        .child(table),
                );
                if unavailable != 0 {
                    body = body.child(div().text_size(px(10.0)).text_color(theme.warning).child(
                        format!(
                            "{unavailable} device(s) unavailable or did not return usage data."
                        ),
                    ));
                }
            }
        }

        let card = div()
            .w(px(980.0))
            .max_w(viewport.width - px(32.0))
            .p(px(22.0))
            .rounded(px(14.0))
            .border_1()
            .border_color(theme.border_strong)
            .bg(theme.surface_dialog)
            .shadow_lg()
            .child(
                div()
                    .mb(px(22.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .child(
                                div()
                                    .text_size(px(20.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme.text)
                                    .child("Usage"),
                            )
                            .child(
                                div()
                                    .mt(px(3.0))
                                    .text_size(px(11.0))
                                    .text_color(theme.text_muted)
                                    .child(format!(
                                        "{} · reachable devices",
                                        usage_date_range(days)
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(ranges)
                            .child(
                                div()
                                    .id("refresh-breakdown")
                                    .size(px(28.0))
                                    .rounded(px(7.0))
                                    .cursor_pointer()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .bg(motion::hover_blend(
                                        "refresh-breakdown",
                                        gpui::transparent_black(),
                                        theme.element_hover,
                                    ))
                                    .on_hover(motion::hover_listener("refresh-breakdown"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if this
                                            .breakdown_dialog
                                            .as_ref()
                                            .is_some_and(|dialog| dialog.task.is_none())
                                        {
                                            this.load_breakdown(days, cx);
                                        }
                                    }))
                                    .child(
                                        icon(icons::REFRESH)
                                            .size(px(14.0))
                                            .text_color(theme.text_muted),
                                    ),
                            )
                            .child(
                                div()
                                    .id("close-breakdown")
                                    .size(px(28.0))
                                    .rounded(px(7.0))
                                    .cursor_pointer()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .bg(motion::hover_blend(
                                        "close-breakdown",
                                        gpui::transparent_black(),
                                        theme.element_hover,
                                    ))
                                    .on_hover(motion::hover_listener("close-breakdown"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.breakdown_dialog = None;
                                        cx.notify();
                                    }))
                                    .child(
                                        icon(icons::X).size(px(15.0)).text_color(theme.text_muted),
                                    ),
                            ),
                    ),
            )
            .child(body);
        Some(popover::dismissible_modal(
            "usage-breakdown",
            viewport,
            card.into_any_element(),
            cx.listener(|this, _, _, cx| {
                this.breakdown_dialog = None;
                cx.notify();
            }),
        ))
    }
}
