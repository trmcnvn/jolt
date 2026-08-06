//! Paired built-in themes and installation-level custom theme files.
//!
//! Custom themes live in `{data_dir}/themes`, independent of Local/Account
//! scope and sign-in state. Each file contains complete light and dark palette
//! snapshots so a Jolt update cannot silently retune a user's theme.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use gpui::Hsla;
use serde::{Deserialize, Serialize};

use crate::theme::{Appearance, Theme, hsl_to_rgb, rgb8};

pub const JOLT_THEME_ID: &str = "jolt";
pub const CATPPUCCIN_THEME_ID: &str = "catppuccin";
pub const ROSE_PINE_THEME_ID: &str = "rose-pine";
const THEME_SCHEMA_VERSION: u32 = 1;
const THEMES_DIR: &str = "themes";
const MAX_THEME_FILES: usize = 256;
const MAX_THEME_FILE_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeColorGroup {
    Surfaces,
    Text,
    Accents,
    Code,
    Terminal,
}

impl ThemeColorGroup {
    pub const ALL: [Self; 5] = [
        Self::Surfaces,
        Self::Text,
        Self::Accents,
        Self::Code,
        Self::Terminal,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Surfaces => "Surfaces and controls",
            Self::Text => "Text",
            Self::Accents => "Accent and status",
            Self::Code => "Code and diff",
            Self::Terminal => "Terminal",
        }
    }
}

macro_rules! theme_color_roles {
    ($( $variant:ident, $key:literal, $label:literal, $group:ident, $field:ident; )+) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum ThemeColorRole {
            $( $variant, )+
        }

        impl ThemeColorRole {
            pub const ALL: &'static [Self] = &[$( Self::$variant, )+];

            pub fn key(self) -> &'static str {
                match self { $( Self::$variant => $key, )+ }
            }

            pub fn label(self) -> &'static str {
                match self { $( Self::$variant => $label, )+ }
            }

            pub fn group(self) -> ThemeColorGroup {
                match self { $( Self::$variant => ThemeColorGroup::$group, )+ }
            }

            pub fn color(self, theme: &Theme) -> Hsla {
                match self { $( Self::$variant => theme.$field, )+ }
            }

            pub fn set_color(self, theme: &mut Theme, color: Hsla) {
                match self { $( Self::$variant => theme.$field = color, )+ }
            }
        }
    };
}

theme_color_roles! {
    Background, "background", "Background", Surfaces, bg;
    Surface, "surface", "Sidebar", Surfaces, surface;
    GlassTint, "glassTint", "Chrome tint", Surfaces, glass_tint;
    SurfaceRaised, "surfaceRaised", "Raised surface", Surfaces, surface_raised;
    SurfaceCard, "surfaceCard", "Card", Surfaces, surface_card;
    SurfaceDialog, "surfaceDialog", "Dialog", Surfaces, surface_dialog;
    SurfaceOverlay, "surfaceOverlay", "Popover", Surfaces, surface_overlay;
    ElementHover, "elementHover", "Hover", Surfaces, element_hover;
    ElementActive, "elementActive", "Active", Surfaces, element_active;
    Border, "border", "Border", Surfaces, border;
    BorderStrong, "borderStrong", "Strong border", Surfaces, border_strong;
    SurfaceRaisedHover, "surfaceRaisedHover", "Raised hover", Surfaces, surface_raised_hover;
    Band, "band", "Recessed band", Surfaces, band;
    InputBackground, "inputBackground", "Input background", Surfaces, input_bg;
    Selection, "selection", "Text selection", Surfaces, selection;
    Cursor, "cursor", "Cursor", Surfaces, cursor;
    Caret, "caret", "Caret", Surfaces, caret;

    Text, "text", "Primary text", Text, text;
    TextMuted, "textMuted", "Muted text", Text, text_muted;
    TextFaint, "textFaint", "Faint text", Text, text_faint;
    TextDim, "textDim", "Dim text", Text, text_dim;
    Solid, "solid", "Solid fill", Text, solid;
    OnSolid, "onSolid", "Text on solid", Text, on_solid;

    Accent, "accent", "Accent", Accents, accent;
    AccentStrong, "accentStrong", "Strong accent", Accents, accent_strong;
    OnAccent, "onAccent", "Text on accent", Accents, on_accent;
    Danger, "danger", "Danger", Accents, danger;
    DangerMuted, "dangerMuted", "Muted danger", Accents, danger_muted;
    DangerStrong, "dangerStrong", "Strong danger", Accents, danger_strong;
    Warning, "warning", "Warning", Accents, warning;
    WarningMuted, "warningMuted", "Muted warning", Accents, warning_muted;
    Success, "success", "Success", Accents, success;
    SuccessMuted, "successMuted", "Muted success", Accents, success_muted;
    Busy, "busy", "Working", Accents, busy;

    CodeText, "codeText", "Inline code", Code, code_text;
    CodeWash, "codeWash", "Inline code wash", Code, code_wash;
    SyntaxKeyword, "syntaxKeyword", "Keyword", Code, syntax_keyword;
    SyntaxString, "syntaxString", "String", Code, syntax_string;
    SyntaxNumber, "syntaxNumber", "Number", Code, syntax_number;
    DiffAdd, "diffAdd", "Diff addition", Code, diff_add;
    DiffDelete, "diffDelete", "Diff deletion", Code, diff_del;
    DiffHunk, "diffHunk", "Diff hunk", Code, diff_hunk_bg;

    TerminalBackground, "terminalBackground", "Background", Terminal, terminal_bg;
    TerminalForeground, "terminalForeground", "Foreground", Terminal, terminal_fg;
    TerminalSelection, "terminalSelection", "Selection", Terminal, terminal_selection;
}

