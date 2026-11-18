use crate::{
    config,
    context::{self, Context, Watcher},
    device::{Device, Info},
    effects::Live,
    input::{self, Player},
    ipc,
    model::{self, Directory, Led, Profile},
};
use anyhow::{Context as _, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::{fd::AsRawFd, unix::net::UnixListener},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub connected: bool,
    pub name: String,
    pub firmware: Vec<String>,
    pub info: Option<Info>,
    pub directory: Option<Directory>,
    pub profiles: Vec<Profile>,
    #[serde(default)]
    pub factory: Vec<Profile>,
    #[serde(default = "default_dpi_range")]
    pub dpi_range: [u16; 3],
    pub slot: usize,
    pub dpi_index: usize,
    pub settings: Value,
    pub context: Context,
    #[serde(default)]
    pub seen_apps: Vec<String>,
    #[serde(default)]
    pub seen_media: Vec<String>,
    pub active_rule: Option<Value>,
    pub colors: Option<[[u8; 3]; 2]>,
    #[serde(default = "default_service_version")]
    pub service_version: String,
    #[serde(default)]
    pub update: Option<crate::updater::UpdateInfo>,
    pub status: String,
    pub error: Option<String>,
    pub macro_error: Option<String>,
    pub pid: u32,
}
fn default_service_version() -> String {
    env!("CARGO_PKG_VERSION").into()
}
fn default_dpi_range() -> [u16; 3] {
    [100, 25600, 50]
}
enum BgUpdate {
    Found(Option<crate::updater::UpdateInfo>),
    AutoInstalled(String, std::path::PathBuf),
}
struct Engine {
    device: Option<Device>,
    snapshot: Snapshot,
    watcher: Option<Watcher>,
    context_until: Option<Instant>,
    live: Live,
    player: Player,
    typing: HashMap<char, (u8, u16)>,
    spy: Option<u8>,
    last_spy_start: Instant,
    mask: u16,
    backup_done: bool,
    slot_before: Option<usize>,
    rule_id: Option<String>,
    blocked_rule: Option<String>,
    last_colors: Option<[[u8; 3]; 2]>,
    last_send: Instant,
    update_rx: Option<std::sync::mpsc::Receiver<BgUpdate>>,
    next_update_check: Instant,
    pending_handoff: Option<(String, std::path::PathBuf)>,
}
impl Engine {
    fn new() -> Result<Self> {
        config::import_python()?;
        let settings = config::load()?;
        let l = settings["keyboard_layout"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(input::layout);
        let typing = input::typing_table(&l)?;
        let mut e = Self {
            device: None,
            snapshot: Snapshot {
                settings,
                service_version: default_service_version(),
                pid: std::process::id(),
                ..Default::default()
            },
            watcher: None,
            context_until: None,
            live: Live::default(),
            player: Player::default(),
            typing,
            spy: None,
            last_spy_start: Instant::now(),
            mask: 0,
            backup_done: false,
            slot_before: None,
            rule_id: None,
            blocked_rule: None,
            last_colors: None,
            last_send: Instant::now(),
            update_rx: None,
            next_update_check: Instant::now() + Duration::from_secs(20),
            pending_handoff: None,
        };
        e.update_watcher();
        Ok(e)
    }
    fn update_watcher(&mut self) {
        let want = self
            .context_until
            .is_some_and(|until| until > Instant::now())
            || self.snapshot.settings["rules"]
                .as_array()
                .is_some_and(|r| r.iter().any(|r| r["enabled"].as_bool().unwrap_or(true)));
        if want && self.watcher.is_none() {
            self.watcher = Some(Watcher::new());
        } else if !want {
            self.watcher = None;
            self.snapshot.context = Context::default();
        }
    }
    fn connect(&mut self) -> Result<()> {
        let mut d = Device::open()?;
        if d.mode()? != 1 {
            d.set_mode(1)?;
        }
        let directory = d.directory()?;
        ensure!(
            !directory.entries.is_empty() && directory.entries.len() <= 16,
            "Invalid profile count"
        );
        let mut profiles = vec![];
        for e in &directory.entries {
            profiles.push(d.profile(e.sector)?);
        }
        self.snapshot.dpi_range = d.dpi_range()?;
        let raw = d.read_sector(256)?;
        let factory_dir = Directory::decode(&raw)?.context("No factory profiles")?;
        let mut factory = vec![];
        for entry in factory_dir.entries {
            factory.push(d.profile(entry.sector)?);
        }
        self.snapshot.factory = factory;
        let (sector, dpi) = d.current()?;
        self.snapshot.name = d.name.clone();
        self.snapshot.firmware = d.firmware.clone();
        self.snapshot.info = Some(d.info.clone());
        self.snapshot.slot = directory
            .entries
            .iter()
            .position(|e| e.sector == sector)
            .map(|n| n + 1)
            .unwrap_or(1);
        self.snapshot.dpi_index = dpi as usize;
        self.snapshot.directory = Some(directory);
        self.snapshot.profiles = profiles;
        self.snapshot.connected = true;
        self.snapshot.status = "Ready".into();
        self.snapshot.error = None;
        self.device = Some(d);
        self.backup_done = false;
        self.mask = 0;
        self.rule_id = None;
        self.sync_spy()?;
        Ok(())
    }
    fn sync_spy(&mut self) -> Result<()> {
        let need = self.snapshot.settings["app_macros"]
            .as_object()
            .is_some_and(|m| !m.is_empty());
        if let Some(d) = &mut self.device {
            // USB resets can silently disable notifications while HID++ polling
            // still works. Renew the subscription without stopping held macros.
            if need
                && (self.spy.is_none() || self.last_spy_start.elapsed() >= Duration::from_secs(5))
            {
                self.spy = Some(d.spy_start()?);
                self.last_spy_start = Instant::now();
            } else if !need && self.spy.is_some() {
                d.spy_stop()?;
                self.spy = None;
                self.player.stop_all();
            }
        }
        Ok(())
    }
    fn lost(&mut self, error: anyhow::Error) {
        self.player.stop_all();
        self.device = None;
        self.spy = None;
        self.mask = 0;
        self.snapshot.connected = false;
        self.snapshot.error = Some(error.to_string());
        self.snapshot.status = "Mouse disconnected — reconnecting".into();
    }
    fn current_profile(&self) -> Option<&Profile> {
        self.snapshot
            .slot
            .checked_sub(1)
            .and_then(|i| self.snapshot.profiles.get(i))
    }
    fn buttons(&mut self, mask: u16) {
        let old = self.mask;
        self.mask = mask;
        let layer = self.current_profile().is_some_and(|p| {
            p.buttons
                .iter()
                .enumerate()
                .any(|(i, b)| b[0] == 144 && b[1] == 11 && (old | mask) & (1 << i) != 0)
        });
        let slot = self.snapshot.slot;
        for button in 0..16 {
            let bit = 1u16 << button;
            if (old ^ mask) & bit == 0 {
                continue;
            }
            let key = format!("{slot}:{}:{button}", u8::from(layer));
            if mask & bit == 0 {
                self.player.release(&key);
                self.player
                    .release(&format!("{slot}:{}:{button}", u8::from(!layer)));
                continue;
            }
            let mid = self.snapshot.settings["app_macros"][&key].as_str();
            let m = mid
                .and_then(|id| {
                    self.snapshot.settings["macros"]
                        .as_array()?
                        .iter()
                        .find(|m| m["id"].as_str() == Some(id))
                })
                .cloned();
            if let Some(m) = m {
                self.snapshot.macro_error = None;
                let result = input::expand(&m, &self.typing).and_then(|events| {
                    self.player
                        .press(key, events, m["mode"].as_str().unwrap_or("once"))
                });
                if let Err(e) = result {
                    self.snapshot.macro_error = Some(e.to_string());
                }
            }
        }
    }
    fn events(&mut self) -> Result<()> {
        let Some(d) = &mut self.device else {
            return Ok(());
        };
        d.hid.drain()?;
        let mut masks = vec![];
        while let Some((index, event, params)) = d.hid.events.pop_front() {
            if Some(index) == self.spy && event == 0 && params.len() >= 2 {
                masks.push(u16::from_be_bytes([params[0], params[1]]));
            }
        }
        for mask in masks {
            self.buttons(mask);
        }
        self.player.reap();
        if let Some(e) = self.player.error.lock().unwrap().take() {
            self.snapshot.macro_error = Some(e);
        }
        Ok(())
    }
    fn poll(&mut self) -> Result<()> {
        if let Some(d) = &mut self.device {
            let (sector, dpi) = d.current()?;
            let before = (self.snapshot.slot, self.snapshot.dpi_index);
            if let Some(slot) = self
                .snapshot
                .directory
                .as_ref()
                .and_then(|dir| dir.entries.iter().position(|e| e.sector == sector))
            {
                self.snapshot.slot = slot + 1;
            }
            self.snapshot.dpi_index = dpi as usize;
            if before != (self.snapshot.slot, self.snapshot.dpi_index)
                && self.snapshot.settings["osd"].as_bool().unwrap_or(true)
            {
                let text = if before.0 != self.snapshot.slot {
                    format!(
                        "{} · Profile {}",
                        self.current_profile()
                            .and_then(|p| p.name.as_deref())
                            .filter(|s| !s.is_empty())
                            .unwrap_or("G502"),
                        self.snapshot.slot
                    )
                } else {
                    format!(
                        "{} DPI",
                        self.current_profile()
                            .and_then(|p| p.dpi.get(dpi as usize))
                            .copied()
                            .unwrap_or(0)
                    )
                };
                queue_osd(text);
            }
        }
        self.sync_spy()?;
        Ok(())
    }
    fn rules(&mut self) -> Result<()> {
        self.update_watcher();
        if let Some(w) = &self.watcher {
            self.snapshot.context = w.snapshot();
        }
        self.evaluate_rules()
    }
    fn evaluate_rules(&mut self) -> Result<()> {
        crate::parity::remember(&mut self.snapshot.seen_apps, &self.snapshot.context.app);
        for media in &self.snapshot.context.media {
            if media.status == "Playing" {
                crate::parity::remember(&mut self.snapshot.seen_media, &media.title);
            }
        }
        let rule = self.snapshot.settings["rules"]
            .as_array()
            .and_then(|r| {
                r.iter()
                    .find(|r| context::matches(r, &self.snapshot.context))
            })
            .cloned();
        let id = rule
            .as_ref()
            .and_then(|r| r["id"].as_str())
            .map(str::to_owned);
        if id != self.rule_id {
            self.blocked_rule = None;
            let old = self.snapshot.active_rule.clone();
            if let Some(slot) = rule
                .as_ref()
                .and_then(|r| r["slot"].as_u64())
                .filter(|&n| n > 0)
            {
                if self.slot_before.is_none() {
                    self.slot_before = Some(self.snapshot.slot);
                }
                if self.snapshot.connected
                    && let Err(e) = self.activate(slot as usize)
                {
                    self.snapshot.error = Some(e.to_string());
                }
            } else if let Some(slot) = self.slot_before.take()
                && old.as_ref().and_then(|r| r["slot"].as_u64()) == Some(self.snapshot.slot as u64)
                && self.snapshot.connected
                && let Err(e) = self.activate(slot)
            {
                self.snapshot.error = Some(e.to_string());
            }
            self.rule_id = id;
        }
        self.snapshot.active_rule = rule;
        Ok(())
    }
    fn effects(&mut self) -> Result<()> {
        let profile = self.current_profile().cloned();
        let previous = self.live.kind.clone();
        let colors = self.live.frame(
            &self.snapshot.settings,
            self.snapshot
                .active_rule
                .as_ref()
                .filter(|rule| rule["id"].as_str() != self.blocked_rule.as_deref()),
            &self.snapshot.context,
            profile.as_ref(),
            self.snapshot.dpi_index,
        );
        if let Some(c) = colors {
            self.snapshot.colors = Some(c);
            if (self.last_colors != Some(c) || self.last_send.elapsed() > Duration::from_secs(1))
                && let Some(d) = &mut self.device
            {
                for (z, rgb) in c.iter().enumerate() {
                    d.led(z, &Led::fixed(*rgb))?;
                }
                self.last_colors = Some(c);
                self.last_send = Instant::now();
            }
        } else if self.live.kind.is_empty() {
            self.snapshot.colors = None;
            if !previous.is_empty() {
                self.restore_lighting()?;
                self.last_colors = None;
            }
        }
        if let Some(token) = self.live.restore_token.take()
            && self.snapshot.settings["screen_token"].as_str() != Some(&token)
        {
            self.snapshot.settings["screen_token"] = token.into();
            config::save(&self.snapshot.settings)?;
        }
        if let Some(error) = self.live.error.clone() {
            self.snapshot.error = Some(error);
            if ["screen", "audio"].contains(&self.live.kind.as_str()) {
                if let Some(rule) = &self.snapshot.active_rule {
                    self.blocked_rule = rule["id"].as_str().map(str::to_owned);
                } else {
                    self.snapshot.settings["live"]["on"] = json!(false);
                    config::save(&self.snapshot.settings)?;
                }
                self.live.choose_screen();
                self.restore_lighting()?;
                self.last_colors = None;
                self.snapshot.colors = None;
            }
        }
        Ok(())
    }
    fn restore_lighting(&mut self) -> Result<()> {
        let leds = self.current_profile().map(|p| p.leds.clone());
        if let (Some(d), Some(leds)) = (&mut self.device, leds) {
            for (z, led) in leds.iter().enumerate() {
                d.led(z, led)?;
            }
        }
        Ok(())
    }
    fn d(&mut self) -> Result<&mut Device> {
        self.device.as_mut().context("G502 HERO not connected")
    }
    fn sector(&self, slot: usize) -> Result<u16> {
        self.snapshot
            .directory
            .as_ref()
            .and_then(|d| slot.checked_sub(1).and_then(|i| d.entries.get(i)))
            .map(|e| e.sector)
            .context("Invalid profile")
    }
    fn activate(&mut self, slot: usize) -> Result<()> {
        let sector = self.sector(slot)?;
        ensure!(
            self.snapshot.directory.as_ref().unwrap().entries[slot - 1].enabled,
            "Enable this profile before activating it"
        );
        self.d()?.activate(sector)?;
        self.snapshot.slot = slot;
        self.snapshot.dpi_index = self
            .current_profile()
            .map(|p| p.default_dpi as usize)
            .unwrap_or(0);
        self.last_colors = None;
        Ok(())
    }
    fn backup(&mut self, path: Option<&str>) -> Result<String> {
        let data = self.d()?.backup()?;
        let auto_dir = config::data_dir().join("backups");
        let p = if let Some(path) = path {
            std::path::PathBuf::from(path)
        } else {
            auto_dir.join(format!(
                "onboard-{}.json",
                SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis()
            ))
        };
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&p, serde_json::to_vec_pretty(&data)?)?;
        if path.is_none() {
            prune_auto_backups(&auto_dir, 50);
        }
        self.backup_done = true;
        Ok(p.display().to_string())
    }
    fn before_write(&mut self) -> Result<()> {
        if !self.backup_done {
            self.backup(None)?;
        }
        Ok(())
    }
    fn write_profile(&mut self, slot: usize, mut profile: Profile) -> Result<()> {
        let sector = self.sector(slot)?;
        let bytes = profile.encode()?;
        self.before_write()?;
        self.d()?.write_sector(sector, &bytes, false)?;
        profile.raw = bytes;
        let current = slot == self.snapshot.slot;
        let dpi = self.snapshot.dpi_index as u8;
        if current {
            self.d()?.reload(dpi)?;
        }
        let mut assignments_changed = false;
        if let Some(map) = self.snapshot.settings["app_macros"].as_object_mut() {
            for (layer, bindings) in [&profile.buttons, &profile.gshift].iter().enumerate() {
                for (button, binding) in bindings.iter().enumerate() {
                    if binding[..2] != [128, 0] {
                        assignments_changed |=
                            map.remove(&format!("{slot}:{layer}:{button}")).is_some();
                    }
                }
            }
        }
        if assignments_changed {
            self.player.stop_all();
            config::save(&self.snapshot.settings)?;
            self.sync_spy()?;
        }
        self.snapshot.profiles[slot - 1] = profile;
        self.last_colors = None;
        self.snapshot.status = "Profile saved and verified on mouse".into();
        Ok(())
    }
    fn clear_app_assignments(&mut self, slot: usize) -> Result<()> {
        if let Some(map) = self.snapshot.settings["app_macros"].as_object_mut() {
            map.retain(|k, _| !k.starts_with(&format!("{slot}:")));
        }
        config::save(&self.snapshot.settings)?;
        self.sync_spy()
    }
    fn settings(&mut self, value: Value) -> Result<()> {
        ensure!(value.is_object(), "Invalid settings");
        if let Some(macros) = value["macros"].as_array() {
            ensure!(macros.len() <= 4096, "Too many macros");
            for m in macros {
                input::expand(m, &self.typing)?;
            }
        }
        if let Some(rules) = value["rules"].as_array() {
            for r in rules {
                ensure!(
                    ["game", "app", "title", "media"]
                        .contains(&r["trigger"].as_str().unwrap_or("")),
                    "Invalid rule trigger"
                );
            }
        }
        if let Some(pages) = value["macro_pages"].as_object() {
            for page in pages.values() {
                ensure!(
                    page.as_u64()
                        .is_some_and(|n| macro_page_allowed(&self.snapshot, n)),
                    "Invalid macro page: profiles and factory memory are protected"
                );
            }
        }
        let typing = value["keyboard_layout"]
            .as_str()
            .map(input::typing_table)
            .transpose()?;
        if ["macros", "app_macros", "macro_pages", "keyboard_layout"]
            .iter()
            .any(|key| value[*key] != self.snapshot.settings[*key])
        {
            self.player.stop_all();
        }
        config::save(&value)?;
        if let Some(typing) = typing {
            self.typing = typing;
        }
        self.blocked_rule = None;
        self.snapshot.error = None;
        self.snapshot.settings = value;
        self.update_watcher();
        self.sync_spy()?;
        self.snapshot.status = "Settings saved".into();
        Ok(())
    }
    fn macro_sector(&mut self, id: &str) -> Result<u16> {
        if let Some(n) = self.snapshot.settings["macro_pages"][id].as_u64() {
            ensure!(
                macro_page_allowed(&self.snapshot, n),
                "Macro page overlaps a profile or is outside user memory"
            );
            return Ok(n as u16);
        }
        let profile_sectors: Vec<_> = self
            .snapshot
            .directory
            .as_ref()
            .context("No directory")?
            .entries
            .iter()
            .map(|e| e.sector)
            .collect();
        let count = self.d()?.info.sector_count;
        for n in 1..count as u16 {
            if profile_sectors.contains(&n) {
                continue;
            }
            let used = self.snapshot.settings["macro_pages"]
                .as_object()
                .is_some_and(|m| m.values().any(|v| v.as_u64() == Some(n as u64)));
            let referenced = self
                .snapshot
                .profiles
                .iter()
                .flat_map(|p| p.buttons.iter().chain(&p.gshift))
                .any(|b| b[0] < 128 && b[1] == n as u8);
            if !used && !referenced && self.d()?.read_sector(n)?.iter().all(|&b| b == 255) {
                return Ok(n);
            }
        }
        bail!("No free sector for an onboard macro")
    }
    fn store_macro(&mut self, id: &str, m: &Value) -> Result<u16> {
        ensure!(
            m["mode"].as_str().unwrap_or("once") == "once",
            "Onboard macros support once mode only"
        );
        let events = input::expand(m, &self.typing)?;
        let payload = input::onboard_macro(&events)?;
        let n = self.macro_sector(id)?;
        let size = self.d()?.info.sector_size;
        ensure!(payload.len() <= size - 2, "Macro too large");
        let mut page = vec![255; size];
        page[..payload.len()].copy_from_slice(&payload);
        let page = model::sealed(page)?;
        self.before_write()?;
        self.d()?.write_sector(n, &page, false)?;
        if !self.snapshot.settings["macro_pages"].is_object() {
            self.snapshot.settings["macro_pages"] = json!({});
        }
        self.snapshot.settings["macro_pages"][id] = n.into();
        Ok(n)
    }
    fn assign_macro(&mut self, a: &Value) -> Result<()> {
        let slot = a["slot"].as_u64().context("Missing slot")? as usize;
        self.sector(slot)?;
        let layer = a["layer"].as_u64().unwrap_or(0) as usize;
        let button = a["button"].as_u64().context("Missing button")? as usize;
        ensure!(layer < 2 && button < 16, "Invalid button");
        let id = a["id"].as_str().context("Missing macro id")?;
        let m = self.snapshot.settings["macros"]
            .as_array()
            .and_then(|m| m.iter().find(|m| m["id"] == id))
            .cloned()
            .context("Unknown macro")?;
        let b = if m["target"] == "mouse" {
            let n = self.store_macro(id, &m)?;
            [0, n as u8, 0, 0]
        } else {
            [128, 0, 255, 255]
        };
        let mut p = self.snapshot.profiles[slot - 1].clone();
        if layer == 0 {
            p.buttons[button] = b;
        } else {
            p.gshift[button] = b;
        }
        self.write_profile(slot, p)?;
        if !self.snapshot.settings["app_macros"].is_object() {
            self.snapshot.settings["app_macros"] = json!({});
        }
        let k = format!("{slot}:{layer}:{button}");
        if m["target"] == "mouse" {
            self.snapshot.settings["app_macros"]
                .as_object_mut()
                .unwrap()
                .remove(&k);
        } else {
            self.snapshot.settings["app_macros"][k] = id.into();
        }
        config::save(&self.snapshot.settings)?;
        self.sync_spy()?;
        Ok(())
    }
    fn command(&mut self, cmd: &str, a: Value) -> Result<Value> {
        match cmd {
            "ping" => {
                Ok(json!({"version":ipc::VERSION,"pid":std::process::id(),"implementation":"Rust"}))
            }
            "snapshot" => {
                if a["watch_context"].as_bool().unwrap_or(false) {
                    self.context_until = Some(Instant::now() + Duration::from_secs(2));
                    self.update_watcher();
                }
                Ok(serde_json::to_value(&self.snapshot)?)
            }
            "settings" => {
                self.settings(a)?;
                Ok(json!("Settings saved"))
            }
            "activate" => {
                self.activate(a["slot"].as_u64().context("Missing slot")? as usize)?;
                Ok(json!("Profile activated"))
            }
            "write_profile" => {
                let slot = a["slot"].as_u64().context("Missing slot")? as usize;
                let p: Profile = serde_json::from_value(a["profile"].clone())?;
                if let Some(index) = a["dpi_index"].as_u64() {
                    ensure!(
                        index < 5 && p.dpi[index as usize] > 0 && p.dpi[index as usize] < 65535,
                        "Invalid active DPI level"
                    );
                }
                self.write_profile(slot, p)?;
                if slot == self.snapshot.slot
                    && let Some(index) = a["dpi_index"].as_u64()
                {
                    self.d()?.dpi_index(index as u8)?;
                    self.snapshot.dpi_index = index as usize;
                }
                Ok(json!("Profile saved"))
            }
            "preview_dpi" => {
                let dpi = u16::try_from(a["dpi"].as_u64().context("Missing DPI")?)?;
                self.d()?.preview_dpi(dpi)?;
                Ok(Value::Null)
            }
            "dpi_index" => {
                let index = u8::try_from(a["index"].as_u64().context("Missing index")?)?;
                self.d()?.dpi_index(index)?;
                self.snapshot.dpi_index = index as usize;
                Ok(Value::Null)
            }
            "preview_led" => {
                let zone = a["zone"].as_u64().context("Missing zone")? as usize;
                let led: Led = serde_json::from_value(a["led"].clone())?;
                self.d()?.led(zone, &led)?;
                Ok(Value::Null)
            }
            "backup" => Ok(json!(self.backup(a["path"].as_str())?)),
            "export_profile" => {
                let slot = a["slot"].as_u64().context("Missing slot")? as usize;
                self.sector(slot)?;
                let p = &self.snapshot.profiles[slot - 1];
                let value = json!({"format":"g502-hub-profile","version":1,"device":"046d:c08b","app":"G502 Hub","summary":{"name":p.name,"dpi":p.dpi},"sector":model::hex(&p.encode()?)});
                let path = a["path"].as_str().context("Missing path")?;
                fs::write(path, serde_json::to_vec_pretty(&value)?)?;
                Ok(json!(path))
            }
            "import_profile" => {
                let slot = a["slot"].as_u64().context("Missing slot")? as usize;
                let path = a["path"].as_str().context("Missing path")?;
                let value: Value = serde_json::from_slice(&fs::read(path)?)?;
                ensure!(
                    value["format"] == "g502-hub-profile"
                        && value["device"] == "046d:c08b"
                        && value["version"].as_u64().unwrap_or(999) <= 1,
                    "Incompatible profile file"
                );
                let bytes =
                    model::unhex(value["sector"].as_str().context("Missing profile data")?)?;
                ensure!(model::crc_ok(&bytes), "Profile checksum mismatch");
                let mut p = Profile::decode(&bytes)?;
                for b in p.buttons.iter_mut().chain(&mut p.gshift) {
                    if b[0] < 128 {
                        *b = [128, 0, 255, 255];
                    }
                }
                self.write_profile(slot, p)?;
                self.clear_app_assignments(slot)?;
                Ok(json!("Profile imported; external macros disabled"))
            }
            "restore" => {
                let value: Value = serde_json::from_slice(&fs::read(
                    a["path"].as_str().context("Missing path")?,
                )?)?;
                let size = self.d()?.info.sector_size;
                ensure!(
                    value["sector_size"].as_u64() == Some(size as u64),
                    "Backup sector size mismatch"
                );
                let sectors = value["sectors"].as_object().context("Invalid backup")?;
                let mut pages = vec![];
                let count = self.d()?.info.sector_count as u16;
                for (k, v) in sectors {
                    let n = u16::from_str_radix(k.trim_start_matches("0x"), 16)?;
                    if n >= count {
                        continue;
                    }
                    let bytes = model::unhex(v.as_str().context("Invalid sector")?)?;
                    ensure!(
                        bytes.len() == size
                            && (model::crc_ok(&bytes) || bytes.iter().all(|&b| b == 255)),
                        "Invalid backup sector {n}"
                    );
                    pages.push((n, bytes));
                }
                pages.sort_by_key(|(n, _)| if *n == 0 { u16::MAX } else { *n });
                self.backup(None)?;
                for (n, bytes) in pages {
                    if self.d()?.read_sector(n)? != bytes {
                        self.d()?.write_sector(n, &bytes, true)?;
                    }
                }
                if let Some(n) = value["current_profile"].as_u64() {
                    self.d()?.activate(u16::try_from(n)?)?;
                }
                self.connect()?;
                Ok(json!("Backup restored and verified"))
            }
            "copy_profile" => {
                let from = a["from"].as_u64().context("Missing source")? as usize;
                let to = a["to"].as_u64().context("Missing destination")? as usize;
                self.sector(from)?;
                self.sector(to)?;
                let p = self.snapshot.profiles[from - 1].clone();
                self.write_profile(to, p)?;
                self.clear_app_assignments(to)?;
                Ok(json!("Profile copied"))
            }
            "reset_profile" => {
                let slot = a["slot"].as_u64().context("Missing slot")? as usize;
                self.sector(slot)?;
                let raw = self.d()?.read_sector(256)?;
                let dir = Directory::decode(&raw)?.context("No factory profiles")?;
                let sector = dir.entries[(slot - 1) % dir.entries.len()].sector;
                let p = self.d()?.profile(sector)?;
                self.write_profile(slot, p)?;
                self.clear_app_assignments(slot)?;
                Ok(json!("Factory profile restored"))
            }
            "profile_enabled" => {
                let slot = a["slot"].as_u64().context("Missing slot")? as usize;
                self.sector(slot)?;
                let mut dir = self.snapshot.directory.clone().context("No directory")?;
                dir.entries[slot - 1].enabled =
                    a["enabled"].as_bool().context("Missing enabled value")?;
                let bytes = dir.encode()?;
                self.before_write()?;
                let current = self.d()?.current()?.0;
                self.d()?.write_sector(0, &bytes, false)?;
                if dir.entries.iter().any(|e| e.enabled && e.sector == current) {
                    self.d()?.activate(current)?;
                }
                self.snapshot.directory = Some(dir);
                self.poll()?;
                Ok(json!("Profile cycle saved"))
            }
            "assign_macro" => {
                self.assign_macro(&a)?;
                Ok(json!("Macro assigned"))
            }
            "save_macro" => {
                let id = a["id"].as_str().context("Macro id missing")?.to_owned();
                crate::parity::macro_size(&a, &self.typing)?;
                let old_page = self.snapshot.settings["macro_pages"][&id].as_u64();
                let to_mouse = a["target"] == "mouse";
                if !to_mouse && let Some(old_page) = old_page {
                    ensure!(
                        macro_page_allowed(&self.snapshot, old_page),
                        "Protected profile sector cannot be erased as a macro"
                    );
                }
                let app_used = self.snapshot.settings["app_macros"]
                    .as_object()
                    .is_some_and(|m| m.values().any(|v| v == &id));
                let page = if to_mouse && (old_page.is_some() || app_used) {
                    Some(self.store_macro(&id, &a)?)
                } else {
                    None
                };
                let mut added = Vec::new();
                for slot in 1..=self.snapshot.profiles.len() {
                    let mut p = self.snapshot.profiles[slot - 1].clone();
                    let mut changed = false;
                    for (layer, bindings) in [&mut p.buttons, &mut p.gshift].into_iter().enumerate()
                    {
                        for (button, binding) in bindings.iter_mut().enumerate() {
                            let key = format!("{slot}:{layer}:{button}");
                            if let Some(page) = page {
                                if self.snapshot.settings["app_macros"][&key] == id {
                                    *binding = [0, page as u8, 0, 0];
                                    changed = true;
                                }
                            } else if !to_mouse
                                && old_page.is_some_and(|page| {
                                    binding[0] < 128 && binding[1] as u64 == page
                                })
                            {
                                *binding = [128, 0, 255, 255];
                                added.push(key);
                                changed = true;
                            }
                        }
                    }
                    if changed {
                        self.write_profile(slot, p)?;
                    }
                }
                let mut settings = self.snapshot.settings.clone();
                if !settings["app_macros"].is_object() {
                    settings["app_macros"] = json!({});
                }
                if to_mouse {
                    settings["app_macros"]
                        .as_object_mut()
                        .unwrap()
                        .retain(|_, v| v != &id);
                } else {
                    for key in added {
                        settings["app_macros"][key] = id.clone().into();
                    }
                    if let Some(map) = settings["macro_pages"].as_object_mut() {
                        map.remove(&id);
                    }
                }
                if !settings["macros"].is_array() {
                    settings["macros"] = json!([]);
                }
                let list = settings["macros"].as_array_mut().unwrap();
                if let Some(i) = list.iter().position(|m| m["id"] == id) {
                    list[i] = a;
                } else {
                    list.push(a);
                }
                self.settings(settings)?;
                if !to_mouse
                    && let Some(old_page) = old_page
                    && self.device.is_some()
                {
                    let size = self.d()?.info.sector_size;
                    self.d()?
                        .write_sector(u16::try_from(old_page)?, &vec![255; size], true)?;
                }
                Ok(json!("Macro saved"))
            }
            "delete_macro" => {
                let id = a["id"].as_str().context("Missing macro id")?;
                let page = self.snapshot.settings["macro_pages"][id].as_u64();
                if let Some(page) = page {
                    ensure!(
                        macro_page_allowed(&self.snapshot, page),
                        "Protected profile sector cannot be erased as a macro"
                    );
                    for slot in 1..=self.snapshot.profiles.len() {
                        let mut p = self.snapshot.profiles[slot - 1].clone();
                        let mut change = false;
                        for b in p.buttons.iter_mut().chain(&mut p.gshift) {
                            if b[0] < 128 && b[1] as u64 == page {
                                *b = [128, 0, 255, 255];
                                change = true;
                            }
                        }
                        if change {
                            self.write_profile(slot, p)?;
                        }
                    }
                }
                let mut settings = self.snapshot.settings.clone();
                if let Some(list) = settings["macros"].as_array_mut() {
                    list.retain(|m| m["id"] != id);
                }
                if let Some(map) = settings["app_macros"].as_object_mut() {
                    map.retain(|_, v| v != id);
                }
                if let Some(map) = settings["macro_pages"].as_object_mut() {
                    map.remove(id);
                }
                self.settings(settings)?;
                if let Some(page) = page
                    && self.device.is_some()
                {
                    let size = self.d()?.info.sector_size;
                    self.d()?
                        .write_sector(u16::try_from(page)?, &vec![255; size], true)?;
                }
                Ok(json!("Macro deleted"))
            }
            "bridge_prefixes" => Ok(json!(crate::bridge::prefixes())),
            "bridge_install" => {
                crate::bridge::install(
                    std::path::Path::new(a["path"].as_str().context("Missing prefix")?),
                    &crate::bridge::bin_dir(),
                )?;
                Ok(json!("LIGHTSYNC integration installed"))
            }
            "bridge_remove" => {
                crate::bridge::uninstall(std::path::Path::new(
                    a["path"].as_str().context("Missing prefix")?,
                ))?;
                Ok(json!("LIGHTSYNC integration removed"))
            }
            "gsi_status" => Ok(context::gsi_status()),
            "games" => Ok(serde_json::to_value(context::installed_games())?),
            "install_gsi" => Ok(json!(context::install_gsi(false)?)),
            "remove_gsi" => Ok(json!(context::install_gsi(true)?)),
            "refresh" => {
                self.player.stop_all();
                self.spy = None;
                self.connect()?;
                Ok(Value::Null)
            }
            "choose_screen" => {
                let mut s = self.snapshot.settings.clone();
                s["screen_token"] = Value::Null;
                s["live"]["effect"] = json!("screen");
                s["live"]["on"] = json!(true);
                self.settings(s)?;
                self.live.choose_screen();
                Ok(Value::Null)
            }
            "lighting" => {
                let mut s = self.snapshot.settings.clone();
                if !s["live"].is_object() {
                    s["live"] = json!({});
                }
                s["live"]["on"] = a["on"].clone();
                if let Some(effect) = a["effect"].as_str() {
                    ensure!(
                        ["screen", "audio", "wave", "dpi"].contains(&effect),
                        "Unknown effect"
                    );
                    s["live"]["effect"] = effect.into();
                }
                self.settings(s)?;
                Ok(Value::Null)
            }
            "check_update" => {
                let info = crate::updater::check_for_updates()?;
                self.snapshot.update = info.clone();
                self.next_update_check = Instant::now() + Duration::from_secs(6 * 3600);
                Ok(serde_json::to_value(info)?)
            }
            other => bail!("Unknown command {other}"),
        }
    }
    fn tick_updater(&mut self) {
        if let Some(rx) = &self.update_rx {
            match rx.try_recv() {
                Ok(BgUpdate::Found(info)) => {
                    self.snapshot.update = info;
                    self.update_rx = None;
                }
                Ok(BgUpdate::AutoInstalled(version, gui_bin)) => {
                    self.snapshot.update = None;
                    self.pending_handoff = Some((version, gui_bin));
                    self.update_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.update_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some((version, _)) = &self.pending_handoff
            && self.mask == 0
        {
            if self.snapshot.settings["osd"].as_bool().unwrap_or(true) {
                queue_osd(format!("G502 Hub updated to v{version}"));
            }
            STOP.store(true, Ordering::Relaxed);
            return;
        }
        if self.update_rx.is_none()
            && self.pending_handoff.is_none()
            && Instant::now() >= self.next_update_check
            && std::env::var_os("G502_RUST_HIDRAW_ROOT").is_none()
        {
            self.next_update_check = Instant::now() + Duration::from_secs(6 * 3600);
            let auto = self.snapshot.settings["auto_update"]
                .as_bool()
                .unwrap_or(true)
                && crate::updater::can_auto_update_current_exe();
            let (tx, rx) = std::sync::mpsc::channel();
            self.update_rx = Some(rx);
            std::thread::spawn(move || {
                if let Ok(found) = crate::updater::check_for_updates() {
                    if let Some(info) = &found
                        && auto
                        && let Ok(gui_bin) =
                            crate::updater::download_and_install_update(info, |_, _, _| {})
                    {
                        let _ = tx.send(BgUpdate::AutoInstalled(
                            info.latest_version.clone(),
                            gui_bin,
                        ));
                        return;
                    }
                    let _ = tx.send(BgUpdate::Found(found));
                }
            });
        }
    }
    fn stop(&mut self) {
        self.player.stop_all();
        let _ = self.restore_lighting();
        if let Some(d) = &mut self.device
            && self.spy.is_some()
        {
            let _ = d.spy_stop();
        }
    }
}
static OSD_PENDING: Mutex<Option<String>> = Mutex::new(None);
static OSD_RUNNING: AtomicBool = AtomicBool::new(false);
static OSD_NOTIFICATION_ID: AtomicU32 = AtomicU32::new(0);
fn queue_osd(text: String) {
    *OSD_PENDING.lock().unwrap() = Some(text);
    if OSD_RUNNING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        std::thread::spawn(|| {
            while let Some(msg) = OSD_PENDING.lock().unwrap().take() {
                let _ = show_osd(&msg);
            }
            OSD_RUNNING.store(false, Ordering::SeqCst);
        });
    }
}
fn show_osd(text: &str) -> Result<()> {
    let c = zbus::blocking::connection::Builder::session()?
        .method_timeout(Duration::from_secs(2))
        .build()?;
    if let Ok(p) = zbus::blocking::Proxy::new(
        &c,
        "org.kde.plasmashell",
        "/org/kde/osdService",
        "org.kde.osdService",
    ) && p
        .call::<_, _, ()>("showText", &("input-mouse", text))
        .is_ok()
    {
        return Ok(());
    }
    let p = zbus::blocking::Proxy::new(
        &c,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    )?;
    let hints = HashMap::from([
        ("transient", zbus::zvariant::Value::from(true)),
        (
            "x-canonical-private-synchronous",
            zbus::zvariant::Value::from("g502-hub"),
        ),
    ]);
    let prev = OSD_NOTIFICATION_ID.load(Ordering::Relaxed);
    let id: u32 = p.call(
        "Notify",
        &(
            "G502 Hub",
            prev,
            "input-mouse",
            text,
            "",
            Vec::<&str>::new(),
            hints,
            1500i32,
        ),
    )?;
    OSD_NOTIFICATION_ID.store(id, Ordering::Relaxed);
    Ok(())
}
pub fn prune_auto_backups(dir: &std::path::Path, keep: usize) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut auto: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("onboard-") && n.ends_with(".json"))
        })
        .collect();
    if auto.len() <= keep {
        return;
    }
    auto.sort();
    let remove_count = auto.len() - keep;
    for old in auto.into_iter().take(remove_count) {
        let _ = fs::remove_file(old);
    }
}
#[derive(Default)]
struct SleepClock {
    previous: Option<(Duration, Duration)>,
}
impl SleepClock {
    fn observe(&mut self, boot: Duration, monotonic: Duration) -> bool {
        let previous = self.previous.replace((boot, monotonic));
        previous.is_some_and(|(old_boot, old_mono)| {
            boot.saturating_sub(old_boot)
                .saturating_sub(monotonic.saturating_sub(old_mono))
                >= Duration::from_secs(1)
        })
    }
    fn resumed(&mut self) -> bool {
        fn read(clock: libc::clockid_t) -> Option<Duration> {
            let mut time = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            if unsafe { libc::clock_gettime(clock, &mut time) } != 0 {
                return None;
            }
            Some(Duration::new(time.tv_sec as u64, time.tv_nsec as u32))
        }
        match (read(libc::CLOCK_BOOTTIME), read(libc::CLOCK_MONOTONIC)) {
            (Some(boot), Some(monotonic)) => self.observe(boot, monotonic),
            _ => false,
        }
    }
}
static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn signal_handler(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}
pub fn run() -> Result<()> {
    let _lock = config::lock("service.lock")?;
    let socket = ipc::socket()?;
    if socket.exists() {
        fs::remove_file(&socket)?;
    }
    let mut e = Engine::new()?;
    if let Err(error) = e.connect() {
        e.lost(error);
    }
    let listener = UnixListener::bind(&socket)?;
    listener.set_nonblocking(true)?;
    unsafe {
        libc::signal(libc::SIGTERM, signal_handler as *const () as usize);
        libc::signal(libc::SIGINT, signal_handler as *const () as usize);
    }
    let mut last_poll = Instant::now();
    let mut last_rule = Instant::now();
    let mut last_effect = Instant::now();
    let mut reconnect = Instant::now();
    let mut sleep_clock = SleepClock::default();
    while !STOP.load(Ordering::Relaxed) {
        // CLOCK_BOOTTIME includes suspend; Instant/CLOCK_MONOTONIC does not.
        // Reopen both physical and virtual devices after wake, even when the
        // hidraw descriptor survived and no I/O error signalled the USB reset.
        if sleep_clock.resumed() {
            e.lost(anyhow::anyhow!("Reconnecting after sleep"));
            e.player = Player::default();
            if let Err(error) = e.connect() {
                e.lost(error);
            }
            reconnect = Instant::now();
        }
        if e.device.is_none() && reconnect.elapsed() > Duration::from_secs(2) {
            reconnect = Instant::now();
            if let Err(error) = e.connect() {
                e.lost(error);
            }
        }
        let fd = e
            .device
            .as_ref()
            .map(|d| d.hid.file.as_raw_fd())
            .unwrap_or(-1);
        let mut fds = [
            libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let tick = if e.live.kind.is_empty()
            && e.snapshot.settings["live"]["on"] != true
            && e.snapshot
                .active_rule
                .as_ref()
                .is_none_or(|r| r["lighting"].is_null())
        {
            Duration::from_millis(250)
        } else {
            Duration::from_millis(33)
        };
        let next = if tick.as_millis() == 33 {
            last_effect + tick
        } else {
            last_poll + tick
        };
        let timeout = next
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(250) as i32;
        unsafe {
            libc::poll(fds.as_mut_ptr(), 2, timeout);
        }
        if fds[1].revents != 0
            && let Err(error) = e.events()
        {
            e.lost(error);
        }
        if fds[0].revents & libc::POLLIN != 0 {
            while let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                let result = (|| -> Result<Value> {
                    let mut line = String::new();
                    BufReader::new(&stream)
                        .take(ipc::MAX as u64)
                        .read_line(&mut line)?;
                    ensure!(line.ends_with('\n'), "Incomplete request");
                    let request: ipc::Request = serde_json::from_str(&line)?;
                    ensure!(request.version == ipc::VERSION, "Incompatible protocol");
                    if request.command == "stop" {
                        STOP.store(true, Ordering::Relaxed);
                        Ok(json!("Service stopped"))
                    } else {
                        e.command(&request.command, request.args)
                    }
                })();
                let response = match result {
                    Ok(value) => json!({"result":value}),
                    Err(error) => {
                        e.snapshot.status = error.to_string();
                        json!({"error":error.to_string()})
                    }
                };
                if let Ok(mut bytes) = serde_json::to_vec(&response) {
                    bytes.push(b'\n');
                    let _ = stream.write_all(&bytes);
                }
            }
        }
        if last_poll.elapsed() >= Duration::from_millis(250) {
            last_poll = Instant::now();
            if let Err(error) = e.poll() {
                e.lost(error);
            }
            if let Err(error) = e.events() {
                e.lost(error);
            }
            e.tick_updater();
        }
        if last_rule.elapsed() >= Duration::from_millis(300) {
            last_rule = Instant::now();
            if let Err(error) = e.rules() {
                e.snapshot.error = Some(error.to_string());
            }
        }
        if last_effect.elapsed() >= tick {
            last_effect = Instant::now();
            if let Err(error) = e.effects() {
                e.snapshot.error = Some(error.to_string());
            }
        }
    }
    let handoff = e.pending_handoff.take();
    e.stop();
    let _ = fs::remove_file(socket);
    drop(_lock);
    if let Some((_, gui_bin)) = handoff {
        let daemon_bin = gui_bin.with_file_name("g502-hubd");
        if daemon_bin.is_file() {
            use std::os::unix::process::CommandExt;
            let mut cmd = std::process::Command::new(daemon_bin);
            cmd.stdin(std::process::Stdio::null());
            if let Ok(rt) = config::runtime()
                && let Ok(log) = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(rt.join("service.log"))
                && let Ok(err) = log.try_clone()
            {
                cmd.stdout(log).stderr(err);
            } else {
                cmd.stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null());
            }
            unsafe {
                cmd.pre_exec(|| {
                    let _ = libc::setsid();
                    Ok(())
                });
            }
            let _ = cmd.spawn();
        }
    }
    Ok(())
}

