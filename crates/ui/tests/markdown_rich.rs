use jolt_ui::markdown::{Block, MathKind, parse_full, rich};
use ratex_types::Color;
use std::sync::Arc;

#[test]
fn parses_all_supported_math_delimiters() {
    let tree = parse_full("Euler: $e^{i\\pi}+1=0$ and \\(a*b*c\\).\n\n$$\\sum_i i$$\n\n\\[a=b\\]");
    let math: Vec<_> = tree
        .blocks
        .iter()
        .filter_map(|top| match &top.block {
            Block::Paragraph { runs } => Some(runs),
            _ => None,
        })
        .flatten()
        .filter_map(|run| run.style.math.map(|kind| (kind, run.text.as_str())))
        .collect();
    assert_eq!(
        math,
        vec![
            (MathKind::Inline, "e^{i\\pi}+1=0"),
            (MathKind::Inline, "a*b*c"),
            (MathKind::Display, "\\sum_i i"),
            (MathKind::Display, "a=b"),
        ]
    );
}

#[test]
fn unclosed_and_code_math_stays_literal() {
    let tree = parse_full("open \\(x and mismatched \\(y\\]\n\n```txt\n\\[raw\\]\n```");
    let Block::Paragraph { runs } = &tree.blocks[0].block else {
        panic!("expected paragraph");
    };
    assert!(runs.iter().all(|run| run.style.math.is_none()));
    assert_eq!(
        runs.iter().map(|run| run.text.as_str()).collect::<String>(),
        "open \\(x and mismatched \\(y\\]"
    );
    let Block::CodeBlock { code, .. } = &tree.blocks[1].block else {
        panic!("expected code block");
    };
    assert_eq!(code, "\\[raw\\]");
}

#[test]
fn markdown_wrapper_keeps_nested_rich_fences_literal() {
    let source = "```markdown\nInline $e^{i\\pi}+1=0$\n\n```math\n\\frac{1}{2}\n```\n\n```mermaid\nflowchart LR\n A --> B\n```\n```";
    let tree = parse_full(source);
    assert_eq!(tree.blocks.len(), 1);
    let Block::CodeBlock { language, code } = &tree.blocks[0].block else {
        panic!("expected one literal Markdown code block");
    };
    assert_eq!(language.as_deref(), Some("markdown"));
    assert!(code.contains("Inline $e^{i\\pi}+1=0$"));
    assert!(code.contains("```math\n\\frac{1}{2}\n```"));
    assert!(code.contains("```mermaid\nflowchart LR\n A --> B\n```"));
}

#[test]
fn native_engines_render_sized_svg() {
    let formula = rich::render_math(
        r"\frac{-b \pm \sqrt{b^2-4ac}}{2a}",
        false,
        Color::WHITE,
        16.0,
    )
    .unwrap();
    assert!(formula.svg.starts_with("<svg"));
    assert!(formula.width > 20.0 && formula.height > 16.0);

    let colors = rich::MermaidColors {
        canvas: "#080808",
        surface: "#141414",
        surface_alt: "#202020",
        text: "#eeeeee",
        muted: "#aaaaaa",
        border: "#555555",
        line: "#999999",
        accent: "#818cf8",
    };
    let diagram = rich::render_mermaid(
        "flowchart LR\n A[Start] --> B{Ready?}\n B -->|Yes| C[Done]",
        &colors,
        "Geist",
        "integration-test",
    )
    .unwrap();
    assert!(diagram.svg.starts_with("<svg"));
    assert!(!diagram.svg.contains("<foreignObject"));
    assert!(diagram.width > 0.0 && diagram.height > 0.0);

    // Exercise the same native SVG rasterizer used by the transcript. This
    // catches unsupported SVG features before a rich block reaches the UI.
    let rasterizer = gpui::SvgRenderer::new(Arc::new(jolt_ui::icons::Assets));
    let formula_image = rasterizer
        .render_single_frame(formula.svg.as_bytes(), 1.0)
        .unwrap();
    let diagram_image = rasterizer
        .render_single_frame(diagram.svg.as_bytes(), 1.0)
        .unwrap();
    assert!(!formula_image.as_bytes(0).unwrap().is_empty());
    assert!(!diagram_image.as_bytes(0).unwrap().is_empty());
}
