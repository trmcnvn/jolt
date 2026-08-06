use jolt_proto::Model;

const CHEAP_MODEL_MARKERS: &[&str] = &["luna", "haiku", "mini", "nano", "flash", "small", "lite"];

/// Pick an economy-tier model when the catalog advertises one. Otherwise keep
/// the configured fallback when available, then use the catalog's last model.
pub(crate) fn cheap_model_id(models: &[Model], fallback: Option<&str>) -> Option<String> {
    models
        .iter()
        .find(|model| {
            let name = format!("{} {}", model.id, model.label).to_lowercase();
            CHEAP_MODEL_MARKERS
                .iter()
                .any(|marker| name.contains(marker))
        })
        .or_else(|| fallback.and_then(|id| models.iter().find(|model| model.id == id)))
        .or_else(|| models.last())
        .map(|model| model.id.clone())
        .or_else(|| fallback.map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, label: &str) -> Model {
        Model {
            id: id.into(),
            label: label.into(),
            description: None,
            reasoning_levels: vec![],
            options: vec![],
        }
    }

    #[test]
    fn prefers_cheap_tier_then_fallback() {
        let models = vec![
            model("gpt-5.6-luna", "GPT-5.6-Luna"),
            model("gpt-5.4-mini", "GPT-5.4-Mini"),
            model("large", "Large"),
        ];
        assert_eq!(
            cheap_model_id(&models, Some("large")).as_deref(),
            Some("gpt-5.6-luna")
        );

        let models = vec![model("large", "Large"), model("other", "Other")];
        assert_eq!(
            cheap_model_id(&models, Some("large")).as_deref(),
            Some("large")
        );
        assert_eq!(cheap_model_id(&models, None).as_deref(), Some("other"));
        assert_eq!(
            cheap_model_id(&[], Some("configured")).as_deref(),
            Some("configured")
        );
        assert_eq!(cheap_model_id(&[], None), None);
    }
}