#[derive(Debug, Clone)]
pub struct ThemeSummary {
    pub id: String,
    pub name: String,
    pub builtin: bool,
    pub light: Theme,
    pub dark: Theme,
}

#[derive(Debug, Clone)]
pub struct EditableTheme {
    pub id: Option<String>,
    pub name: String,
    pub source_theme_id: String,
    pub revision: u64,
    pub light: Theme,
    pub dark: Theme,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThemeFile {
    schema_version: u32,
    id: String,
    name: String,
    revision: u64,
    source_theme_id: String,
    light_palette: BTreeMap<String, String>,
    dark_palette: BTreeMap<String, String>,
}

impl ThemeFile {
    fn validate(&self) -> bool {
        self.schema_version >= 1
            && uuid::Uuid::parse_str(&self.id).is_ok()
            && !self.name.trim().is_empty()
            && palette_is_complete(&self.light_palette)
            && palette_is_complete(&self.dark_palette)
    }

    fn theme(&self, appearance: Appearance) -> Theme {
        let mut theme = builtin_theme(&self.source_theme_id, appearance)
            .unwrap_or_else(|| Theme::for_appearance(appearance));
        let colors = match appearance {
            Appearance::Light => &self.light_palette,
            Appearance::Dark => &self.dark_palette,
        };
        apply_palette(&mut theme, colors);
        theme.palette_id = self.id.clone().into();
        theme.palette_revision = self.revision;
        theme
    }
}

#[derive(Debug, Clone, Default)]
pub struct ThemeCatalog {
    custom: BTreeMap<String, ThemeFile>,
}

impl ThemeCatalog {
    pub fn load(data_dir: &Path) -> Self {
        let mut catalog = Self::default();
        let dir = themes_dir(data_dir);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return catalog;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            match std::fs::read_to_string(&path) {
                Ok(text) => match serde_json::from_str::<ThemeFile>(&text) {
                    Ok(file) if file.validate() => {
                        catalog.custom.insert(file.id.clone(), file);
                    }
                    Ok(_) => {
                        tracing::warn!(path = %path.display(), "invalid custom theme; ignoring");
                    }
                    Err(err) => {
                        tracing::warn!(path = %path.display(), error = %err, "could not parse custom theme");
                    }
                },
                Err(err) => {
                    tracing::warn!(path = %path.display(), error = %err, "could not read custom theme");
                }
            }
        }
        catalog
    }

    pub fn summaries(&self) -> Vec<ThemeSummary> {
        let mut themes = vec![
            builtin_summary(JOLT_THEME_ID, "Jolt"),
            builtin_summary(CATPPUCCIN_THEME_ID, "Catppuccin"),
            builtin_summary(ROSE_PINE_THEME_ID, "Rosé Pine"),
        ];
        let mut custom: Vec<_> = self
            .custom
            .values()
            .map(|file| ThemeSummary {
                id: file.id.clone(),
                name: file.name.clone(),
                builtin: false,
                light: file.theme(Appearance::Light),
                dark: file.theme(Appearance::Dark),
            })
            .collect();
        custom.sort_by_key(|theme| theme.name.to_lowercase());
        themes.extend(custom);
        themes
    }

    pub fn resolve(&self, id: &str, appearance: Appearance) -> Theme {
        self.custom
            .get(id)
            .map(|file| file.theme(appearance))
            .or_else(|| builtin_theme(id, appearance))
            .unwrap_or_else(|| Theme::for_appearance(appearance))
    }

    pub fn editable(&self, id: &str) -> EditableTheme {
        if let Some(file) = self.custom.get(id) {
            return EditableTheme {
                id: Some(file.id.clone()),
                name: file.name.clone(),
                source_theme_id: file.source_theme_id.clone(),
                revision: file.revision,
                light: file.theme(Appearance::Light),
                dark: file.theme(Appearance::Dark),
            };
        }
        let name = builtin_name(id).unwrap_or("Jolt");
        EditableTheme {
            id: None,
            name: format!("Custom {name}"),
            source_theme_id: if builtin_name(id).is_some() {
                id.to_string()
            } else {
                JOLT_THEME_ID.to_string()
            },
            revision: 0,
            light: builtin_theme(id, Appearance::Light).unwrap_or_else(Theme::light),
            dark: builtin_theme(id, Appearance::Dark).unwrap_or_else(Theme::dark),
        }
    }

