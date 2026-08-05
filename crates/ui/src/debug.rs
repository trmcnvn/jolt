//! Compile-time-gated performance HUD for local diagnostics.
//!
//! This module is absent from ordinary release builds. Enable it in an
//! optimized build with `--features debug-ui`. Toggle it with
//! Cmd/Ctrl+Shift+F12, or open it at launch with `JOLT_PERFORMANCE_HUD=1`.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use gpui::profiler::{self, FrameTimingCollector};
use gpui::{
    Bounds, Context, Render, SharedString, Task, Window, WindowId, actions, canvas, div, fill,
    point, prelude::*, px, size,
};

use crate::theme::Theme;

actions!(debug, [TogglePerformanceHud]);

const POLL_INTERVAL: Duration = Duration::from_millis(250);
const HISTORY_WINDOW: Duration = Duration::from_secs(2);
const ACTIVE_WINDOW: Duration = Duration::from_secs(1);
const ACTIVE_INTERVAL_MAX: Duration = Duration::from_millis(100);
const IDLE_AFTER: Duration = Duration::from_millis(500);
const MIN_ACTIVE_INTERVALS: usize = 3;
const MAX_SAMPLES: usize = 1_024;
const GRAPH_BUCKETS: usize = 120;
const GRAPH_HEIGHT: f32 = 72.0;
const GRAPH_MAX_MS: f64 = 33.333;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct GraphBucket {
    interval: Option<Duration>,
    draw: Option<Duration>,
}

pub(crate) fn performance_hud_requested() -> bool {
    std::env::var_os("JOLT_PERFORMANCE_HUD").is_some_and(|value| !value.is_empty() && value != "0")
}

#[derive(Clone, Copy)]
struct FrameSample {
    at: Instant,
    draw: Duration,
    dirty_to_draw: Option<Duration>,
}

#[derive(Clone, Copy, Default)]
struct PerformanceSnapshot {
    active_fps: Option<f64>,
    interval_p50: Option<Duration>,
    interval_p95: Option<Duration>,
    draw_p50: Option<Duration>,
    draw_p95: Option<Duration>,
    dirty_to_draw_p95: Option<Duration>,
}

struct GpuInfo {
    device: SharedString,
    backend: &'static str,
    driver: Option<SharedString>,
    software_emulated: bool,
    allocated_bytes: Option<u64>,
}

/// Small in-window diagnostics overlay. It only exists in debug builds or when
/// the non-default `debug-ui` feature is explicitly enabled.
pub struct PerformanceHud {
    collector: FrameTimingCollector,
    window_id: Option<WindowId>,
    samples: VecDeque<FrameSample>,
    snapshot: PerformanceSnapshot,
    gpu: Option<GpuInfo>,
    #[cfg(target_os = "macos")]
    metal_device: Option<metal::Device>,
    owns_frame_trace: bool,
    _poll: Task<()>,
}

impl PerformanceHud {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let owns_frame_trace = profiler::set_frame_trace_enabled(true);
        #[cfg(target_os = "macos")]
        let metal_device = metal::Device::system_default();
        #[cfg(target_os = "macos")]
        let gpu = metal_device.as_ref().map(|device| GpuInfo {
            device: device.name().to_owned().into(),
            backend: "Metal",
            driver: None,
            software_emulated: false,
            allocated_bytes: Some(device.current_allocated_size() as u64),
        });
        #[cfg(not(target_os = "macos"))]
        let gpu = None;

        let poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;
                if this
                    .update(cx, |hud, cx| {
                        hud.collect(Instant::now());
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });

        Self {
            collector: FrameTimingCollector::new(),
            window_id: None,
            samples: VecDeque::new(),
            snapshot: PerformanceSnapshot::default(),
            gpu,
            #[cfg(target_os = "macos")]
            metal_device,
            owns_frame_trace,
            _poll: poll,
        }
    }

    fn collect(&mut self, now: Instant) {
        let Some(window_id) = self.window_id else {
            return;
        };
        self.samples.extend(
            self.collector
                .collect_unseen()
                .into_iter()
                .filter(|timing| timing.window_id == window_id)
                .map(|timing| FrameSample {
                    at: timing.draw_start,
                    draw: timing.draw_duration(),
                    dirty_to_draw: timing.dirty_to_draw_duration(),
                }),
        );
        let cutoff = now.checked_sub(HISTORY_WINDOW).unwrap_or(now);
        while self
            .samples
            .front()
            .is_some_and(|sample| sample.at < cutoff)
        {
            self.samples.pop_front();
        }
        while self.samples.len() > MAX_SAMPLES {
            self.samples.pop_front();
        }
        self.snapshot = summarize(&self.samples, now);

        #[cfg(target_os = "macos")]
        if let (Some(device), Some(gpu)) = (&self.metal_device, &mut self.gpu) {
            gpu.allocated_bytes = Some(device.current_allocated_size() as u64);
        }
    }

    fn discover_gpu(&mut self, window: &Window) {
        if self.gpu.is_some() {
            return;
        }
        self.gpu = window.gpu_specs().map(|specs| GpuInfo {
            device: specs.device_name.into(),
            backend: graphics_backend(),
            driver: (!specs.driver_name.is_empty()).then(|| specs.driver_name.into()),
            software_emulated: specs.is_software_emulated,
            allocated_bytes: None,
        });
    }
}

impl Drop for PerformanceHud {
    fn drop(&mut self) {
        if self.owns_frame_trace {
            profiler::set_frame_trace_enabled(false);
        }
    }
}

impl Render for PerformanceHud {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.window_id = Some(window.window_handle().window_id());
        self.discover_gpu(window);

        let theme = Theme::of(cx);
        let status = self
            .snapshot
            .active_fps
            .map_or_else(|| "Idle".to_owned(), |fps| format!("{fps:.0} FPS active"));
        let build = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        };

        let graph = frame_time_graph(
            graph_buckets(&self.samples, Instant::now(), GRAPH_BUCKETS),
            theme,
        );
        let mut card = div()
            .absolute()
            .top(px(52.0))
            .right(px(12.0))
            .w(px(300.0))
            .p(px(12.0))
            .flex()
            .flex_col()
            .gap(px(5.0))
            .rounded(px(10.0))
            .border_1()
            .border_color(theme.border_strong)
            .bg(theme.surface_overlay.opacity(0.96))
            .font_family(theme.font_mono.clone())
            .text_size(px(11.0))
            .text_color(theme.text_muted)
            .child(
                div()
                    .flex()
                    .justify_between()
                    .text_color(theme.text)
                    .child("PERFORMANCE")
                    .child(build),
            )
            .child(metric_row("Frame rate", status, theme))
            .child(metric_row(
                "Frame interval p50 / p95",
                format_duration_pair(self.snapshot.interval_p50, self.snapshot.interval_p95),
                theme,
            ))
            .child(metric_row(
                "UI draw p50 / p95",
                format_duration_pair(self.snapshot.draw_p50, self.snapshot.draw_p95),
                theme,
            ))
            .child(metric_row(
                "Dirty → draw p95",
                format_duration(self.snapshot.dirty_to_draw_p95),
                theme,
            ))
            .child(
                div()
                    .mt(px(4.0))
                    .flex()
                    .justify_between()
                    .child("FRAME TIME")
                    .child("2 seconds"),
            )
            .child(graph)
            .child(
                div()
                    .flex()
                    .justify_between()
                    .text_color(theme.text_faint)
                    .child("8.3 · 16.7 · 33.3 ms")
                    .child("interval · draw"),
            );

        card = if let Some(gpu) = &self.gpu {
            let kind = if gpu.software_emulated {
                "software"
            } else {
                "hardware"
            };
            let identity = gpu.driver.as_ref().map_or_else(
                || format!("{} · {kind}", gpu.backend),
                |driver| format!("{} · {driver} · {kind}", gpu.backend),
            );
            card.child(div().mt(px(4.0)).h(px(1.0)).bg(theme.border))
                .child(metric_row("GPU", gpu.device.clone(), theme))
                .child(metric_row("Backend", identity, theme))
                .child(metric_row(
                    "Process GPU allocation",
                    gpu.allocated_bytes
                        .map_or_else(|| "Unavailable".to_owned(), |bytes| format_bytes(bytes)),
                    theme,
                ))
        } else {
            card.child(div().mt(px(4.0)).h(px(1.0)).bg(theme.border))
                .child(metric_row("GPU", "Unavailable", theme))
        };

        card
    }
}