fn macro_page_allowed(snapshot: &Snapshot, page: u64) -> bool {
    page > 0
        && snapshot
            .info
            .as_ref()
            .is_some_and(|i| page < i.sector_count as u64)
        && snapshot
            .directory
            .as_ref()
            .is_some_and(|d| !d.entries.iter().any(|e| e.sector as u64 == page))
}
#[cfg(test)]
mod memory_guard_tests {
    use super::*;
    #[test]
    fn macros_cannot_overwrite_profiles_or_factory_memory() {
        let snapshot = Snapshot {
            info: Some(Info {
                profile_format: 2,
                macro_format: 1,
                profile_count: 5,
                rom_profile_count: 5,
                button_count: 11,
                sector_count: 10,
                sector_size: 255,
            }),
            directory: Some(Directory {
                entries: (1..=5)
                    .map(|sector| crate::model::Entry {
                        sector,
                        enabled: true,
                        flag: 0,
                    })
                    .collect(),
                terminator: [255, 255, 0, 0],
                raw: vec![],
            }),
            ..Default::default()
        };
        for page in [0, 1, 2, 3, 4, 5, 10, 256] {
            assert!(!macro_page_allowed(&snapshot, page));
        }
        for page in [6, 7, 8, 9] {
            assert!(macro_page_allowed(&snapshot, page));
        }
    }
}