    pub fn save(&mut self, draft: &EditableTheme, data_dir: &Path) -> io::Result<String> {
        let id = draft
            .id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let file = ThemeFile {
            schema_version: THEME_SCHEMA_VERSION,
            id: id.clone(),
            name: draft.name.trim().to_string(),
            revision: draft.revision.saturating_add(1),
            source_theme_id: draft.source_theme_id.clone(),
            light_palette: snapshot_palette(&draft.light),
            dark_palette: snapshot_palette(&draft.dark),
        };
        if !file.validate() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "theme name and palettes must be valid",
            ));
        }
        let dir = themes_dir(data_dir);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{id}.json"));
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(&file)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)?;
        self.custom.insert(id.clone(), file);
        Ok(id)
    }

    pub fn delete(&mut self, id: &str, data_dir: &Path) -> io::Result<bool> {
        if self.custom.remove(id).is_none() {
            return Ok(false);
        }
        match std::fs::remove_file(themes_dir(data_dir).join(format!("{id}.json"))) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(true),
            Err(err) => Err(err),
        }
    }
}

pub fn local_theme_records(data_dir: &Path) -> io::Result<Vec<jolt_proto::ThemeFileRecord>> {
    let dir = themes_dir(data_dir);
    let mut records = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(records);
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let contents = std::fs::read_to_string(&path)?;
        let file: ThemeFile = serde_json::from_str(&contents)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        let path_id = path.file_stem().and_then(|stem| stem.to_str());
        if !file.validate() || contents.len() > MAX_THEME_FILE_BYTES || path_id != Some(&file.id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid custom theme {}", path.display()),
            ));
        }
        records.push(jolt_proto::ThemeFileRecord {
            id: file.id,
            revision: file.revision,
            deleted: false,
            contents,
        });
        if records.len() > MAX_THEME_FILES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "too many custom themes",
            ));
        }
    }
    records.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(records)
}

#[derive(Debug, Default)]
pub(crate) struct ThemeSyncPlan {
    pub upserts: Vec<jolt_proto::ThemeFileRecord>,
    pub deletes: Vec<String>,
}

impl ThemeSyncPlan {
    pub fn project_upserts_onto(
        &self,
        remote: &[jolt_proto::ThemeFileRecord],
    ) -> Vec<jolt_proto::ThemeFileRecord> {
        let mut projected: BTreeMap<_, _> = remote
            .iter()
            .cloned()
            .map(|record| (record.id.clone(), record))
            .collect();
        for record in &self.upserts {
            if projected
                .get(&record.id)
                .is_none_or(|current| record.revision > current.revision)
            {
                projected.insert(record.id.clone(), record.clone());
            }
        }
        projected.into_values().collect()
    }

    /// Predict the registry state after successful mutation RPCs. Keeping this
    /// as the next baseline prevents retries from creating extra conflict
    /// copies if the follow-up list call alone is interrupted.
    pub fn project_onto(
        &self,
        remote: &[jolt_proto::ThemeFileRecord],
    ) -> Vec<jolt_proto::ThemeFileRecord> {
        let mut projected: BTreeMap<_, _> = self
            .project_upserts_onto(remote)
            .into_iter()
            .map(|record| (record.id.clone(), record))
            .collect();
        for id in &self.deletes {
            let revision = projected
                .get(id)
                .map_or(1, |record| record.revision.saturating_add(1));
            projected.insert(
                id.clone(),
                jolt_proto::ThemeFileRecord {
                    id: id.clone(),
                    revision,
                    deleted: true,
                    contents: String::new(),
                },
            );
        }
        projected.into_values().collect()
    }
}

/// Compare disk and registry state against the last successfully installed
/// registry frame. If both sides changed one theme, preserve the registry row
/// under its original ID and upload the local version as an explicit copy.
pub(crate) fn plan_theme_file_sync(
    local: &[jolt_proto::ThemeFileRecord],
    remote: &[jolt_proto::ThemeFileRecord],
    known: &[jolt_proto::ThemeFileRecord],
) -> io::Result<ThemeSyncPlan> {
    let local: BTreeMap<_, _> = local.iter().map(|row| (row.id.as_str(), row)).collect();
    let remote: BTreeMap<_, _> = remote.iter().map(|row| (row.id.as_str(), row)).collect();
    let known: BTreeMap<_, _> = known.iter().map(|row| (row.id.as_str(), row)).collect();
    let ids: std::collections::BTreeSet<_> = local
        .keys()
        .chain(remote.keys())
        .chain(known.keys())
        .copied()
        .collect();
    let mut plan = ThemeSyncPlan::default();

    for id in ids {
        let local_row = local.get(id).copied();
        let remote_row = remote.get(id).copied();
        let known_row = known.get(id).copied();
        let local_changed = !local_matches_registry(local_row, known_row);
        let remote_changed = remote_row != known_row;

        match (local_changed, remote_changed) {
            (false, _) => {}
            (true, false) => match local_row {
                Some(row) => plan.upserts.push(row.clone()),
                None => plan.deletes.push(id.to_string()),
            },
            (true, true) if local_matches_registry(local_row, remote_row) => {}
            (true, true) => match local_row {
                Some(row) => plan.upserts.push(conflict_copy(row)?),
                None => {
                    if let Some(row) = remote_row.filter(|row| !row.deleted) {
                        plan.upserts.push(conflict_copy(row)?);
                        plan.deletes.push(id.to_string());
                    }
                }
            },
        }
    }
    Ok(plan)
}

fn local_matches_registry(
    local: Option<&jolt_proto::ThemeFileRecord>,
    registry: Option<&jolt_proto::ThemeFileRecord>,
) -> bool {
    match (local, registry) {
        (Some(local), Some(registry)) => !registry.deleted && local == registry,
        (None, None) | (None, Some(jolt_proto::ThemeFileRecord { deleted: true, .. })) => true,
        _ => false,
    }
}