fn metric_row(
    label: impl Into<SharedString>,
    value: impl Into<SharedString>,
    theme: &Theme,
) -> gpui::Div {
    div()
        .flex()
        .justify_between()
        .gap(px(12.0))
        .child(label.into())
        .child(div().text_color(theme.text).child(value.into()))
}

fn frame_time_graph(buckets: Vec<GraphBucket>, theme: &Theme) -> impl IntoElement {
    let guide_color = theme.border.opacity(0.75);
    let interval_color = theme.text_faint.opacity(0.38);
    let draw_color = theme.accent.opacity(0.9);
    let spike_color = theme.danger.opacity(0.9);

    div()
        .relative()
        .w_full()
        .h(px(GRAPH_HEIGHT))
        .overflow_hidden()
        .rounded(px(5.0))
        .bg(crate::theme::wash(0.035))
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    paint_frame_time_graph(
                        bounds,
                        &buckets,
                        guide_color,
                        interval_color,
                        draw_color,
                        spike_color,
                        window,
                    );
                },
            )
            .absolute()
            .size_full(),
        )
}

fn paint_frame_time_graph(
    bounds: Bounds<gpui::Pixels>,
    buckets: &[GraphBucket],
    guide_color: gpui::Hsla,
    interval_color: gpui::Hsla,
    draw_color: gpui::Hsla,
    spike_color: gpui::Hsla,
    window: &mut Window,
) {
    let origin_x = f32::from(bounds.origin.x);
    let origin_y = f32::from(bounds.origin.y);
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    if buckets.is_empty() || width <= 0.0 || height <= 0.0 {
        return;
    }

    for guide_ms in [8.333, 16.667, GRAPH_MAX_MS] {
        let y = origin_y + height * (1.0 - (guide_ms / GRAPH_MAX_MS) as f32);
        window.paint_quad(fill(
            Bounds::new(point(px(origin_x), px(y)), size(px(width), px(0.5))),
            guide_color,
        ));
    }

    let bucket_width = width / buckets.len() as f32;
    let interval_width = (bucket_width - 0.5).max(0.5);
    let draw_width = (interval_width * 0.45).max(0.5);
    for (index, bucket) in buckets.iter().enumerate() {
        let x = origin_x + bucket_width * index as f32;
        if let Some(interval) = bucket.interval {
            let interval_ms = interval.as_secs_f64() * 1_000.0;
            let bar_height = ((interval_ms / GRAPH_MAX_MS).min(1.0) as f32 * height).max(0.5);
            window.paint_quad(fill(
                Bounds::new(
                    point(px(x), px(origin_y + height - bar_height)),
                    size(px(interval_width), px(bar_height)),
                ),
                interval_color,
            ));
            if interval_ms > GRAPH_MAX_MS {
                window.paint_quad(fill(
                    Bounds::new(
                        point(px(x), px(origin_y)),
                        size(px(interval_width), px(1.5)),
                    ),
                    spike_color,
                ));
            }
        }
        if let Some(draw) = bucket.draw {
            let draw_ms = draw.as_secs_f64() * 1_000.0;
            let bar_height = ((draw_ms / GRAPH_MAX_MS).min(1.0) as f32 * height).max(0.5);
            let draw_x = x + (interval_width - draw_width) * 0.5;
            window.paint_quad(fill(
                Bounds::new(
                    point(px(draw_x), px(origin_y + height - bar_height)),
                    size(px(draw_width), px(bar_height)),
                ),
                draw_color,
            ));
            if draw_ms > GRAPH_MAX_MS {
                window.paint_quad(fill(
                    Bounds::new(
                        point(px(draw_x), px(origin_y)),
                        size(px(draw_width), px(1.5)),
                    ),
                    spike_color,
                ));
            }
        }
    }
}

