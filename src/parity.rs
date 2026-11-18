//! Shared behavior ported from the original settings pages.
use crate::{input, model::Profile};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::HashMap;
pub const BUTTON_NAMES: &[&str] = &[
    "G1 · Left click",
    "G2 · Right click",
    "G3 · Wheel click",
    "G4 · Back",
    "G5 · Forward",
    "G6 · Sniper",
    "G7 · Front left",
    "G8 · Behind G7",
    "G9 · Behind wheel",
    "Wheel tilt right",
    "Wheel tilt left",
];
pub const MEDIA: &[(&str, &str)] = &[
    ("play-pause", "Play / pause"),
    ("next-track", "Next track"),
    ("prev-track", "Previous track"),
    ("stop", "Stop"),
    ("mute", "Mute"),
    ("volume-up", "Volume up"),
    ("volume-down", "Volume down"),
    ("browser-back", "Browser back"),
    ("browser-forward", "Browser forward"),
    ("browser-refresh", "Browser refresh"),
    ("browser-home", "Browser home"),
    ("browser-search", "Browser search"),
    ("file-browser", "File browser"),
    ("calculator", "Calculator"),
    ("mail", "Mail"),
    ("brightness-up", "Brightness up"),
    ("brightness-down", "Brightness down"),
];
pub const SHORTCUTS: &[(&str, &str)] = &[
    ("Copy", "ctrl+c"),
    ("Paste", "ctrl+v"),
    ("Cut", "ctrl+x"),
    ("Undo", "ctrl+z"),
    ("Redo", "ctrl+shift+z"),
    ("Select all", "ctrl+a"),
    ("Save", "ctrl+s"),
    ("Find", "ctrl+f"),
    ("New tab", "ctrl+t"),
    ("Close tab", "ctrl+w"),
    ("Reopen tab", "ctrl+shift+t"),
    ("Next tab", "ctrl+tab"),
    ("Previous tab", "ctrl+shift+tab"),
    ("Switch window", "alt+tab"),
    ("Show desktop", "meta+d"),
    ("Lock screen", "meta+l"),
    ("Screenshot", "printscreen"),
    ("Escape", "esc"),
    ("Enter", "enter"),
    ("Delete", "delete"),
];
pub fn shortcut_binding(combo: &str, typing: &HashMap<char, (u8, u16)>) -> String {
    if let Some(key) = combo.rsplit('+').next().filter(|k| k.len() == 1)
        && let Some((_, code)) = typing.get(&key.chars().next().unwrap())
        && let Some((usage, _)) = input::keys::HID_EVDEV.iter().find(|(_, ev)| ev == code)
        && let Ok((mods, _)) = input::combo(combo)
    {
        return input::describe_binding([128, 2, mods, *usage as u8]);
    }
    format!("key:{combo}")
}
pub fn dpi_range(payload: &[u8]) -> Result<[u16; 3]> {
    let mut plain = vec![];
    let mut step = 1;
    for v in payload
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&p| u16::from_be_bytes(p))
    {
        if v == 0 {
            break;
        }
        if v >> 13 == 7 {
            step = v & 0x1fff;
        } else {
            plain.push(v);
        }
    }
    let lo = *plain.iter().min().context("No sensor DPI values")?;
    let hi = *plain.iter().max().unwrap();
    ensure!(lo > 0 && step > 0 && hi >= lo, "Invalid sensor DPI range");
    Ok([lo, hi, step])
}
pub fn quantize(dpi: f32, range: [u16; 3]) -> u16 {
    let [lo, hi, step] = range;
    ((dpi / step.max(1) as f32).round() * step.max(1) as f32).clamp(lo as f32, hi as f32) as u16
}
/// Original indices are retained through insertions so default/sniper/active follow their DPI.
pub fn relevel(profile: &mut Profile, values: &[(usize, u16)], active: usize) -> usize {
    let find = |old: usize| values.iter().position(|(slot, _)| *slot == old);
    let default = find(profile.default_dpi as usize).unwrap_or(0);
    let shift = find(profile.shift_dpi as usize).unwrap_or(0);
    profile.dpi = std::array::from_fn(|i| values.get(i).map(|(_, v)| *v).unwrap_or(0));
    profile.default_dpi = default as u8;
    profile.shift_dpi = shift as u8;
    find(active).unwrap_or(default)
}
pub fn macro_size(m: &Value, typing: &HashMap<char, (u8, u16)>) -> Result<Option<usize>> {
    ensure!(
        m["steps"].as_array().is_some_and(|s| !s.is_empty()),
        "Add at least one step."
    );
    let events = input::expand(m, typing)?;
    if m["target"] == "mouse" {
        ensure!(
            m["mode"].as_str().unwrap_or("once") == "once",
            "Macros saved on the mouse play once per press"
        );
        Ok(Some(input::onboard_macro(&events)?.len()))
    } else {
        Ok(None)
    }
}
pub fn templates() -> Vec<Value> {
    [("YouTube", "media", "youtube", "color", "#ff0000"), ("Netflix", "media", "netflix", "color", "#e50914"),
     ("Twitch", "media", "twitch", "color", "#9146ff"), ("Spotify", "media", "spotify", "audio", "#14b4ff"),
     ("Any Steam game", "game", "any", "art", "#14b4ff"), ("Counter-Strike 2", "game", "730", "game", "#14b4ff"),
     ("Dota 2", "game", "570", "game", "#14b4ff")].into_iter().map(|(name, trigger, value, lighting, color)|
        json!({"name":name,"trigger":trigger,"value":value,"lighting":lighting,"color":color,"period_ms":4000,"slot":null,"enabled":true})).collect()
}
pub fn macro_on(
    settings: &Value,
    slot: usize,
    layer: usize,
    button: usize,
    binding: &str,
) -> Option<String> {
    if let Some(id) = settings["app_macros"][format!("{slot}:{layer}:{button}")].as_str() {
        return Some(id.into());
    }
    let raw = input::binding(binding).ok()?;
    if raw[0] < 128
        && let Some(pages) = settings["macro_pages"].as_object()
    {
        return pages
            .iter()
            .find(|(_, page)| page.as_u64() == Some(raw[1] as u64))
            .map(|(id, _)| id.clone());
    }
    None
}
fn key_usage_label(usage: u8, typing: &HashMap<char, (u8, u16)>) -> String {
    let name = input::keys::KEYS
        .iter()
        .find(|(_, u)| *u == usage as u16)
        .map(|(n, _)| *n)
        .unwrap_or("");
    match name {
        "enter" => return "Enter".into(),
        "esc" => return "Esc".into(),
        "backspace" => return "Backspace".into(),
        "tab" => return "Tab".into(),
        "space" => return "Space".into(),
        "capslock" => return "Caps Lock".into(),
        "printscreen" => return "Print Screen".into(),
        "scrolllock" => return "Scroll Lock".into(),
        "pause" => return "Pause".into(),
        "insert" => return "Insert".into(),
        "home" => return "Home".into(),
        "pageup" => return "Page Up".into(),
        "delete" => return "Delete".into(),
        "end" => return "End".into(),
        "pagedown" => return "Page Down".into(),
        "right" => return "→".into(),
        "left" => return "←".into(),
        "down" => return "↓".into(),
        "up" => return "↑".into(),
        "numlock" => return "Num Lock".into(),
        "kpenter" => return "Num Enter".into(),
        "kpslash" => return "Num /".into(),
        "kpasterisk" => return "Num *".into(),
        "kpminus" => return "Num -".into(),
        "kpplus" => return "Num +".into(),
        "kpdot" => return "Num .".into(),
        "menu" => return "Menu".into(),
        _ => {}
    }
    if let Some(digit) = name
        .strip_prefix("kp")
        .filter(|d| d.chars().all(|c| c.is_ascii_digit()))
    {
        return format!("Num {digit}");
    }
    if let Some(fn_num) = name
        .strip_prefix('f')
        .filter(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()))
    {
        return format!("F{fn_num}");
    }
    if let Some((_, evdev)) = input::keys::HID_EVDEV
        .iter()
        .find(|(h, _)| *h == usage as u16)
        && let Some((&ch, _)) = typing
            .iter()
            .find(|(c, (mods, code))| *mods == 0 && code == evdev && !c.is_whitespace())
    {
        return ch.to_uppercase().to_string();
    }
    if name.len() == 1 {
        name.to_uppercase()
    } else if !name.is_empty() {
        name.into()
    } else {
        format!("Key 0x{usage:02X}")
    }
}
pub fn friendly_binding(binding: &str, typing: &HashMap<char, (u8, u16)>) -> String {
    let Ok(raw) = input::binding(binding) else {
        return binding.into();
    };
    match raw {
        [255, 255, 255, 255] => "Not set".into(),
        [128, 0, _, _] => "Disabled".into(),
        [128, 1, a, c] => match u16::from_be_bytes([a, c]) {
            0x0001 => "Left click".into(),
            0x0002 => "Right click".into(),
            0x0004 => "Middle click".into(),
            0x0008 => "Back".into(),
            0x0010 => "Forward".into(),
            _ => binding.into(),
        },
        [128, 2, mods, usage] => {
            let mut parts = Vec::new();
            for (bit, label) in [
                (0x01, "Ctrl"),
                (0x02, "Shift"),
                (0x04, "Alt"),
                (0x08, "Meta"),
                (0x10, "Right Ctrl"),
                (0x20, "Right Shift"),
                (0x40, "AltGr"),
                (0x80, "Right Meta"),
            ] {
                if mods & bit != 0 {
                    parts.push(label.to_string());
                }
            }
            if usage != 0 {
                parts.push(key_usage_label(usage, typing));
            }
            if parts.is_empty() {
                "None".into()
            } else {
                parts.join(" + ")
            }
        }
        [128, 3, a, c] => {
            let u = u16::from_be_bytes([a, c]);
            if let Some((key, _)) = input::keys::CONSUMER.iter().find(|(_, code)| *code == u)
                && let Some((_, label)) = MEDIA.iter().find(|(k, _)| k == key)
            {
                return (*label).into();
            }
            binding.into()
        }
        [144, f, _, arg] if (f as usize) < input::SPECIAL.len() => match input::SPECIAL[f as usize]
        {
            "noop" => "Nothing".into(),
            "tilt-left" => "Scroll left".into(),
            "tilt-right" => "Scroll right".into(),
            "dpi-up" => "DPI up".into(),
            "dpi-down" => "DPI down".into(),
            "dpi-cycle" => "DPI cycle".into(),
            "dpi-default" => "DPI default".into(),
            "dpi-shift" => "DPI shift (sniper)".into(),
            "profile-next" => "Next profile".into(),
            "profile-prev" => "Previous profile".into(),
            "profile-cycle" => "Cycle profiles".into(),
            "gshift" => "G-Shift (hold)".into(),
            "battery" => "Battery status".into(),
            "profile-select" => format!("Switch to profile {arg}"),
            "mode-switch" => "Mode switch".into(),
            "host" => "Host button".into(),
            "scroll-down" => "Scroll down".into(),
            "scroll-up" => "Scroll up".into(),
            other => other.into(),
        },
        [b0, _, _, _] if b0 < 128 => "Onboard macro".into(),
        _ => binding.into(),
    }
}
pub fn assignment_label(
    settings: &Value,
    slot: usize,
    layer: usize,
    button: usize,
    binding: &str,
) -> String {
    if let Some(id) = macro_on(settings, slot, layer, button, binding)
        && let Some(m) = settings["macros"]
            .as_array()
            .and_then(|items| items.iter().find(|m| m["id"] == id))
    {
        return format!(
            "Macro “{}”, {}",
            m["name"].as_str().unwrap_or("Macro"),
            if m["target"] == "mouse" {
                "saved on the mouse"
            } else {
                "run by the app"
            }
        );
    }
    binding.into()
}
pub fn friendly_assignment_label(
    settings: &Value,
    slot: usize,
    layer: usize,
    button: usize,
    binding: &str,
    typing: &HashMap<char, (u8, u16)>,
) -> String {
    let label = assignment_label(settings, slot, layer, button, binding);
    if label != binding {
        label
    } else {
        friendly_binding(binding, typing)
    }
}
pub fn remember(items: &mut Vec<String>, value: &str) {
    if value.is_empty() {
        return;
    }
    items.retain(|v| v != value);
    items.insert(0, value.into());
    items.truncate(12);
}
pub fn trigger_description(rule: &Value, games: &[Value]) -> String {
    let value = rule["value"].as_str().unwrap_or("");
    match rule["trigger"].as_str().unwrap_or("") {
        "game" if value == "any" => "While any game is running".into(),
        "game" => format!(
            "While {} is running",
            games
                .iter()
                .find(|g| {
                    g["appid"].as_str() == Some(value)
                        || g["appid"]
                            .as_u64()
                            .is_some_and(|id| value.parse::<u64>() == Ok(id))
                })
                .and_then(|g| g["name"].as_str())
                .map(str::to_owned)
                .unwrap_or(format!("Steam app {value}"))
        ),
        "app" => format!("While {value} is in front"),
        "title" => format!("While a window title contains “{value}”"),
        _ => format!("While playing media contains “{value}”"),
    }
}
pub fn action_description(rule: &Value) -> String {
    let kind = rule["lighting"].as_str().unwrap_or("");
    let label = match kind {
        "color" => "Color",
        "breathing" => "Breathing",
        "art" => "Cover art colors",
        "game" => "Game-driven",
        "screen" => "Screen sampler",
        "audio" => "Audio visualizer",
        "wave" => "Color wave",
        "dpi" => "DPI colors",
        _ => "",
    };
    let mut parts = vec![];
    if !label.is_empty() {
        parts.push(if ["color", "breathing"].contains(&kind) {
            format!("{label} {}", rule["color"].as_str().unwrap_or("#14b4ff"))
        } else {
            label.into()
        });
    }
    if let Some(slot) = rule["slot"].as_u64().filter(|n| *n > 0) {
        parts.push(format!("profile slot {slot}"));
    }
    if parts.is_empty() {
        "nothing yet".into()
    } else {
        parts.join(", ")
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sensor_ranges_and_rounding() {
        assert_eq!(
            dpi_range(&[0, 100, 0xe0, 50, 0x64, 0, 0, 0]).unwrap(),
            [100, 25600, 50]
        );
        assert_eq!(quantize(477., [100, 25600, 50]), 500);
        assert!(dpi_range(&[0, 0]).is_err());
    }
    #[test]
    fn macro_storage_constraints_and_media_catalogue() {
        let mut m = json!({"mode":"once","target":"mouse","steps":[{"kind":"click","args":{"button":"left"}}]});
        assert_eq!(macro_size(&m, &HashMap::new()).unwrap(), Some(7));
        m["mode"] = json!("toggle");
        assert!(macro_size(&m, &HashMap::new()).is_err());
        m["steps"] = json!([]);
        assert!(macro_size(&m, &HashMap::new()).is_err());
        for (key, _) in MEDIA {
            assert!(input::binding(&format!("media:{key}")).is_ok(), "{key}");
        }
        for (_, key) in SHORTCUTS {
            assert!(input::combo(key).is_ok(), "{key}");
        }
    }
    #[test]
    fn recent_values_and_templates() {
        let mut items = vec![];
        for n in 0..20 {
            remember(&mut items, &n.to_string());
        }
        remember(&mut items, "12");
        assert_eq!(items.len(), 12);
        assert_eq!(items[0], "12");
        assert_eq!(items.iter().filter(|v| *v == "12").count(), 1);
        assert_eq!(templates().len(), 7);
    }
    #[test]
    fn friendly_labels_respect_keyboard_layout_and_special_actions() {
        let fr = input::typing_table("fr").unwrap();
        let us = input::typing_table("us").unwrap();
        let fr_select_all = shortcut_binding("ctrl+a", &fr);
        assert_eq!(fr_select_all, "key:ctrl+q");
        assert_eq!(friendly_binding(&fr_select_all, &fr), "Ctrl + A");
        assert_eq!(friendly_binding("key:ctrl+a", &us), "Ctrl + A");
        assert_eq!(friendly_binding("left", &us), "Left click");
        assert_eq!(friendly_binding("dpi-shift", &us), "DPI shift (sniper)");
        assert_eq!(friendly_binding("unassigned", &us), "Not set");
        assert_eq!(friendly_binding("disabled", &us), "Disabled");
        assert_eq!(friendly_binding("media:play-pause", &us), "Play / pause");
        assert_eq!(
            friendly_binding("profile-select:3", &us),
            "Switch to profile 3"
        );
        assert_eq!(friendly_binding("key:f5", &us), "F5");
        assert_eq!(friendly_binding("key:kp7", &us), "Num 7");
        let settings = json!({"macros":[{"id":"m1","name":"Burst","target":"app"}],"app_macros":{"1:0:5":"m1"}});
        assert_eq!(
            friendly_assignment_label(&settings, 1, 0, 5, "disabled", &us),
            "Macro “Burst”, run by the app"
        );
        assert_eq!(
            friendly_assignment_label(&settings, 1, 0, 0, "left", &us),
            "Left click"
        );
    }
    #[test]
    fn trigger_description_resolves_numeric_and_string_appids() {
        let games = vec![
            json!({"appid": 730, "name": "Counter-Strike 2"}),
            json!({"appid": "570", "name": "Dota 2"}),
        ];
        assert_eq!(
            trigger_description(&json!({"trigger":"game","value":"730"}), &games),
            "While Counter-Strike 2 is running"
        );
        assert_eq!(
            trigger_description(&json!({"trigger":"game","value":"570"}), &games),
            "While Dota 2 is running"
        );
    }
}