fn conflict_copy(record: &jolt_proto::ThemeFileRecord) -> io::Result<jolt_proto::ThemeFileRecord> {
    let mut file: ThemeFile = serde_json::from_str(&record.contents)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    if record.deleted
        || !file.validate()
        || file.id != record.id
        || file.revision != record.revision
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid conflicting custom theme",
        ));
    }
    file.id = uuid::Uuid::new_v4().to_string();
    file.name = format!("{} (conflict)", file.name);
    file.revision = 1;
    let contents = serde_json::to_string_pretty(&file)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    Ok(jolt_proto::ThemeFileRecord {
        id: file.id,
        revision: file.revision,
        deleted: false,
        contents,
    })
}

/// Replace the installation-level custom-theme directory with one authoritative
/// account-registry frame. Every payload is validated before disk is touched.
pub fn install_synced_theme_files(
    records: &[jolt_proto::ThemeFileRecord],
    data_dir: &Path,
) -> io::Result<()> {
    if records.iter().filter(|record| !record.deleted).count() > MAX_THEME_FILES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "too many synced custom themes",
        ));
    }
    let mut validated = Vec::with_capacity(records.len());
    for record in records {
        if uuid::Uuid::parse_str(&record.id).is_err()
            || record.contents.len() > MAX_THEME_FILE_BYTES
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "synced theme id or size is invalid",
            ));
        }
        if record.deleted {
            if !record.contents.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "deleted theme carries file contents",
                ));
            }
            continue;
        }
        let file: ThemeFile = serde_json::from_str(&record.contents)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        if !file.validate() || file.id != record.id || file.revision != record.revision {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "synced theme id, revision, or palette is invalid",
            ));
        }
        validated.push((record.id.clone(), record.contents.as_str()));
    }

    let dir = themes_dir(data_dir);
    std::fs::create_dir_all(&dir)?;
    let wanted: std::collections::HashSet<&str> =
        validated.iter().map(|(id, _)| id.as_str()).collect();
    for entry in std::fs::read_dir(&dir)? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
            let id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or_default();
            if !wanted.contains(id) {
                std::fs::remove_file(path)?;
            }
        }
    }
    for (id, contents) in validated {
        let path = dir.join(format!("{id}.json"));
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, contents)?;
        std::fs::rename(tmp, path)?;
    }
    Ok(())
}

fn themes_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(THEMES_DIR)
}

fn builtin_name(id: &str) -> Option<&'static str> {
    match id {
        JOLT_THEME_ID => Some("Jolt"),
        CATPPUCCIN_THEME_ID => Some("Catppuccin"),
        ROSE_PINE_THEME_ID => Some("Rosé Pine"),
        _ => None,
    }
}

fn builtin_summary(id: &str, name: &str) -> ThemeSummary {
    ThemeSummary {
        id: id.to_string(),
        name: name.to_string(),
        builtin: true,
        light: builtin_theme(id, Appearance::Light).expect("known built-in has a light palette"),
        dark: builtin_theme(id, Appearance::Dark).expect("known built-in has a dark palette"),
    }
}

fn builtin_theme(id: &str, appearance: Appearance) -> Option<Theme> {
    let mut theme = match (id, appearance) {
        (JOLT_THEME_ID, Appearance::Light) => Theme::light(),
        (JOLT_THEME_ID, Appearance::Dark) => Theme::dark(),
        (CATPPUCCIN_THEME_ID, Appearance::Light) => catppuccin_latte(),
        (CATPPUCCIN_THEME_ID, Appearance::Dark) => catppuccin_mocha(),
        (ROSE_PINE_THEME_ID, Appearance::Light) => rose_pine_dawn(),
        (ROSE_PINE_THEME_ID, Appearance::Dark) => rose_pine(),
        _ => return None,
    };
    theme.palette_id = id.to_string().into();
    Some(theme)
}

fn c(value: &str) -> Hsla {
    parse_hex_color(value).expect("built-in theme colors are valid hex")
}