#[cfg(test)]
mod resume_tests {
    use super::*;
    #[test]
    fn suspend_is_detected_once_without_confusing_busy_service_or_wall_clock() {
        let mut clock = SleepClock::default();
        let d = Duration::from_secs;
        assert!(!clock.observe(d(100), d(60)));
        assert!(!clock.observe(d(130), d(90))); // 30 seconds of ordinary work
        assert!(clock.observe(d(3730), d(90))); // one hour asleep
        assert!(!clock.observe(d(3731), d(91)));
        assert!(clock.observe(d(3742), d(92))); // another suspend
        assert!(!clock.observe(d(3743), d(93)));
    }
    #[test]
    fn auto_backup_pruning_keeps_newest_and_preserves_custom_files() {
        let dir = std::env::temp_dir().join(format!(
            "g502-prune-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        for n in 1000..1008 {
            fs::write(dir.join(format!("onboard-{n}.json")), b"{}").unwrap();
        }
        fs::write(dir.join("custom-backup.json"), b"{}").unwrap();
        prune_auto_backups(&dir, 5);
        let mut names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "custom-backup.json",
                "onboard-1003.json",
                "onboard-1004.json",
                "onboard-1005.json",
                "onboard-1006.json",
                "onboard-1007.json",
            ]
        );
        let _ = fs::remove_dir_all(dir);
    }
    #[test]
    fn switching_macro_from_mouse_to_app_reclaims_onboard_sector_and_disabled_rule_slot_keeps_lighting()
     {
        use std::io::{Read, Write};
        use std::os::fd::{FromRawFd, IntoRawFd};
        use std::os::unix::net::UnixStream;
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/onboard-c08b.json")).unwrap();
        let raw_dir = model::unhex(fixture["sectors"]["0x0000"].as_str().unwrap()).unwrap();
        let raw_p1 = model::unhex(fixture["sectors"]["0x0001"].as_str().unwrap()).unwrap();
        let mut sectors: HashMap<u16, Vec<u8>> = HashMap::new();
        sectors.insert(0, raw_dir.clone());
        for s in 1..=5u16 {
            let key = format!("0x{s:04X}");
            sectors.insert(
                s,
                model::unhex(fixture["sectors"][&key].as_str().unwrap()).unwrap(),
            );
        }
        for s in 6..10u16 {
            sectors.insert(s, vec![255; 255]);
        }
        let shared_sectors = std::sync::Arc::new(Mutex::new(sectors));
        let server_sectors = shared_sectors.clone();
        let (a, mut b) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let mut active_sector = 1u16;
            let mut active_dpi = 0u8;
            let mut write_target = 0u16;
            let mut pending = Vec::new();
            loop {
                let mut id = [0u8];
                if b.read_exact(&mut id).is_err() {
                    break;
                }
                let size = if id[0] == 0x10 { 7 } else { 20 };
                let mut pkt = vec![0u8; size];
                pkt[0] = id[0];
                if b.read_exact(&mut pkt[1..]).is_err() {
                    break;
                }
                let feat = pkt[2];
                let fn_id = pkt[3] >> 4;
                let mut payload = vec![0u8; 16];
                match (feat, fn_id) {
                    (1, 3) => active_sector = u16::from_be_bytes([pkt[4], pkt[5]]),
                    (1, 4) => payload[..2].copy_from_slice(&active_sector.to_be_bytes()),
                    (1, 11) => payload[0] = active_dpi,
                    (1, 12) => active_dpi = pkt[4],
                    (1, 5) => {
                        let sec = u16::from_be_bytes([pkt[4], pkt[5]]);
                        let off = u16::from_be_bytes([pkt[6], pkt[7]]) as usize;
                        let map = server_sectors.lock().unwrap();
                        let data = &map[&sec];
                        payload.copy_from_slice(&data[off..off + 16]);
                    }
                    (1, 6) => {
                        write_target = u16::from_be_bytes([pkt[4], pkt[5]]);
                        pending.clear();
                    }
                    (1, 7) => pending.extend_from_slice(&pkt[4..]),
                    (1, 8) => {
                        pending.truncate(255);
                        server_sectors
                            .lock()
                            .unwrap()
                            .insert(write_target, pending.clone());
                    }
                    (2, _) => {}
                    _ => {}
                }
                let mut reply = vec![0x11, 255, pkt[2], pkt[3]];
                reply.extend_from_slice(&payload);
                let _ = b.write_all(&reply);
            }
        });
        let mut hid = crate::device::Hid::from_file(
            unsafe { std::fs::File::from_raw_fd(a.into_raw_fd()) },
            std::path::PathBuf::from("mock"),
        );
        hid.index_for_test(0x8100, 1);
        hid.index_for_test(0x8110, 2);
        let lock_path = std::env::temp_dir().join(format!("g502-svc-lock-{}", std::process::id()));
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&lock_path)
            .unwrap();
        let _ = fs::remove_file(&lock_path);
        let info = Info {
            profile_format: 2,
            macro_format: 1,
            profile_count: 5,
            rom_profile_count: 5,
            button_count: 11,
            sector_count: 10,
            sector_size: 255,
        };
        let mut dir = Directory::decode(&raw_dir).unwrap().unwrap();
        dir.entries[2].enabled = false;
        let profiles: Vec<_> = (1..=5).map(|_| Profile::decode(&raw_p1).unwrap()).collect();
        let cfg_root = std::env::temp_dir().join(format!(
            "g502-svc-cfg-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &cfg_root);
            std::env::set_var("XDG_DATA_HOME", &cfg_root);
        }
        let mut engine = Engine {
            device: Some(Device::for_test(hid, info.clone(), lock)),
            snapshot: Snapshot {
                connected: true,
                info: Some(info),
                directory: Some(dir),
                profiles,
                slot: 1,
                settings: json!({"macros":[],"app_macros":{},"macro_pages":{},"rules":[]}),
                ..Default::default()
            },
            watcher: None,
            context_until: None,
            live: Live::default(),
            player: Player::default(),
            typing: input::typing_table("us").unwrap(),
            spy: None,
            last_spy_start: Instant::now(),
            mask: 0,
            backup_done: true,
            slot_before: None,
            rule_id: None,
            blocked_rule: None,
            last_colors: None,
            last_send: Instant::now(),
            update_rx: None,
            next_update_check: Instant::now() + Duration::from_secs(3600),
            pending_handoff: None,
        };
        let m_mouse = json!({
            "id": "m_switch",
            "name": "Click",
            "mode": "once",
            "target": "mouse",
            "gap_ms": 0,
            "steps": [{"kind": "click", "args": {"button": "left"}}]
        });
        engine.command("save_macro", m_mouse).unwrap();
        engine
            .command(
                "assign_macro",
                json!({"id": "m_switch", "slot": 1, "layer": 0, "button": 5}),
            )
            .unwrap();
        assert_eq!(engine.snapshot.settings["macro_pages"]["m_switch"], 6);
        assert!(shared_sectors.lock().unwrap()[&6].iter().any(|&x| x != 255));

        // Switch the macro from mouse storage to app execution: sector 6 must be reclaimed!
        let m_app = json!({
            "id": "m_switch",
            "name": "Click",
            "mode": "while-held",
            "target": "app",
            "gap_ms": 0,
            "steps": [{"kind": "click", "args": {"button": "left"}}]
        });
        engine.command("save_macro", m_app).unwrap();
        assert!(engine.snapshot.settings["macro_pages"]["m_switch"].is_null());
        assert_eq!(engine.snapshot.settings["app_macros"]["1:0:5"], "m_switch");
        assert!(shared_sectors.lock().unwrap()[&6].iter().all(|&x| x == 255));

        // Rule targeting disabled slot 3 should record an error without aborting active_rule lighting.
        engine.snapshot.settings["rules"] = json!([{
            "id": "r1",
            "name": "Disabled slot rule",
            "trigger": "app",
            "value": "firefox",
            "lighting": "color",
            "color": "#ff0000",
            "slot": 3,
            "enabled": true
        }]);
        engine.snapshot.context.app = "firefox".into();
        engine.evaluate_rules().unwrap();
        assert_eq!(
            engine.snapshot.active_rule.as_ref().unwrap()["id"],
            json!("r1")
        );
        assert_eq!(engine.rule_id.as_deref(), Some("r1"));
        assert!(engine.snapshot.error.is_some());

        // Rule targeting enabled slot 2 switches slot and restores slot 1 when context changes.
        engine.snapshot.settings["rules"][0]["slot"] = json!(2);
        engine.rule_id = None;
        engine.slot_before = None;
        engine.evaluate_rules().unwrap();
        assert_eq!(engine.snapshot.slot, 2);
        engine.snapshot.context.app = "terminal".into();
        engine.evaluate_rules().unwrap();
        assert_eq!(engine.snapshot.slot, 1);
        assert!(engine.snapshot.active_rule.is_none());

        drop(engine);
        server.join().unwrap();
        let _ = fs::remove_dir_all(cfg_root);
    }
}
