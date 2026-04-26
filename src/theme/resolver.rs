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
        .filter_map(|token| {
            let (modifier, inner) = if let Some(pos) = token.find(':') {
                let (m, rest) = token.split_at(pos);
                (Some(m), &rest[1..])
            } else {
                (None, token)
            };

            let resolved = {
                let mut result = None;
                for prefix in &["bg-", "text-", "border-"] {
                    if let Some(key) = inner.strip_prefix(prefix) {
                        if let Some(value) = colors.get(key) {
                            let encoded = value.replace(' ', "_");
                            result = Some(format!("{}[{}]", prefix, encoded));
                            break;
                        }
                    }
                }
                if result.is_none() {
                    if let Some(key) = inner.strip_prefix("rounded-") {
                        if let Some(value) = theme.radius.get(key) {
                            result = Some(format!("rounded-[{}]", value));
                        }
                    }
                }
                result.unwrap_or_else(|| inner.to_string())
            };

            match modifier {
                Some("dark") => {
                    if mode == ThemeMode::Dark {
                        Some(resolved)
                    } else {
                        None
                    }
                }
                Some("light") => {
                    if mode == ThemeMode::Light {
                        Some(resolved)
                    } else {
                        None
                    }
                }
                Some(m) => Some(format!("{}:{}", m, resolved)),
                None => Some(resolved),
            }
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
    foreground: "oklch(0.985 0 0)"
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
    fn resolve_tw_text_foreground_dark_uses_dark_color_value() {
        let theme = test_theme();
        assert_eq!(
            resolve_tw("text-foreground", &theme, ThemeMode::Dark),
            "text-[oklch(0.985_0_0)]"
        );
    }

    #[test]
    fn resolve_tw_text_muted_foreground_matches_longest_key() {
        let theme = test_theme();
        assert_eq!(
            resolve_tw("text-muted-foreground", &theme, ThemeMode::Dark),
            "text-[oklch(0.708_0_0)]"
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

    #[test]
    fn resolve_tw_border_border_dark_uses_dark_color_value() {
        let theme = test_theme();
        assert_eq!(
            resolve_tw("border-border", &theme, ThemeMode::Dark),
            "border-[oklch(1_0_0_/_10%)]"
        );
    }

    #[test]
    fn resolve_tw_rounded_lg_substitutes_radius_value() {
        let theme = test_theme();
        assert_eq!(
            resolve_tw("rounded-lg", &theme, ThemeMode::Dark),
            "rounded-[0.625rem]"
        );
    }

    #[test]
    fn resolve_tw_breakpoint_prefix_stripped_resolved_and_reattached() {
        let theme = test_theme();
        assert_eq!(
            resolve_tw("md:bg-primary", &theme, ThemeMode::Dark),
            "md:bg-[oklch(0.922_0_0)]"
        );
    }

    #[test]
    fn resolve_tw_dark_modifier_dark_mode_emits_resolved_inner_token() {
        let theme = test_theme();
        assert_eq!(
            resolve_tw("dark:bg-primary", &theme, ThemeMode::Dark),
            "bg-[oklch(0.922_0_0)]"
        );
    }

    #[test]
    fn resolve_tw_dark_modifier_light_mode_drops_token() {
        let theme = test_theme();
        assert_eq!(
            resolve_tw("dark:bg-primary", &theme, ThemeMode::Light),
            ""
        );
    }

    #[test]
    fn resolve_tw_dark_modifier_light_mode_drops_token_from_multi_class_string() {
        let theme = test_theme();
        assert_eq!(
            resolve_tw("flex dark:bg-primary", &theme, ThemeMode::Light),
            "flex"
        );
    }

    #[test]
    fn resolve_tw_light_modifier_light_mode_emits_resolved_inner_token() {
        let theme = test_theme();
        assert_eq!(
            resolve_tw("light:bg-primary", &theme, ThemeMode::Light),
            "bg-[oklch(0.205_0_0)]"
        );
    }

    #[test]
    fn resolve_tw_light_modifier_dark_mode_drops_token() {
        let theme = test_theme();
        assert_eq!(
            resolve_tw("light:bg-primary", &theme, ThemeMode::Dark),
            ""
        );
    }

    #[test]
    fn resolve_tw_light_modifier_dark_mode_drops_token_from_multi_class_string() {
        let theme = test_theme();
        assert_eq!(
            resolve_tw("flex light:bg-primary", &theme, ThemeMode::Dark),
            "flex"
        );
    }
}
