//! Native source-to-SVG adapters for chat math and Mermaid fences.
//!
//! Both engines are deterministic and network-free. The UI rasterizes the SVG
//! through GPUI's existing renderer and caches the resulting `RenderImage`.

use anyhow::{Context as _, Result, anyhow};
use merman::render::{HeadlessRenderer, HostThemeOutput, HostThemeProfile, HostThemeRoles};
use ratex_layout::{LayoutOptions, layout, to_display_list};
use ratex_svg::{SvgOptions, render_to_svg};
use ratex_types::{Color, MathStyle};

#[derive(Debug, Clone)]
pub struct RichSvg {
    pub svg: String,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone)]
pub struct MermaidColors<'a> {
    pub canvas: &'a str,
    pub surface: &'a str,
    pub surface_alt: &'a str,
    pub text: &'a str,
    pub muted: &'a str,
    pub border: &'a str,
    pub line: &'a str,
    pub accent: &'a str,
}

pub fn render_math(source: &str, inline: bool, color: Color, font_size: f32) -> Result<RichSvg> {
    if source.trim().is_empty() {
        return Err(anyhow!("empty formula"));
    }
    let ast = ratex_parser::parse(source).context("invalid TeX")?;
    let style = if inline {
        MathStyle::Text
    } else {
        MathStyle::Display
    };
    let layout = layout(
        &ast,
        &LayoutOptions::default().with_style(style).with_color(color),
    );
    let list = to_display_list(&layout);
    let padding = if inline { 1.0 } else { 8.0 };
    let options = SvgOptions {
        font_size: f64::from(font_size),
        padding,
        stroke_width: 1.0,
        embed_glyphs: true,
        font_dir: String::new(),
    };
    let width = (list.width * options.font_size + 2.0 * padding).max(1.0) as f32;
    let height = ((list.height + list.depth) * options.font_size + 2.0 * padding).max(1.0) as f32;
    Ok(RichSvg {
        svg: render_to_svg(&list, &options),
        width,
        height,
    })
}

pub fn render_mermaid(
    source: &str,
    colors: &MermaidColors<'_>,
    font_family: &str,
    id: &str,
) -> Result<RichSvg> {
    if source.trim().is_empty() {
        return Err(anyhow!("empty Mermaid diagram"));
    }
    let font_family = mermaid_font_family(font_family);
    let profile = HostThemeProfile::builder()
        .font_family(font_family)
        .roles(HostThemeRoles {
            canvas: Some(colors.canvas.to_string()),
            surface: Some(colors.surface.to_string()),
            surface_alt: Some(colors.surface_alt.to_string()),
            surface_muted: Some(colors.surface_alt.to_string()),
            text: Some(colors.text.to_string()),
            subtle_text: Some(colors.muted.to_string()),
            border: Some(colors.border.to_string()),
            line: Some(colors.line.to_string()),
            edge_label_background: Some(colors.canvas.to_string()),
            cluster_background: Some(colors.surface.to_string()),
            cluster_border: Some(colors.border.to_string()),
            note_background: Some(colors.surface_alt.to_string()),
            note_border: Some(colors.accent.to_string()),
            note_text: Some(colors.text.to_string()),
            actor_background: Some(colors.surface.to_string()),
            actor_border: Some(colors.border.to_string()),
            actor_text: Some(colors.text.to_string()),
            activation_background: Some(colors.surface_alt.to_string()),
            activation_border: Some(colors.accent.to_string()),
            ..HostThemeRoles::default()
        })
        .series_palette([
            colors.accent.to_string(),
            colors.line.to_string(),
            colors.muted.to_string(),
        ])
        .output(HostThemeOutput::resvg_safe_editor())
        .build();
    let renderer = HeadlessRenderer::new()
        .with_host_theme(&profile)
        .with_vendored_text_measurer()
        .with_diagram_id(id);
    let svg = renderer
        .render_svg_sync(source)
        .context("invalid Mermaid")?
        .ok_or_else(|| anyhow!("no Mermaid diagram found"))?;
    let (width, height) = svg_dimensions(&svg).unwrap_or((640.0, 360.0));
    Ok(RichSvg { svg, width, height })
}

