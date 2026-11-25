//! Écrans branchés et estimation de leur consommation. Un écran n'a pas de
//! capteur de watts lisible depuis le PC : on part de ce qu'il dit de lui
//! (EDID : modèle, taille), de son mode (fréquence, HDR, lus chez KDE ou X11),
//! de sa luminosité (DDC/CI quand l'écran répond juste, sinon réglée à la main)
//! et, pour un OLED, de la clarté de l'image affichée.

use std::path::Path;
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tech { Oled, Lcd }

/// Fiche du fabricant (étiquette énergie) : W typique, W maximum, W en veille.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spec { pub typical: f32, pub max: f32, pub standby: f32 }

/// Écrans dont on connaît la fiche (nom EDID, technologie, fiche).
const KNOWN: [(&str, Tech, Spec); 1] = [
    // Samsung Odyssey OLED G5 G50SF (27", QD-OLED, 180 Hz) : étiquette UE 28 W, 78 W max, 0,5 W en veille.
    ("Odyssey G50SF", Tech::Oled, Spec { typical: 28.0, max: 78.0, standby: 0.5 }),
];

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Monitor {
    /// Connecteur côté noyau (« DP-1 »).
    pub connector: String,
    pub vendor: String,
    pub name: String,
    pub serial: String,
    /// Taille de la dalle en cm (largeur, hauteur).
    pub size_cm: (f32, f32),
    pub enabled: bool,
    pub res: (u32, u32),
    /// Fréquence actuelle (Hz) et fréquences possibles à cette définition.
    pub hz: f32,
    pub rates: Vec<f32>,
    pub hdr: bool,
    pub vrr: bool,
    /// Luminosité lue par DDC/CI (0–1), si l'écran donne une valeur cohérente.
    pub ddc_brightness: Option<f32>,
}

impl Monitor {
    pub fn key(&self) -> String { if self.serial.is_empty() { format!("{} {}", self.vendor, self.name) } else { self.serial.clone() } }
    pub fn known(&self) -> Option<(Tech, Spec)> { KNOWN.iter().find(|(n, _, _)| self.name.contains(n)).map(|(_, t, s)| (*t, *s)) }
    pub fn tech(&self) -> Tech {
        match self.known() { Some((t, _)) => t, None if self.name.to_uppercase().contains("OLED") => Tech::Oled, None => Tech::Lcd }
    }
    pub fn diagonal_in(&self) -> f32 { (self.size_cm.0.powi(2) + self.size_cm.1.powi(2)).sqrt() / 2.54 }
    /// Surface de la dalle en dm² (un 27" 16:9 ≈ 20 dm²) ; 20 si l'EDID ne la donne pas.
    fn area_dm2(&self) -> f32 { let a = self.size_cm.0 * self.size_cm.1 / 100.0; if a > 1.0 { a } else { 20.0 } }
    pub fn title(&self) -> String {
        let brand = match self.vendor.as_str() { "SAM" | "SEC" => "Samsung", "DEL" => "Dell", "GSM" => "LG", "AUS" => "ASUS", "ACR" => "Acer", "AOC" => "AOC", "BNQ" => "BenQ", "MSI" | "MSG" => "MSI", "PHL" => "Philips", "GBT" => "Gigabyte", "LEN" => "Lenovo", "HWP" | "HPN" => "HP", "IVM" => "iiyama", "VSC" => "ViewSonic", v => v };
        let name = if self.name.is_empty() { "Écran".to_string() } else { self.name.clone() };
        if brand.is_empty() || name.contains(brand) { name } else { format!("{brand} {name}") }
    }
}

/// Clarté moyenne de l'image (0 = noir, 1 = blanc) : sur un OLED, c'est elle qui fait les watts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Content { Auto, Dark, Mixed, Light }

impl Content {
    pub fn apl(self) -> f32 { match self { Content::Dark => 0.12, Content::Mixed => 0.3, Content::Light => 0.65, Content::Auto => 0.3 } }
    pub fn id(self) -> &'static str { match self { Content::Auto => "auto", Content::Dark => "dark", Content::Mixed => "mixed", Content::Light => "light" } }
    pub fn from_id(s: &str) -> Self { match s { "dark" => Content::Dark, "mixed" => Content::Mixed, "light" => Content::Light, _ => Content::Auto } }
    pub fn label(self) -> &'static str { match self { Content::Auto => "Auto", Content::Dark => "Sombre", Content::Mixed => "Jeu, film", Content::Light => "Clair" } }
}