fn catppuccin_latte() -> Theme {
    let mut t = Theme::light();
    t.bg = c("#eff1f5");
    t.surface = c("#e6e9ef");
    t.glass_tint = c("#e6e9ef");
    t.surface_raised = c("#ccd0da");
    t.surface_card = c("#e6e9ef");
    t.surface_dialog = c("#e6e9ef");
    t.surface_overlay = c("#e6e9ef");
    t.element_hover = c("#ccd0da80");
    t.element_active = c("#ccd0da");
    t.border = c("#8c8fa126");
    t.border_strong = c("#acb0be");
    t.text = c("#4c4f69");
    t.text_muted = c("#5c5f77");
    t.text_faint = c("#7c7f93");
    t.text_dim = c("#6c6f85");
    t.solid = c("#4c4f69");
    t.on_solid = c("#dce0e8");
    t.accent = c("#8839ef");
    t.accent_strong = c("#8839ef");
    t.on_accent = c("#dce0e8");
    t.danger = c("#d20f39");
    t.danger_muted = c("#e64553");
    t.danger_strong = c("#d20f39");
    t.warning = c("#df8e1d");
    t.warning_muted = c("#fe640b");
    t.success = c("#40a02b");
    t.success_muted = c("#179299");
    t.busy = c("#ea76cb");
    t.surface_raised_hover = c("#bcc0cc");
    t.band = c("#dce0e8");
    t.input_bg = c("#ccd0da");
    t.selection = c("#7c7f934d");
    t.cursor = c("#dc8a78");
    t.caret = c("#1e66f5");
    t.code_text = c("#8839ef");
    t.code_wash = c("#8839ef1a");
    t.syntax_keyword = c("#8839ef");
    t.syntax_string = c("#40a02b");
    t.syntax_number = c("#fe640b");
    t.diff_add = c("#40a02b");
    t.diff_del = c("#d20f39");
    t.diff_hunk_bg = c("#1e66f526");
    t.terminal_bg = c("#eff1f5");
    t.terminal_fg = c("#4c4f69");
    t.terminal_selection = c("#acb0be");
    set_ansi(
        &mut t,
        [
            "#5c5f77", "#d20f39", "#40a02b", "#df8e1d", "#1e66f5", "#ea76cb", "#179299", "#acb0be",
            "#6c6f85", "#de293e", "#49af3d", "#eea02d", "#456eff", "#fe85d8", "#2d9fa8", "#bcc0cc",
        ],
    );
    t
}

fn catppuccin_mocha() -> Theme {
    let mut t = Theme::dark();
    t.bg = c("#1e1e2e");
    t.surface = c("#181825");
    t.glass_tint = c("#181825");
    t.surface_raised = c("#313244");
    t.surface_card = c("#181825");
    t.surface_dialog = c("#181825");
    t.surface_overlay = c("#181825");
    t.element_hover = c("#31324480");
    t.element_active = c("#313244");
    t.border = c("#7f849c26");
    t.border_strong = c("#585b70");
    t.text = c("#cdd6f4");
    t.text_muted = c("#bac2de");
    t.text_faint = c("#9399b2");
    t.text_dim = c("#a6adc8");
    t.solid = c("#cdd6f4");
    t.on_solid = c("#11111b");
    t.accent = c("#cba6f7");
    t.accent_strong = c("#cba6f7");
    t.on_accent = c("#11111b");
    t.danger = c("#f38ba8");
    t.danger_muted = c("#eba0ac");
    t.danger_strong = c("#f38ba8");
    t.warning = c("#f9e2af");
    t.warning_muted = c("#fab387");
    t.success = c("#a6e3a1");
    t.success_muted = c("#94e2d5");
    t.busy = c("#f5c2e7");
    t.surface_raised_hover = c("#45475a");
    t.band = c("#11111b");
    t.input_bg = c("#313244");
    t.selection = c("#9399b240");
    t.cursor = c("#f5e0dc");
    t.caret = c("#89b4fa");
    t.code_text = c("#cba6f7");
    t.code_wash = c("#cba6f71f");
    t.syntax_keyword = c("#cba6f7");
    t.syntax_string = c("#a6e3a1");
    t.syntax_number = c("#fab387");
    t.diff_add = c("#a6e3a1");
    t.diff_del = c("#f38ba8");
    t.diff_hunk_bg = c("#89b4fa33");
    t.terminal_bg = c("#1e1e2e");
    t.terminal_fg = c("#cdd6f4");
    t.terminal_selection = c("#585b70");
    set_ansi(
        &mut t,
        [
            "#45475a", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#a6adc8",
            "#585b70", "#f37799", "#89d88b", "#ebd391", "#74a8fc", "#f2aede", "#6bd7ca", "#bac2de",
        ],
    );
    t
}

fn rose_pine_dawn() -> Theme {
    let mut t = Theme::light();
    t.bg = c("#faf4ed");
    t.surface = c("#faf4ed");
    t.glass_tint = c("#faf4ed");
    t.surface_raised = c("#fffaf3");
    t.surface_card = c("#fffaf3");
    t.surface_dialog = c("#fffaf3");
    t.surface_overlay = c("#fffaf3");
    t.element_hover = c("#6e6a860d");
    t.element_active = c("#6e6a8614");
    t.border = c("#6e6a8614");
    t.border_strong = c("#6e6a8626");
    t.text = c("#575279");
    t.text_muted = c("#797593");
    t.text_faint = c("#9893a5");
    t.text_dim = c("#9893a5");
    t.solid = c("#575279");
    t.on_solid = c("#faf4ed");
    t.accent = c("#907aa9");
    t.accent_strong = c("#907aa9");
    t.on_accent = c("#faf4ed");
    t.danger = c("#b4637a");
    t.danger_muted = c("#d7827e");
    t.danger_strong = c("#b4637a");
    t.warning = c("#ea9d34");
    t.warning_muted = c("#d7827e");
    t.success = c("#56949f");
    t.success_muted = c("#286983");
    t.busy = c("#d7827e");
    t.surface_raised_hover = c("#f2e9e1");
    t.band = c("#6e6a860d");
    t.input_bg = c("#f9f2ea");
    t.selection = c("#6e6a8614");
    t.cursor = c("#575279");
    t.caret = c("#56949f");
    t.code_text = c("#907aa9");
    t.code_wash = c("#907aa91f");
    t.syntax_keyword = c("#286983");
    t.syntax_string = c("#ea9d34");
    t.syntax_number = c("#d7827e");
    t.diff_add = c("#56949f");
    t.diff_del = c("#b4637a");
    t.diff_hunk_bg = c("#907aa91f");
    t.terminal_bg = c("#faf4ed");
    t.terminal_fg = c("#575279");
    t.terminal_selection = c("#6e6a8614");
    set_ansi(
        &mut t,
        [
            "#f2e9e1", "#b4637a", "#286983", "#ea9d34", "#56949f", "#907aa9", "#d7827e", "#575279",
            "#9893a5", "#b4637a", "#286983", "#ea9d34", "#56949f", "#907aa9", "#d7827e", "#575279",
        ],
    );
    t
}