fn graph_buckets(
    samples: &VecDeque<FrameSample>,
    now: Instant,
    bucket_count: usize,
) -> Vec<GraphBucket> {
    let mut buckets = vec![GraphBucket::default(); bucket_count];
    if bucket_count == 0 {
        return buckets;
    }
    let cutoff = now.checked_sub(HISTORY_WINDOW).unwrap_or(now);
    let ordered: Vec<_> = samples.iter().copied().collect();
    let intervals: Vec<_> = ordered
        .windows(2)
        .map(|pair| pair[1].at.duration_since(pair[0].at))
        .collect();

    for (index, interval) in intervals.iter().copied().enumerate() {
        let current = ordered[index + 1];
        if current.at < cutoff || current.at > now || !graphable_interval(&intervals, index) {
            continue;
        }
        let position =
            current.at.duration_since(cutoff).as_secs_f64() / HISTORY_WINDOW.as_secs_f64();
        let bucket_index = ((position * bucket_count as f64) as usize).min(bucket_count - 1);
        let bucket = &mut buckets[bucket_index];
        bucket.interval = Some(
            bucket
                .interval
                .map_or(interval, |value| value.max(interval)),
        );
        bucket.draw = Some(
            bucket
                .draw
                .map_or(current.draw, |value| value.max(current.draw)),
        );
    }
    buckets
}

fn graphable_interval(intervals: &[Duration], index: usize) -> bool {
    let interval = intervals[index];
    interval <= ACTIVE_INTERVAL_MAX
        || (index > 0
            && index + 1 < intervals.len()
            && intervals[index - 1] <= ACTIVE_INTERVAL_MAX
            && intervals[index + 1] <= ACTIVE_INTERVAL_MAX)
}

fn summarize(samples: &VecDeque<FrameSample>, now: Instant) -> PerformanceSnapshot {
    let active_cutoff = now.checked_sub(ACTIVE_WINDOW).unwrap_or(now);
    let mut intervals = Vec::new();
    let mut active_samples = Vec::new();
    let mut last_active_at = None;

    let ordered: Vec<_> = samples.iter().copied().collect();
    for pair in ordered.windows(2) {
        let [previous, current] = pair else { continue };
        let interval = current.at.duration_since(previous.at);
        if current.at >= active_cutoff && interval <= ACTIVE_INTERVAL_MAX {
            intervals.push(interval);
            active_samples.push(*current);
            last_active_at = Some(current.at);
        }
    }

    let active = intervals.len() >= MIN_ACTIVE_INTERVALS
        && last_active_at.is_some_and(|at| now.duration_since(at) <= IDLE_AFTER);
    if !active {
        return PerformanceSnapshot::default();
    }

    let interval_p50 = percentile(&intervals, 0.50);
    let draws: Vec<_> = active_samples.iter().map(|sample| sample.draw).collect();
    let dirty_to_draw: Vec<_> = active_samples
        .iter()
        .filter_map(|sample| sample.dirty_to_draw)
        .collect();
    PerformanceSnapshot {
        active_fps: interval_p50.map(|interval| 1.0 / interval.as_secs_f64()),
        interval_p50,
        interval_p95: percentile(&intervals, 0.95),
        draw_p50: percentile(&draws, 0.50),
        draw_p95: percentile(&draws, 0.95),
        dirty_to_draw_p95: percentile(&dirty_to_draw, 0.95),
    }
}

fn percentile(values: &[Duration], quantile: f64) -> Option<Duration> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let index = ((sorted.len() as f64 * quantile).ceil() as usize)
        .saturating_sub(1)
        .min(sorted.len() - 1);
    sorted.get(index).copied()
}

fn format_duration(duration: Option<Duration>) -> String {
    duration.map_or_else(
        || "—".to_owned(),
        |duration| format!("{:.1} ms", duration.as_secs_f64() * 1_000.0),
    )
}