/// Use the resolved UI font for labels, with embedded Geist as a deterministic
/// fallback for GPUI's separate SVG font database. `sans-serif` gives merman a
/// conservative measurement fallback and covers glyphs absent from both faces.
fn mermaid_font_family(font_family: &str) -> String {
    let font_family = gpui::font_name_with_fallbacks(font_family.trim(), "system-ui");
    let mut families = font_family
        .split(',')
        .map(str::trim)
        .filter(|family| !family.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if !families
        .iter()
        .any(|family| family.eq_ignore_ascii_case("Geist"))
    {
        let before_generic = families
            .iter()
            .position(|family| family.eq_ignore_ascii_case("sans-serif"))
            .unwrap_or(families.len());
        families.insert(before_generic, "Geist".to_string());
    }
    if !families
        .iter()
        .any(|family| family.eq_ignore_ascii_case("sans-serif"))
    {
        families.push("sans-serif".to_string());
    }
    families.join(", ")
}

fn svg_dimensions(svg: &str) -> Option<(f32, f32)> {
    if let Some(view_box) = svg_attribute(svg, "viewBox") {
        let values: Vec<f32> = view_box
            .split(|ch: char| ch.is_ascii_whitespace() || ch == ',')
            .filter(|part| !part.is_empty())
            .filter_map(|part| part.parse().ok())
            .collect();
        if values.len() == 4 && values[2] > 0.0 && values[3] > 0.0 {
            return Some((values[2], values[3]));
        }
    }
    let width = svg_attribute(svg, "width")?
        .trim_end_matches("px")
        .parse::<f32>()
        .ok()?;
    let height = svg_attribute(svg, "height")?
        .trim_end_matches("px")
        .parse::<f32>()
        .ok()?;
    (width > 0.0 && height > 0.0).then_some((width, height))
}

fn svg_attribute<'a>(svg: &'a str, name: &str) -> Option<&'a str> {
    let start_tag = svg.get(..svg.find('>')?)?;
    let needle = format!("{name}=\"");
    let start = start_tag.find(&needle)? + needle.len();
    let end = start_tag[start..].find('"')? + start;
    Some(&start_tag[start..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_formula_to_sized_standalone_svg() {
        let rendered = render_math(
            r"\frac{-b \pm \sqrt{b^2-4ac}}{2a}",
            false,
            Color::WHITE,
            16.0,
        )
        .unwrap();
        assert!(rendered.svg.starts_with("<svg"));
        assert!(rendered.svg.contains("<path"));
        assert!(rendered.width > 20.0);
        assert!(rendered.height > 16.0);
    }

    #[test]
    fn invalid_formula_is_an_error() {
        assert!(render_math(r"\frac{", false, Color::WHITE, 16.0).is_err());
    }

    #[test]
    fn renders_mermaid_without_html_labels() {
        let colors = MermaidColors {
            canvas: "#080808",
            surface: "#141414",
            surface_alt: "#202020",
            text: "#eeeeee",
            muted: "#aaaaaa",
            border: "#555555",
            line: "#999999",
            accent: "#818cf8",
        };
        let rendered = render_mermaid(
            "flowchart LR\n A[Start] --> B[Done]",
            &colors,
            "Avenir Next",
            "test",
        )
        .unwrap();
        assert!(rendered.svg.starts_with("<svg"));
        assert!(!rendered.svg.contains("<foreignObject"));
        assert!(rendered.svg.contains("Avenir Next,Geist,sans-serif"));
        assert!(rendered.width > 0.0 && rendered.height > 0.0);
    }

    #[test]
    fn mermaid_font_stack_uses_geist_once_and_keeps_sans_fallback() {
        assert_eq!(mermaid_font_family("Geist"), "Geist, sans-serif");
        assert_eq!(
            mermaid_font_family("Custom Font, sans-serif"),
            "Custom Font, Geist, sans-serif"
        );
        assert_eq!(
            mermaid_font_family(".SystemUIFont"),
            "system-ui, Geist, sans-serif"
        );
    }
}