fn rose_pine() -> Theme {
    let mut t = Theme::dark();
    t.bg = c("#191724");
    t.surface = c("#191724");
    t.glass_tint = c("#191724");
    t.surface_raised = c("#1f1d2e");
    t.surface_card = c("#1f1d2e");
    t.surface_dialog = c("#1f1d2e");
    t.surface_overlay = c("#1f1d2e");
    t.element_hover = c("#6e6a861a");
    t.element_active = c("#6e6a8633");
    t.border = c("#6e6a8633");
    t.border_strong = c("#6e6a8666");
    t.text = c("#e0def4");
    t.text_muted = c("#908caa");
    t.text_faint = c("#6e6a86");
    t.text_dim = c("#6e6a86");
    t.solid = c("#e0def4");
    t.on_solid = c("#191724");
    t.accent = c("#c4a7e7");
    t.accent_strong = c("#c4a7e7");
    t.on_accent = c("#191724");
    t.danger = c("#eb6f92");
    t.danger_muted = c("#ebbcba");
    t.danger_strong = c("#eb6f92");
    t.warning = c("#f6c177");
    t.warning_muted = c("#ebbcba");
    t.success = c("#9ccfd8");
    t.success_muted = c("#31748f");
    t.busy = c("#ebbcba");
    t.surface_raised_hover = c("#26233a");
    t.band = c("#6e6a861a");
    t.input_bg = c("#232034");
    t.selection = c("#6e6a8633");
    t.cursor = c("#e0def4");
    t.caret = c("#9ccfd8");
    t.code_text = c("#c4a7e7");
    t.code_wash = c("#c4a7e71f");
    t.syntax_keyword = c("#31748f");
    t.syntax_string = c("#f6c177");
    t.syntax_number = c("#ebbcba");
    t.diff_add = c("#9ccfd8");
    t.diff_del = c("#eb6f92");
    t.diff_hunk_bg = c("#c4a7e71f");
    t.terminal_bg = c("#191724");
    t.terminal_fg = c("#e0def4");
    t.terminal_selection = c("#6e6a8633");
    set_ansi(
        &mut t,
        [
            "#26233a", "#eb6f92", "#31748f", "#f6c177", "#9ccfd8", "#c4a7e7", "#ebbcba", "#e0def4",
            "#6e6a86", "#eb6f92", "#31748f", "#f6c177", "#9ccfd8", "#c4a7e7", "#ebbcba", "#e0def4",
        ],
    );
    t
}

fn set_ansi(theme: &mut Theme, colors: [&str; 16]) {
    theme.terminal_ansi = colors.map(c);
}

fn snapshot_palette(theme: &Theme) -> BTreeMap<String, String> {
    let mut colors = BTreeMap::new();
    for role in ThemeColorRole::ALL {
        colors.insert(role.key().to_string(), format_hex_color(role.color(theme)));
    }
    for (index, color) in theme.terminal_ansi.iter().enumerate() {
        colors.insert(format!("terminalAnsi{index}"), format_hex_color(*color));
    }
    colors
}

fn palette_is_complete(colors: &BTreeMap<String, String>) -> bool {
    ThemeColorRole::ALL.iter().all(|role| {
        colors
            .get(role.key())
            .is_some_and(|value| parse_hex_color(value).is_ok())
    }) && (0..16).all(|index| {
        colors
            .get(&format!("terminalAnsi{index}"))
            .is_some_and(|value| parse_hex_color(value).is_ok())
    })
}

fn apply_palette(theme: &mut Theme, colors: &BTreeMap<String, String>) {
    for role in ThemeColorRole::ALL {
        if let Some(color) = colors
            .get(role.key())
            .and_then(|value| parse_hex_color(value).ok())
        {
            role.set_color(theme, color);
        }
    }
    for (index, target) in theme.terminal_ansi.iter_mut().enumerate() {
        if let Some(color) = colors
            .get(&format!("terminalAnsi{index}"))
            .and_then(|value| parse_hex_color(value).ok())
        {
            *target = color;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseHexColorError {
    Shape,
    Digit,
}

impl std::fmt::Display for ParseHexColorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Shape => formatter.write_str("expected #RRGGBB or #RRGGBBAA"),
            Self::Digit => formatter.write_str("invalid hexadecimal color"),
        }
    }
}

impl std::error::Error for ParseHexColorError {}