/// Consommation estimée à la prise (W). `brightness` = luminosité du menu de
/// l'écran (0–1), `apl` = clarté moyenne de l'image (0–1).
pub fn estimate(tech: Tech, spec: Option<Spec>, area_dm2: f32, hz: f32, brightness: f32, apl: f32, hdr: bool) -> f32 {
    let hz = if hz > 0.0 { hz } else { 60.0 };
    // Électronique (carte de l'écran, pilotes de la dalle) : un peu plus quand la fréquence monte.
    let fast = ((hz - 60.0) / 120.0).clamp(0.0, 1.5);
    let b = brightness.clamp(0.0, 1.0);
    match tech {
        Tech::Oled => {
            let base = 8.0 + 1.5 * fast;
            // Étiquette énergie : contenu vidéo type (≈ 35 % de clarté), luminosité d'usine.
            let typical = spec.map_or(6.0 + 1.1 * area_dm2, |s| s.typical);
            let max = spec.map_or(typical * 2.6, |s| s.max);
            let coef = (typical - 8.75).max(4.0) / 0.35f32.powf(0.8);
            // En HDR, l'écran pousse sa luminosité au maximum et éclaire plus les zones claires.
            let light = if hdr { 1.45 } else { b };
            (base + coef * light * apl.clamp(0.0, 1.0).powf(0.8)).min(max)
        }
        Tech::Lcd => {
            let base = 4.0 + 1.5 * fast;
            // Rétroéclairage : toute la dalle, quelle que soit l'image. ≈ 1 W/dm² à fond.
            let full = match spec { Some(s) => ((s.typical - 4.75) / 0.75).max(4.0), None => area_dm2 };
            let light = if hdr { 1.0 } else { b };
            let w = base + full * (0.08 + 0.92 * light);
            spec.map_or(w, |s| w.min(s.max))
        }
    }
}

impl Monitor {
    pub fn watts(&self, hz: f32, brightness: f32, apl: f32) -> f32 {
        estimate(self.tech(), self.known().map(|k| k.1), self.area_dm2(), hz, brightness, apl, self.hdr)
    }
}

// ---------------------------------------------------------------- détection

/// Nom, numéro de série, fabricant et taille (cm) lus dans l'EDID.
pub fn parse_edid(d: &[u8]) -> Option<(String, String, String, (f32, f32))> {
    if d.len() < 128 || d[..8] != [0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0] { return None; }
    let m = u16::from_be_bytes([d[8], d[9]]);
    let vendor: String = [(m >> 10) & 31, (m >> 5) & 31, m & 31].iter().map(|c| (b'A' + *c as u8 - 1) as char).collect();
    let (mut name, mut serial) = (String::new(), String::new());
    let mut size = (d[21] as f32, d[22] as f32);
    for i in (54..126).step_by(18) {
        let b = &d[i..i + 18];
        if b[0] == 0 && b[1] == 0 {
            let text = String::from_utf8_lossy(&b[5..18]).split('\n').next().unwrap_or("").trim().to_string();
            match b[3] { 0xFC => name = text, 0xFF => serial = text, _ => {} }
        } else if i == 54 {
            // Taille exacte en mm dans le premier mode détaillé.
            let (w, h) = (((b[14] as u16 & 0xF0) << 4 | b[12] as u16) as f32, ((b[14] as u16 & 0x0F) << 8 | b[13] as u16) as f32);
            if w > 50.0 && h > 50.0 && w < 3000.0 { size = (w / 10.0, h / 10.0); }
        }
    }
    Some((vendor, name, serial, size))
}

/// Écrans branchés d'après /sys/class/drm (sans leur mode).
fn drm_monitors() -> Vec<Monitor> {
    let mut list = Vec::new();
    let Ok(dir) = std::fs::read_dir("/sys/class/drm") else { return list };
    let mut paths: Vec<_> = dir.flatten().map(|e| e.path()).collect();
    paths.sort();
    for p in paths {
        let file = p.file_name().and_then(|f| f.to_str()).unwrap_or("").to_string();
        let Some(connector) = file.strip_prefix("card").and_then(|r| r.split_once('-')).map(|(_, c)| c.to_string()) else { continue };
        if std::fs::read_to_string(p.join("status")).map_or(true, |s| s.trim() != "connected") { continue; }
        let Some((vendor, name, serial, size_cm)) = std::fs::read(p.join("edid")).ok().and_then(|d| parse_edid(&d)) else { continue };
        list.push(Monitor { connector, vendor, name, serial, size_cm, enabled: true, ..Default::default() });
    }
    list
}

