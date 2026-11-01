//! Picks an extra UI scale on large screens, so a 1440p or 4K display at 100% still gets a readable window.

use std::process::{Command, Stdio};

const TARGET_HEIGHT: f64 = 1080.0;
const MIN_FACTOR: f64 = 1.2;
const MAX_FACTOR: f64 = 2.0;

/// The factor to hand to `QT_SCALE_FACTOR`, from the primary output's logical height in `kscreen-doctor -j` output.
fn factor_for(json: &str) -> Option<f64> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let output = value
        .get("outputs")?
        .as_array()?
        .iter()
        .filter(|o| o.get("enabled").and_then(|e| e.as_bool()).unwrap_or(false))
        .min_by_key(|o| match o.get("priority").and_then(|p| p.as_i64()) {
            Some(p) if p > 0 => p,
            _ => i64::MAX,
        })?;
    let height = output.get("size")?.get("height")?.as_f64()?;
    let scale = output.get("scale").and_then(|s| s.as_f64()).filter(|s| *s > 0.0).unwrap_or(1.0);
    let factor = (height / scale / TARGET_HEIGHT).min(MAX_FACTOR);
    (factor >= MIN_FACTOR).then(|| (factor * 20.0).round() / 20.0)
}

/// `XPM_UI_SCALE=off` or a number overrides detection; otherwise KDE's `kscreen-doctor` is asked.
/// Other desktops get no extra scale, because their own scaling is not visible before Qt starts.
pub(crate) fn detect() -> Option<f64> {
    if ["QT_SCALE_FACTOR", "QT_SCREEN_SCALE_FACTORS", "SLINT_SCALE_FACTOR"].iter().any(|v| std::env::var_os(v).is_some()) {
        return None;
    }
    if let Ok(v) = std::env::var("XPM_UI_SCALE") {
        return v.parse::<f64>().ok().filter(|f| (1.0..=4.0).contains(f) && *f > 1.0);
    }
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    if !desktop.to_ascii_uppercase().contains("KDE") {
        return None;
    }
    let out = Command::new("kscreen-doctor").arg("-j").stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    factor_for(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outputs(height: u32, scale: f64) -> String {
        format!(r#"{{"outputs":[{{"enabled":true,"priority":1,"scale":{scale},"size":{{"width":3840,"height":{height}}}}}]}}"#)
    }

    #[test]
    fn full_hd_and_laptops_stay_unscaled() {
        assert_eq!(factor_for(&outputs(1080, 1.0)), None);
        assert_eq!(factor_for(&outputs(1600, 1.6)), None);
    }

    #[test]
    fn big_screens_at_one_hundred_percent_get_scaled() {
        assert_eq!(factor_for(&outputs(1440, 1.0)), Some(1.35));
        assert_eq!(factor_for(&outputs(2160, 1.0)), Some(2.0));
    }

    #[test]
    fn existing_desktop_scaling_is_taken_into_account() {
        assert_eq!(factor_for(&outputs(2160, 2.0)), None);
        assert_eq!(factor_for(&outputs(2160, 1.5)), Some(1.35));
    }

    #[test]
    fn the_primary_enabled_output_decides() {
        let json = r#"{"outputs":[{"enabled":true,"priority":2,"scale":1,"size":{"height":1080}},{"enabled":true,"priority":1,"scale":1,"size":{"height":2160}},{"enabled":false,"priority":0,"scale":1,"size":{"height":480}}]}"#;
        assert_eq!(factor_for(json), Some(2.0));
        assert_eq!(factor_for("not json"), None);
    }
}