pub fn parse_hex_color(value: &str) -> Result<Hsla, ParseHexColorError> {
    let digits = value
        .trim()
        .strip_prefix('#')
        .ok_or(ParseHexColorError::Shape)?;
    if digits.len() != 6 && digits.len() != 8 {
        return Err(ParseHexColorError::Shape);
    }
    let byte = |start| {
        u8::from_str_radix(&digits[start..start + 2], 16).map_err(|_| ParseHexColorError::Digit)
    };
    let r = byte(0)?;
    let g = byte(2)?;
    let b = byte(4)?;
    let a = if digits.len() == 8 { byte(6)? } else { 255 };
    let mut color = rgb8(r, g, b);
    color.a = f32::from(a) / 255.0;
    Ok(color)
}

pub fn format_hex_color(color: Hsla) -> String {
    let [r, g, b] = hsl_to_rgb(color.h, color.s, color.l);
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    let (r, g, b, a) = (channel(r), channel(g), channel(b), channel(color.a));
    if a == 255 {
        format!("#{r:02x}{g:02x}{b:02x}")
    } else {
        format!("#{r:02x}{g:02x}{b:02x}{a:02x}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_use_canonical_palette_roles() {
        let themes = ThemeCatalog::default().summaries();
        assert_eq!(themes.len(), 3);

        let latte = &themes[1].light;
        assert_colors(
            [
                latte.bg,
                latte.surface,
                latte.input_bg,
                latte.text,
                latte.text_muted,
                latte.text_dim,
                latte.text_faint,
                latte.accent,
                latte.selection,
                latte.cursor,
                latte.syntax_keyword,
            ],
            [
                "#eff1f5",
                "#e6e9ef",
                "#ccd0da",
                "#4c4f69",
                "#5c5f77",
                "#6c6f85",
                "#7c7f93",
                "#8839ef",
                "#7c7f934d",
                "#dc8a78",
                "#8839ef",
            ],
        );
        assert_colors(
            latte.terminal_ansi,
            [
                "#5c5f77", "#d20f39", "#40a02b", "#df8e1d", "#1e66f5", "#ea76cb", "#179299",
                "#acb0be", "#6c6f85", "#de293e", "#49af3d", "#eea02d", "#456eff", "#fe85d8",
                "#2d9fa8", "#bcc0cc",
            ],
        );

        let mocha = &themes[1].dark;
        assert_colors(
            [
                mocha.bg,
                mocha.surface,
                mocha.input_bg,
                mocha.text,
                mocha.text_muted,
                mocha.text_dim,
                mocha.text_faint,
                mocha.accent,
                mocha.selection,
                mocha.cursor,
                mocha.syntax_keyword,
            ],
            [
                "#1e1e2e",
                "#181825",
                "#313244",
                "#cdd6f4",
                "#bac2de",
                "#a6adc8",
                "#9399b2",
                "#cba6f7",
                "#9399b240",
                "#f5e0dc",
                "#cba6f7",
            ],
        );
        assert_colors(
            mocha.terminal_ansi,
            [
                "#45475a", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5",
                "#a6adc8", "#585b70", "#f37799", "#89d88b", "#ebd391", "#74a8fc", "#f2aede",
                "#6bd7ca", "#bac2de",
            ],
        );

        let dawn = &themes[2].light;
        assert_colors(
            [
                dawn.bg,
                dawn.surface_card,
                dawn.surface_raised_hover,
                dawn.text,
                dawn.text_muted,
                dawn.text_faint,
                dawn.accent,
                dawn.element_hover,
                dawn.element_active,
                dawn.border_strong,
                dawn.syntax_keyword,
                dawn.syntax_string,
                dawn.syntax_number,
            ],
            [
                "#faf4ed",
                "#fffaf3",
                "#f2e9e1",
                "#575279",
                "#797593",
                "#9893a5",
                "#907aa9",
                "#6e6a860d",
                "#6e6a8614",
                "#6e6a8626",
                "#286983",
                "#ea9d34",
                "#d7827e",
            ],
        );
        assert_colors(
            dawn.terminal_ansi,
            [
                "#f2e9e1", "#b4637a", "#286983", "#ea9d34", "#56949f", "#907aa9", "#d7827e",
                "#575279", "#9893a5", "#b4637a", "#286983", "#ea9d34", "#56949f", "#907aa9",
                "#d7827e", "#575279",
            ],
        );

        let rose_pine = &themes[2].dark;
        assert_colors(
            [
                rose_pine.bg,
                rose_pine.surface_card,
                rose_pine.surface_raised_hover,
                rose_pine.text,
                rose_pine.text_muted,
                rose_pine.text_faint,
                rose_pine.accent,
                rose_pine.element_hover,
                rose_pine.element_active,
                rose_pine.border_strong,
                rose_pine.syntax_keyword,
                rose_pine.syntax_string,
                rose_pine.syntax_number,
            ],
            [
                "#191724",
                "#1f1d2e",
                "#26233a",
                "#e0def4",
                "#908caa",
                "#6e6a86",
                "#c4a7e7",
                "#6e6a861a",
                "#6e6a8633",
                "#6e6a8666",
                "#31748f",
                "#f6c177",
                "#ebbcba",
            ],
        );
        assert_colors(
            rose_pine.terminal_ansi,
            [
                "#26233a", "#eb6f92", "#31748f", "#f6c177", "#9ccfd8", "#c4a7e7", "#ebbcba",
                "#e0def4", "#6e6a86", "#eb6f92", "#31748f", "#f6c177", "#9ccfd8", "#c4a7e7",
                "#ebbcba", "#e0def4",
            ],
        );
    }

    fn assert_colors<const N: usize>(actual: [Hsla; N], expected: [&str; N]) {
        assert_eq!(actual.map(format_hex_color), expected.map(str::to_owned));
    }

    #[test]
    fn hex_colors_round_trip_with_optional_alpha() {
        for value in ["#eff1f5", "#cba6f759", "#000000", "#ffffff00"] {
            assert_eq!(format_hex_color(parse_hex_color(value).unwrap()), value);
        }
        assert!(parse_hex_color("eff1f5").is_err());
        assert!(parse_hex_color("#xyzxyz").is_err());
    }

    #[test]
    fn custom_theme_round_trips_as_two_complete_variants() {
        let dir = tempfile::tempdir().unwrap();
        let mut catalog = ThemeCatalog::default();
        let mut draft = catalog.editable(CATPPUCCIN_THEME_ID);
        draft.name = "My Catppuccin".into();
        draft.light.accent = c("#123456");
        let id = catalog.save(&draft, dir.path()).unwrap();

        let loaded = ThemeCatalog::load(dir.path());
        let light = loaded.resolve(&id, Appearance::Light);
        let dark = loaded.resolve(&id, Appearance::Dark);
        assert_eq!(format_hex_color(light.accent), "#123456");
        assert_eq!(format_hex_color(dark.bg), "#1e1e2e");
        assert_eq!(light.palette_id.as_ref(), id);
        assert_eq!(dark.palette_revision, 1);
    }

    #[test]
    fn synced_records_install_and_remove_global_files() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let mut catalog = ThemeCatalog::default();
        let draft = catalog.editable(ROSE_PINE_THEME_ID);
        let id = catalog.save(&draft, source.path()).unwrap();
        let records = local_theme_records(source.path()).unwrap();

        install_synced_theme_files(&records, target.path()).unwrap();
        assert_eq!(ThemeCatalog::load(target.path()).summaries().len(), 4);
        assert!(
            themes_dir(target.path())
                .join(format!("{id}.json"))
                .exists()
        );

        install_synced_theme_files(
            &[jolt_proto::ThemeFileRecord {
                id,
                revision: 2,
                deleted: true,
                contents: String::new(),
            }],
            target.path(),
        )
        .unwrap();
        assert_eq!(ThemeCatalog::load(target.path()).summaries().len(), 3);
    }

    #[test]
    fn concurrent_theme_edits_preserve_both_versions() {
        let dir = tempfile::tempdir().unwrap();
        let mut catalog = ThemeCatalog::default();
        let draft = catalog.editable(JOLT_THEME_ID);
        catalog.save(&draft, dir.path()).unwrap();
        let known = local_theme_records(dir.path()).unwrap();
        let local = revised_record(&known[0], "Local edit", 2);
        let remote = revised_record(&known[0], "Remote edit", 2);

        let plan = plan_theme_file_sync(
            std::slice::from_ref(&local),
            std::slice::from_ref(&remote),
            &known,
        )
        .unwrap();
        assert!(plan.deletes.is_empty());
        assert_eq!(plan.upserts.len(), 1);
        assert_ne!(plan.upserts[0].id, known[0].id);
        let copy: ThemeFile = serde_json::from_str(&plan.upserts[0].contents).unwrap();
        assert_eq!(copy.name, "Local edit (conflict)");
        assert_eq!(copy.revision, 1);
        let projected = plan.project_onto(&[remote]);
        assert_eq!(projected.len(), 2);
    }

    #[test]
    fn concurrent_local_delete_preserves_remote_edit_as_a_copy() {
        let dir = tempfile::tempdir().unwrap();
        let mut catalog = ThemeCatalog::default();
        let draft = catalog.editable(JOLT_THEME_ID);
        let id = catalog.save(&draft, dir.path()).unwrap();
        let known = local_theme_records(dir.path()).unwrap();
        let remote = revised_record(&known[0], "Remote edit", 2);

        let plan = plan_theme_file_sync(&[], &[remote], &known).unwrap();
        assert_eq!(plan.deletes, [id]);
        assert_eq!(plan.upserts.len(), 1);
        let copy: ThemeFile = serde_json::from_str(&plan.upserts[0].contents).unwrap();
        assert_eq!(copy.name, "Remote edit (conflict)");
    }

    fn revised_record(
        record: &jolt_proto::ThemeFileRecord,
        name: &str,
        revision: u64,
    ) -> jolt_proto::ThemeFileRecord {
        let mut file: ThemeFile = serde_json::from_str(&record.contents).unwrap();
        file.name = name.into();
        file.revision = revision;
        jolt_proto::ThemeFileRecord {
            id: record.id.clone(),
            revision,
            deleted: false,
            contents: serde_json::to_string_pretty(&file).unwrap(),
        }
    }

    #[test]
    fn malformed_theme_file_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let themes = themes_dir(dir.path());
        std::fs::create_dir_all(&themes).unwrap();
        std::fs::write(themes.join("bad.json"), "{not json").unwrap();
        assert_eq!(ThemeCatalog::load(dir.path()).summaries().len(), 3);
    }
}