/// Mode des écrans d'après KDE (kscreen-doctor -j), sinon d'après xrandr.
fn apply_modes(list: &mut [Monitor]) {
    if let Some(v) = Command::new("kscreen-doctor").arg("-j").output().ok().filter(|o| o.status.success())
        .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok()) {
        for o in v["outputs"].as_array().into_iter().flatten() {
            let Some(m) = list.iter_mut().find(|m| o["name"].as_str() == Some(m.connector.as_str())) else { continue };
            m.enabled = o["enabled"].as_bool().unwrap_or(true);
            m.hdr = o["hdr"].as_bool().unwrap_or(false);
            m.vrr = o["vrrPolicy"].as_u64().map_or(false, |p| p != 0);
            let modes = o["modes"].as_array().cloned().unwrap_or_default();
            let cur = o["currentModeId"].as_str().and_then(|id| modes.iter().find(|x| x["id"].as_str() == Some(id)));
            if let Some(c) = cur {
                m.hz = c["refreshRate"].as_f64().unwrap_or(0.0) as f32;
                m.res = (c["size"]["width"].as_u64().unwrap_or(0) as u32, c["size"]["height"].as_u64().unwrap_or(0) as u32);
                m.rates = modes.iter().filter(|x| x["size"] == c["size"]).filter_map(|x| x["refreshRate"].as_f64()).map(|r| r as f32).collect();
            }
        }
    } else if let Some(text) = Command::new("xrandr").arg("--current").output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).into_owned()) {
        parse_xrandr(&text, list);
    }
    for m in list.iter_mut() {
        m.rates.sort_by(|a, b| a.total_cmp(b));
        m.rates.dedup_by(|a, b| (*a - *b).abs() < 1.0);
    }
}

pub fn parse_xrandr(text: &str, list: &mut [Monitor]) {
    let mut cur: Option<usize> = None;
    for line in text.lines() {
        if !line.starts_with(' ') {
            cur = list.iter().position(|m| line.split_whitespace().next() == Some(m.connector.as_str()));
            continue;
        }
        let Some(i) = cur else { continue };
        let mut words = line.split_whitespace();
        let Some((w, h)) = words.next().and_then(|r| r.split_once('x')) else { continue };
        let rates: Vec<(f32, bool)> = words.filter_map(|r| r.trim_end_matches(['+', '*']).parse::<f32>().ok().map(|v| (v, r.contains('*')))).collect();
        if let Some((hz, _)) = rates.iter().find(|r| r.1) {
            let m = &mut list[i];
            m.hz = *hz;
            m.res = (w.parse().unwrap_or(0), h.trim_end_matches(|c: char| !c.is_ascii_digit()).parse().unwrap_or(0));
            m.rates = rates.iter().map(|r| r.0).collect();
        }
    }
}

/// Luminosité par DDC/CI pour chaque connecteur, quand l'écran répond une valeur cohérente
/// (certains, comme les Samsung, renvoient n'importe quoi : on les règle alors à la main).
pub fn ddc_brightness() -> Vec<(String, f32)> {
    let mut out = Vec::new();
    let Some(text) = Command::new("ddcutil").args(["detect", "--brief"]).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()) else { return out };
    let mut display = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(n) = line.strip_prefix("Display ") { display = n.trim().parse::<u32>().ok(); }
        if let (Some(n), Some(c)) = (display, line.strip_prefix("DRM connector:")) {
            let connector = c.trim().split_once('-').map(|(_, r)| r.to_string()).unwrap_or_default();
            let res = Command::new("ddcutil").args(["getvcp", "10", "--brief", "--display", &n.to_string()]).output();
            if let Some(b) = res.ok().and_then(|o| parse_vcp(&String::from_utf8_lossy(&o.stdout))) { out.push((connector, b)); }
        }
    }
    out
}

/// « VCP 10 C 40 100 » → 0,4 ; rien si la valeur dépasse le maximum.
pub fn parse_vcp(s: &str) -> Option<f32> {
    let w: Vec<&str> = s.split_whitespace().collect();
    let (cur, max) = (w.get(3)?.parse::<f32>().ok()?, w.get(4)?.parse::<f32>().ok()?);
    (max > 0.0 && cur <= max).then(|| cur / max)
}

pub fn monitors() -> Vec<Monitor> {
    let mut list = drm_monitors();
    apply_modes(&mut list);
    list
}