fn format_duration_pair(first: Option<Duration>, second: Option<Duration>) -> String {
    match (first, second) {
        (Some(first), Some(second)) => format!(
            "{:.1} / {:.1} ms",
            first.as_secs_f64() * 1_000.0,
            second.as_secs_f64() * 1_000.0
        ),
        _ => "—".to_owned(),
    }
}

fn format_bytes(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    format!("{:.1} MiB", bytes as f64 / MIB)
}

const fn graphics_backend() -> &'static str {
    if cfg!(target_os = "windows") {
        "DirectX"
    } else if cfg!(target_os = "linux") {
        "WGPU"
    } else if cfg!(target_os = "macos") {
        "Metal"
    } else {
        "GPUI"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(at: Instant, draw_ms: u64) -> FrameSample {
        FrameSample {
            at,
            draw: Duration::from_millis(draw_ms),
            dirty_to_draw: Some(Duration::from_millis(draw_ms + 1)),
        }
    }

    #[test]
    fn sustained_frames_report_active_fps_and_percentiles() {
        let start = Instant::now();
        let samples = (0..8)
            .map(|index| sample(start + Duration::from_millis(index * 10), index + 1))
            .collect();
        let snapshot = summarize(&samples, start + Duration::from_millis(75));

        assert_eq!(snapshot.active_fps, Some(100.0));
        assert_eq!(snapshot.interval_p50, Some(Duration::from_millis(10)));
        assert_eq!(snapshot.draw_p50, Some(Duration::from_millis(5)));
        assert_eq!(snapshot.draw_p95, Some(Duration::from_millis(8)));
        assert_eq!(snapshot.dirty_to_draw_p95, Some(Duration::from_millis(9)));
    }

    #[test]
    fn sparse_event_driven_frames_report_idle() {
        let start = Instant::now();
        let samples = (0..5)
            .map(|index| sample(start + Duration::from_millis(index * 250), 2))
            .collect();
        let snapshot = summarize(&samples, start + Duration::from_secs(1));

        assert!(snapshot.active_fps.is_none());
        assert!(snapshot.draw_p95.is_none());
    }

    #[test]
    fn old_active_burst_expires() {
        let start = Instant::now();
        let samples = (0..8)
            .map(|index| sample(start + Duration::from_millis(index * 8), 2))
            .collect();
        let snapshot = summarize(&samples, start + Duration::from_secs(2));

        assert!(snapshot.active_fps.is_none());
    }

    #[test]
    fn graph_buckets_preserve_the_largest_spike() {
        let start = Instant::now();
        let samples = VecDeque::from([
            sample(start, 1),
            sample(start + Duration::from_millis(10), 2),
            sample(start + Duration::from_millis(90), 25),
            sample(start + Duration::from_millis(100), 3),
        ]);
        let buckets = graph_buckets(&samples, start + Duration::from_millis(100), 1);

        assert_eq!(buckets[0].interval, Some(Duration::from_millis(80)));
        assert_eq!(buckets[0].draw, Some(Duration::from_millis(25)));
    }

    #[test]
    fn graph_keeps_a_long_stall_inside_an_active_burst() {
        let start = Instant::now();
        let samples = VecDeque::from([
            sample(start, 1),
            sample(start + Duration::from_millis(10), 2),
            sample(start + Duration::from_millis(130), 30),
            sample(start + Duration::from_millis(140), 2),
        ]);
        let buckets = graph_buckets(&samples, start + Duration::from_millis(140), 1);

        assert_eq!(buckets[0].interval, Some(Duration::from_millis(120)));
    }

    #[test]
    fn graph_leaves_event_driven_idle_gaps_blank() {
        let start = Instant::now();
        let samples = VecDeque::from([
            sample(start, 1),
            sample(start + Duration::from_millis(250), 2),
            sample(start + Duration::from_millis(500), 2),
            sample(start + Duration::from_millis(750), 2),
        ]);
        let buckets = graph_buckets(&samples, start + Duration::from_millis(750), 8);

        assert!(buckets.iter().all(|bucket| bucket.interval.is_none()));
    }

    #[test]
    fn bytes_are_labeled_as_binary_megabytes() {
        assert_eq!(format_bytes(1572864), "1.5 MiB");
    }
}
