use super::{Theme, ThemeMode};

pub fn resolve_tw_in_json(value: &mut serde_json::Value, theme: &Theme, mode: ThemeMode) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(tw) = map.get_mut("tw") {
                if let Some(s) = tw.as_str() {
                    let resolved = resolve_tw(s, theme, mode);
                    *tw = serde_json::Value::String(resolved);
                }
            }
            for v in map.values_mut() {
                resolve_tw_in_json(v, theme, mode);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                resolve_tw_in_json(v, theme, mode);
            }
        }
        _ => {}
    }
}

pub fn resolve_tw(classes: &str, theme: &Theme, mode: ThemeMode) -> String {
    let colors = theme.colors_for_mode(mode);
    classes
        .split_whitespace()
        .map(|token| {
            if let Some(key) = token.strip_prefix("bg-") {
                if let Some(value) = colors.get(key) {
                    let encoded = value.replace(' ', "_");
                    return format!("bg-[{}]", encoded);
                }
            }
            token.to_string()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_theme() -> Theme {
        Theme::from_yaml(r#"
colors:
  light:
    primary: "oklch(0.205 0 0)"
    muted-foreground: "oklch(0.556 0 0)"
    border: "oklch(0.922 0 0)"
  dark:
    primary: "oklch(0.922 0 0)"
    muted-foreground: "oklch(0.708 0 0)"
    border: "oklch(1 0 0 / 10%)"
radius:
  sm: "0.375rem"
  lg: "0.625rem"
"#).unwrap()
    }

    #[test]
    fn resolve_tw_bg_primary_dark_uses_dark_color_value() {
        let theme = test_theme();
        assert_eq!(
            resolve_tw("bg-primary", &theme, ThemeMode::Dark),
            "bg-[oklch(0.922_0_0)]"
        );
    }

    #[test]
    fn resolve_tw_bg_unknown_key_passes_through() {
        let theme = test_theme();
        assert_eq!(
            resolve_tw("bg-unknown", &theme, ThemeMode::Dark),
            "bg-unknown"
        );
    }

    #[test]
    fn resolve_tw_no_prefix_match_passes_through() {
        let theme = test_theme();
        assert_eq!(
            resolve_tw("flex", &theme, ThemeMode::Dark),
            "flex"
        );
    }

    #[test]
    fn resolve_tw_multiple_tokens_processed_independently() {
        let theme = test_theme();
        assert_eq!(
            resolve_tw("flex bg-primary", &theme, ThemeMode::Dark),
            "flex bg-[oklch(0.922_0_0)]"
        );
    }
}