/// Thème du bureau sombre ? (KDE : couleur de fond des fenêtres.)
pub fn dark_desktop() -> Option<bool> {
    let home = std::env::var_os("HOME")?;
    let text = std::fs::read_to_string(Path::new(&home).join(".config/kdeglobals")).ok()?;
    let mut in_window = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') { in_window = line == "[Colors:Window]"; continue; }
        if let Some(v) = line.strip_prefix("BackgroundNormal=").filter(|_| in_window) {
            let c: Vec<f32> = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
            if c.len() >= 3 { return Some(0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2] < 110.0); }
        }
    }
    None
}

// ---------------------------------------------------------------- préférences

/// Réglages par écran (clé : numéro de série) : luminosité du menu, contenu.
#[derive(Clone, Debug, Default)]
pub struct Prefs { pub map: serde_json::Map<String, serde_json::Value> }

fn prefs_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME").map(Into::into)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".config")))?;
    Some(base.join("PowerCap").join("ecrans.json"))
}

impl Prefs {
    pub fn load() -> Self {
        let map = prefs_path().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        Prefs { map }
    }
    pub fn save(&self) {
        let Some(p) = prefs_path() else { return };
        if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); }
        let _ = std::fs::write(p, serde_json::to_string_pretty(&self.map).unwrap_or_default());
    }
    /// Luminosité du menu de l'écran (0–100), 100 par défaut.
    pub fn brightness(&self, key: &str) -> f32 { self.map.get(key).and_then(|v| v["brightness"].as_f64()).unwrap_or(100.0) as f32 }
    pub fn content(&self, key: &str) -> Content { Content::from_id(self.map.get(key).and_then(|v| v["content"].as_str()).unwrap_or("auto")) }
    pub fn set(&mut self, key: &str, brightness: f32, content: Content) {
        self.map.insert(key.into(), serde_json::json!({ "brightness": brightness.round(), "content": content.id() }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edid_and_modes() {
        let mut d = vec![0u8; 128];
        d[..8].copy_from_slice(&[0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0]);
        d[8..10].copy_from_slice(&[0x4C, 0x2D]);
        d[21] = 60; d[22] = 34;
        d[72..90].copy_from_slice(&[0, 0, 0, 0xFC, 0, b'O', b'd', b'y', b's', b's', b'e', b'y', b' ', b'G', b'5', b'0', b'S', b'F']);
        d[90..108].copy_from_slice(&[0, 0, 0, 0xFF, 0, b'H', b'N', b'A', b'Y', b'1', b'\n', b' ', b' ', b' ', b' ', b' ', b' ', b' ']);
        let (v, n, s, size) = parse_edid(&d).unwrap();
        assert_eq!((v.as_str(), n.as_str(), s.as_str(), size), ("SAM", "Odyssey G50SF", "HNAY1", (60.0, 34.0)));
        let mut list = vec![Monitor { connector: "DP-1".into(), ..Default::default() }];
        parse_xrandr("DP-1 connected primary 2560x1440+0+0\n   2560x1440    179.95*+ 119.88    59.95  \n   1920x1080    60.00  \n", &mut list);
        assert_eq!((list[0].hz, list[0].res, list[0].rates.len()), (179.95, (2560, 1440), 3));
        assert_eq!(parse_vcp("VCP 10 C 253 50"), None);
        assert_eq!(parse_vcp("VCP 10 C 40 100"), Some(0.4));
    }

    #[test]
    fn estimates() {
        let g5 = Monitor { name: "Odyssey G50SF".into(), size_cm: (60.0, 34.0), ..Default::default() };
        assert_eq!(g5.tech(), Tech::Oled);
        // Fiche Samsung : ≈ 28 W en vidéo type à 120 Hz, luminosité d'usine.
        assert!((g5.watts(120.0, 1.0, 0.35) - 28.0).abs() < 0.5);
        // Image sombre ≪ image claire ; les Hz pèsent peu.
        assert!(g5.watts(180.0, 1.0, 0.12) < 20.0 && g5.watts(180.0, 1.0, 0.65) > 40.0);
        let d = g5.watts(180.0, 1.0, 0.3) - g5.watts(60.0, 1.0, 0.3);
        assert!(d > 1.0 && d < 2.0);
        assert!(g5.watts(180.0, 1.0, 1.0) <= 78.0);
        // LCD 27" inconnu : ≈ 20 à 25 W à luminosité moyenne, peu importe l'image.
        let lcd = Monitor { name: "VG27AQ".into(), size_cm: (59.8, 33.6), ..Default::default() };
        let w = lcd.watts(144.0, 0.5, 0.1);
        assert!(w > 13.0 && w < 25.0, "{w}");
        assert_eq!(w, lcd.watts(144.0, 0.5, 0.9));
    }
}
