//! PowerCap : règle la limite de puissance (en watts) des cartes graphiques
//! NVIDIA sous Linux, et montre en direct ce que consomment la carte, le
//! processeur (estimation, avec ses limites PL1/PL2) et le PC complet. Le
//! réglage de la carte s'applique tout de suite et, si on le demande, à chaque
//! démarrage et après chaque mise en veille grâce à un petit service systemd
//! par carte (/etc/systemd/system/powercap-gpu-<id>.service). Les actions qui
//! demandent les droits administrateur passent par pkexec.

use eframe::egui::{self, Color32, CornerRadius, Margin, RichText, Stroke};
use std::collections::HashMap;
use std::process::Command;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

mod selfupdate;
use selfupdate::SelfUpdate;

const UNIT_DIR: &str = "/etc/systemd/system";
const UNIT_PREFIX: &str = "powercap-gpu";
/// Préfixe des services créés par les versions 1.0.0 et 1.0.1 (alors appelées Watts).
const LEGACY_PREFIX: &str = "watts-gpu-power";

/// Écrit le service de la carte, l'active et applique la limite.
/// `$1` = watts, `$2` = UUID de la carte, `$3` = nom du service.
const PERSIST_SCRIPT: &str = r#"set -e
case "$1" in ''|*[!0-9]*) echo "valeur invalide" >&2; exit 2;; esac
case "$2" in GPU-*) ;; *) echo "carte invalide" >&2; exit 2;; esac
case "$2" in *[!A-Za-z0-9-]*) echo "carte invalide" >&2; exit 2;; esac
case "$3" in powercap-gpu-*.service) ;; *) echo "service invalide" >&2; exit 2;; esac
case "$3" in */*|*[!A-Za-z0-9.-]*) echo "service invalide" >&2; exit 2;; esac
NVSMI="$(command -v nvidia-smi || echo /usr/bin/nvidia-smi)"
# Services des anciennes versions (l'app s'appelait alors Watts).
for old in watts-gpu-power.service "watts-gpu-power-${3#powercap-gpu-}"; do
  if [ -e "/etc/systemd/system/$old" ]; then
    systemctl disable "$old" >/dev/null 2>&1 || true
    rm -f "/etc/systemd/system/$old"
  fi
done
cat > "/etc/systemd/system/$3" <<EOF
# Créé par PowerCap (https://github.com/VGioM/PowerCap) : limite de puissance
# d'une carte graphique NVIDIA, relancée au démarrage et après chaque veille.
[Unit]
Description=Limite de puissance NVIDIA $2 ($1 W, réglée avec PowerCap)
After=nvidia-persistenced.service display-manager.service suspend.target hibernate.target hybrid-sleep.target suspend-then-hibernate.target

[Service]
Type=oneshot
ExecStart=$NVSMI -i $2 -pl $1

[Install]
WantedBy=graphical.target suspend.target hibernate.target hybrid-sleep.target suspend-then-hibernate.target
EOF
chmod 644 "/etc/systemd/system/$3"
systemctl daemon-reload
systemctl reenable "$3"
"$NVSMI" -i "$2" -pl "$1"
"#;

/// Retire le service de la carte et remet sa limite d'origine.
/// `$1` = watts d'origine, `$2` = UUID, `$3` = nom du service.
const REMOVE_SCRIPT: &str = r#"set -e
case "$1" in ''|*[!0-9]*) echo "valeur invalide" >&2; exit 2;; esac
case "$2" in GPU-*) ;; *) echo "carte invalide" >&2; exit 2;; esac
case "$2" in *[!A-Za-z0-9-]*) echo "carte invalide" >&2; exit 2;; esac
case "$3" in powercap-gpu-*.service|watts-gpu-power*.service) ;; *) echo "service invalide" >&2; exit 2;; esac
case "$3" in */*|*[!A-Za-z0-9.-]*) echo "service invalide" >&2; exit 2;; esac
NVSMI="$(command -v nvidia-smi || echo /usr/bin/nvidia-smi)"
systemctl disable "$3" >/dev/null 2>&1 || true
rm -f "/etc/systemd/system/$3"
systemctl daemon-reload
"$NVSMI" -i "$2" -pl "$1"
"#;

const BG: Color32 = Color32::from_rgb(0x17, 0x18, 0x1B);
const CARD: Color32 = Color32::from_rgb(0x21, 0x22, 0x26);
const CARD2: Color32 = Color32::from_rgb(0x2B, 0x2C, 0x31);
const LINE: Color32 = Color32::from_rgb(0x33, 0x35, 0x3B);
const TEXT: Color32 = Color32::from_rgb(0xF2, 0xF2, 0xF4);
const MUTED: Color32 = Color32::from_rgb(0xA0, 0xA3, 0xAB);
const ACCENT: Color32 = Color32::from_rgb(0x4C, 0xC2, 0x6B);
const ACCENT_DIM: Color32 = Color32::from_rgb(0x24, 0x4A, 0x31);
const ON_ACCENT: Color32 = Color32::from_rgb(0x0C, 0x1A, 0x10);
const WARM: Color32 = Color32::from_rgb(0xFF, 0xA9, 0x4D);
const HOT: Color32 = Color32::from_rgb(0xFF, 0x62, 0x62);

// ---------------------------------------------------------------- mesures

/// nvidia-smi : chemin absolu (pkexec l'exige), sinon on compte sur le PATH.
fn nvsmi() -> String {
    ["/usr/bin/nvidia-smi", "/usr/sbin/nvidia-smi", "/bin/nvidia-smi", "/usr/local/bin/nvidia-smi"]
        .iter().find(|p| std::path::Path::new(p).exists()).map(|p| p.to_string())
        .unwrap_or_else(|| "nvidia-smi".into())
}

#[derive(Clone, Default, Debug, PartialEq)]
struct Gpu {
    index: u32, uuid: String, name: String,
    draw: f32, limit: f32, default: f32, min: f32, max: f32,
    temp: f32, fan: f32, clock: f32, util: f32,
}

impl Gpu {
    /// Les cartes de PC portables (et certaines anciennes) refusent le réglage.
    fn adjustable(&self) -> bool { self.default > 0.0 && self.min > 0.0 && self.max > self.min }
    /// Nom du service systemd propre à cette carte (stable : tiré de l'UUID).
    fn unit(&self) -> String {
        let id: String = self.uuid.trim_start_matches("GPU-").chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect();
        format!("{UNIT_PREFIX}-{}.service", if id.is_empty() { self.index.to_string() } else { id.to_lowercase() })
    }
}

fn num(s: &str) -> f32 { s.trim().parse().unwrap_or(0.0) }

const QUERY: &str = "--query-gpu=index,uuid,name,power.draw,power.limit,power.default_limit,power.min_limit,power.max_limit,temperature.gpu,fan.speed,clocks.sm,utilization.gpu";

fn parse_gpu(line: &str) -> Result<Gpu, String> {
    let f: Vec<&str> = line.split(',').map(str::trim).collect();
    if f.len() < 12 { return Err(format!("Réponse inattendue de nvidia-smi : {line}")); }
    // Le nom peut, en théorie, contenir des virgules : les 9 derniers champs sont numériques.
    let n = f.len();
    Ok(Gpu {
        index: f[0].parse().unwrap_or(0), uuid: f[1].to_string(), name: f[2..n - 9].join(", "),
        draw: num(f[n - 9]), limit: num(f[n - 8]), default: num(f[n - 7]), min: num(f[n - 6]), max: num(f[n - 5]),
        temp: num(f[n - 4]), fan: num(f[n - 3]), clock: num(f[n - 2]), util: num(f[n - 1]),
    })
}

fn query_gpus() -> Result<Vec<Gpu>, String> {
    let out = Command::new(nvsmi()).args(["--format=csv,noheader,nounits", QUERY]).output()
        .map_err(|_| "nvidia-smi est introuvable : il faut le pilote NVIDIA propriétaire (sur Bazzite, l'image « -nvidia »).".to_string())?;
    if !out.status.success() {
        let e = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let o = String::from_utf8_lossy(&out.stdout).trim().to_string();
        return Err(format!("nvidia-smi a échoué : {}", if e.is_empty() { o } else { e }));
    }
    let gpus = String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.trim().is_empty()).map(parse_gpu).collect::<Result<Vec<_>, _>>()?;
    if gpus.is_empty() { return Err("Aucune carte graphique NVIDIA trouvée.".into()); }
    Ok(gpus)
}

/// Température du processeur (Intel « Package id 0 », AMD « Tctl »), en °C.
fn cpu_temp() -> Option<f32> {
    for entry in std::fs::read_dir("/sys/class/hwmon").ok()?.flatten() {
        let p = entry.path();
        let name = std::fs::read_to_string(p.join("name")).unwrap_or_default();
        if !matches!(name.trim(), "coretemp" | "k10temp" | "zenpower") { continue; }
        let mut first = None;
        for i in 1..=64 {
            let Some(v) = std::fs::read_to_string(p.join(format!("temp{i}_input"))).ok().and_then(|s| s.trim().parse::<f32>().ok()) else { continue };
            let label = std::fs::read_to_string(p.join(format!("temp{i}_label"))).unwrap_or_default();
            let label = label.trim();
            if label.starts_with("Package") || label == "Tctl" || label == "Tdie" { return Some(v / 1000.0); }
            first.get_or_insert(v / 1000.0);
        }
        return first;
    }
    None
}

/// Limites de puissance du processeur lues dans /sys/class/powercap (RAPL),
/// telles que le BIOS les a réglées : PL1 (continu), PL2 (pointes), durée des pointes.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
struct CpuLimits { pl1: Option<f32>, pl2: Option<f32>, tau: Option<f32> }

fn read_f(path: &std::path::Path) -> Option<f32> { std::fs::read_to_string(path).ok()?.trim().parse().ok() }

fn cpu_limits() -> CpuLimits {
    let base = std::path::Path::new("/sys/class/powercap/intel-rapl:0");
    let mut l = CpuLimits::default();
    for c in 0..3 {
        let Ok(name) = std::fs::read_to_string(base.join(format!("constraint_{c}_name"))) else { continue };
        let w = read_f(&base.join(format!("constraint_{c}_power_limit_uw"))).map(|u| u / 1e6).filter(|w| *w > 0.0);
        match name.trim() {
            "long_term" => { l.pl1 = w; l.tau = read_f(&base.join(format!("constraint_{c}_time_window_us"))).map(|u| u / 1e6).filter(|t| *t > 0.0); }
            "short_term" => l.pl2 = w,
            _ => {}
        }
    }
    l
}

/// Nom du processeur (« 13th Gen Intel(R) Core(TM) i5-13400F » → « Intel Core i5-13400F »).
fn cpu_name() -> String {
    let info = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let raw = info.lines().find_map(|l| l.strip_prefix("model name")).and_then(|l| l.split_once(':')).map(|(_, v)| v.trim().to_string()).unwrap_or_default();
    let mut n = raw.replace("(R)", "").replace("(TM)", "").replace(" CPU", "");
    if let Some(i) = n.find("Intel") { n = n[i..].to_string(); }
    if let Some(i) = n.find(" with ") { n.truncate(i); }
    n.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Temps CPU cumulés (actif, total) de /proc/stat.
fn cpu_times() -> Option<(u64, u64)> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let v: Vec<u64> = stat.lines().next()?.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
    if v.len() < 5 { return None; }
    let total: u64 = v.iter().take(8).sum();
    let idle = v[3] + v[4];
    Some((total - idle, total))
}

/// Fréquence moyenne actuelle et fréquence maximale, en GHz.
fn cpu_freqs() -> (f32, f32) {
    let (mut sum, mut n, mut max) = (0.0, 0.0, 0.0f32);
    if let Ok(dir) = std::fs::read_dir("/sys/devices/system/cpu") {
        for e in dir.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.starts_with("cpu") || !name[3..].chars().all(|c| c.is_ascii_digit()) || name.len() == 3 { continue; }
            if let Some(f) = read_f(&e.path().join("cpufreq/scaling_cur_freq")) { sum += f; n += 1.0; }
            if let Some(f) = read_f(&e.path().join("cpufreq/cpuinfo_max_freq")) { max = max.max(f); }
        }
    }
    (if n > 0.0 { sum / n / 1e6 } else { 0.0 }, max / 1e6)
}

/// Consommation estimée du processeur. Le compteur d'énergie exact (RAPL) n'est
/// lisible qu'en administrateur : on estime à partir de la charge et de la
/// fréquence, entre la consommation au repos et la pointe permise par PL2.
fn cpu_power_estimate(util: f32, freq_ratio: f32, limits: &CpuLimits) -> f32 {
    // Certaines cartes mères mettent PL1/PL2 à « illimité » (4095 W) : on borne.
    let pl1 = limits.pl1.unwrap_or(65.0).min(320.0);
    let pl2 = limits.pl2.unwrap_or(pl1 * 1.5).min(320.0);
    // La pointe, c'est PL2 ; si PL2 est sous PL1, c'est lui qui limite tout le temps.
    let peak = pl2.min(pl1 * 2.5).max(5.0);
    let idle = (peak * 0.06).max(5.0);
    let f = freq_ratio.clamp(0.2, 1.0);
    (idle + (peak - idle) * util.clamp(0.0, 1.0) * f * f).min(peak)
}

/// Le reste du PC (carte mère, mémoire, SSD, ventilateurs, USB), en W, et le
/// rendement typique d'une alimentation 80 Plus Gold à charge moyenne.
const REST_W: f32 = 35.0;
const PSU_EFFICIENCY: f32 = 0.90;

#[derive(Clone, Copy, Default)]
struct Peaks { temp: f32, draw: f32 }

#[derive(Clone, Default)]
struct Cpu { name: String, temp: Option<f32>, util: f32, freq: f32, max_freq: f32, limits: CpuLimits, watts: f32 }

#[derive(Clone)]
struct Live {
    gpus: Result<Vec<Gpu>, String>,
    cpu: Cpu,
    cpu_max: f32,
    cpu_w_max: f32,
    total_max: f32,
    peaks: HashMap<String, Peaks>,
}

impl Live {
    /// Consommation du PC complet à la prise (hors écran), en W.
    fn total(&self) -> f32 {
        let gpus: f32 = self.gpus.as_ref().map(|l| l.iter().map(|g| g.draw).sum()).unwrap_or(0.0);
        (gpus + self.cpu.watts + REST_W) / PSU_EFFICIENCY
    }
}

fn spawn_monitor(ctx: egui::Context) -> Arc<Mutex<Live>> {
    let live = Arc::new(Mutex::new(Live { gpus: Err("Lecture des cartes graphiques…".into()), cpu: Cpu { name: cpu_name(), ..Default::default() },
        cpu_max: 0.0, cpu_w_max: 0.0, total_max: 0.0, peaks: HashMap::new() }));
    let shared = live.clone();
    let mut prev = cpu_times();
    // Secondes passées au-dessus de PL1 : au-delà de la durée de pointe, le processeur redescend à PL1.
    let mut above_pl1 = 0.0f32;
    std::thread::spawn(move || loop {
        let gpus = query_gpus();
        let now = cpu_times();
        let util = match (prev, now) {
            (Some((a0, t0)), Some((a1, t1))) if t1 > t0 => (a1 - a0) as f32 / (t1 - t0) as f32,
            _ => 0.0,
        };
        prev = now;
        let (freq, max_freq) = cpu_freqs();
        let limits = cpu_limits();
        let mut watts = cpu_power_estimate(util, if max_freq > 0.0 { freq / max_freq } else { 1.0 }, &limits);
        if let (Some(pl1), Some(tau)) = (limits.pl1.filter(|p| limits.pl2.map_or(true, |p2| *p <= p2)), limits.tau) {
            if watts > pl1 { above_pl1 += 1.0; if above_pl1 > tau { watts = pl1; } } else { above_pl1 = 0.0; }
        }
        let temp = cpu_temp();
        if let Ok(mut l) = shared.lock() {
            if let Ok(list) = &gpus {
                for g in list {
                    let p = l.peaks.entry(g.uuid.clone()).or_default();
                    p.temp = p.temp.max(g.temp);
                    p.draw = p.draw.max(g.draw);
                }
            }
            if let Some(c) = temp { l.cpu_max = l.cpu_max.max(c); }
            l.gpus = gpus;
            let name = std::mem::take(&mut l.cpu.name);
            l.cpu = Cpu { name, temp, util, freq, max_freq, limits, watts };
            l.cpu_w_max = l.cpu_w_max.max(watts);
            let total = l.total();
            l.total_max = l.total_max.max(total);
        }
        ctx.request_repaint();
        std::thread::sleep(Duration::from_secs(1));
    });
    live
}

// ---------------------------------------------------------------- services

#[derive(Clone, Debug, PartialEq)]
struct Unit { file: String, target: String, watts: u32, enabled: bool }

/// Lit « ExecStart=… nvidia-smi -i <carte> -pl <watts> » : (carte, watts).
fn parse_unit(text: &str) -> Option<(String, u32)> {
    text.lines().find_map(|l| {
        let words: Vec<&str> = l.trim().strip_prefix("ExecStart=")?.split_whitespace().collect();
        let after = |flag: &str| words.iter().position(|w| *w == flag).and_then(|i| words.get(i + 1)).copied();
        Some((after("-i").unwrap_or("0").to_string(), after("-pl")?.parse().ok()?))
    })
}

fn read_units() -> Vec<Unit> {
    let Ok(dir) = std::fs::read_dir(UNIT_DIR) else { return vec![] };
    let mut units: Vec<Unit> = dir.flatten().filter_map(|e| {
        let file = e.file_name().to_string_lossy().to_string();
        if !(file.starts_with(UNIT_PREFIX) || file.starts_with(LEGACY_PREFIX)) || !file.ends_with(".service") { return None; }
        let (target, watts) = parse_unit(&std::fs::read_to_string(e.path()).ok()?)?;
        Some(Unit { file, target, watts, enabled: false })
    }).collect();
    for u in &mut units {
        u.enabled = Command::new("systemctl").args(["is-enabled", &u.file]).output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "enabled").unwrap_or(false);
    }
    // Les services PowerCap d'abord, ceux de Watts ensuite.
    units.sort_by_key(|u| (u.file.starts_with(LEGACY_PREFIX), u.file.clone()));
    units
}

fn unit_for<'a>(units: &'a [Unit], g: &Gpu) -> Option<&'a Unit> {
    units.iter().find(|u| u.target == g.uuid || u.target == g.index.to_string())
}

#[derive(Clone)]
enum Job { Apply { w: u32, uuid: String }, Persist { w: u32, uuid: String, unit: String }, Remove { w: u32, uuid: String, unit: String } }

fn run_job(job: &Job) -> Result<String, String> {
    if Command::new("pkexec").arg("--version").output().is_err() {
        return Err("pkexec est introuvable : impossible de demander les droits administrateur.".into());
    }
    let mut cmd = Command::new("pkexec");
    match job {
        Job::Apply { w, uuid } => cmd.args([nvsmi(), "-i".into(), uuid.clone(), "-pl".into(), w.to_string()]),
        Job::Persist { w, uuid, unit } => cmd.args(["/bin/sh", "-c", PERSIST_SCRIPT, "sh", &w.to_string(), uuid, unit]),
        Job::Remove { w, uuid, unit } => cmd.args(["/bin/sh", "-c", REMOVE_SCRIPT, "sh", &w.to_string(), uuid, unit]),
    };
    let out = cmd.output().map_err(|e| format!("Impossible de lancer pkexec ({e})."))?;
    match out.status.code() {
        Some(0) => Ok(match job {
            Job::Apply { w, .. } => format!("Limite réglée à {w} W jusqu'au prochain redémarrage."),
            Job::Persist { w, .. } => format!("Limite réglée à {w} W, et gardée au démarrage et après la veille."),
            Job::Remove { w, .. } => format!("Réglage permanent retiré, carte revenue à {w} W."),
        }),
        Some(126) | Some(127) => Err("Autorisation annulée ou refusée.".into()),
        _ => {
            let err = format!("{}\n{}", String::from_utf8_lossy(&out.stderr), String::from_utf8_lossy(&out.stdout));
            let err = err.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ");
            Err(if err.is_empty() { "L'opération a échoué.".into() } else { err })
        }
    }
}

// ---------------------------------------------------------------- conseils

/// Perte d'images/s estimée (en %) quand la carte est à pleine charge, selon
/// la part de la limite d'origine. Courbe typique des RTX 30/40/50.
fn fps_loss(ratio: f32) -> f32 {
    const P: [(f32, f32); 7] = [(0.4, 40.0), (0.5, 27.0), (0.6, 16.0), (0.7, 9.0), (0.8, 4.0), (0.9, 1.5), (1.0, 0.0)];
    if ratio >= 1.0 { return 0.0; }
    if ratio <= P[0].0 { return P[0].1; }
    for w in P.windows(2) {
        let ((x0, y0), (x1, y1)) = (w[0], w[1]);
        if ratio <= x1 { return y0 + (y1 - y0) * (ratio - x0) / (x1 - x0); }
    }
    0.0
}

/// Pas du curseur : 5 W, ou 1 W pour les petites cartes.
fn step(g: &Gpu) -> f32 { if g.max - g.min >= 40.0 { 5.0 } else { 1.0 } }

fn preset_watts(g: &Gpu, ratio: f32) -> f32 {
    let s = step(g);
    ((g.default * ratio / s).round() * s).clamp(g.min, g.max)
}

const PRESETS: [(&str, &str, f32); 4] = [
    ("Canicule", "le plus frais", 0.70),
    ("Équilibré", "recommandé", 0.80),
    ("Léger", "≈ rien perdu", 0.90),
    ("D'origine", "100 %", 1.00),
];

// ---------------------------------------------------------------- interface

struct App {
    live: Arc<Mutex<Live>>,
    units: Vec<Unit>,
    selected: String,
    target: Option<f32>,
    job: Option<mpsc::Receiver<Result<String, String>>>,
    msg: Option<Result<String, String>>,
    hook: TestHook,
    update: SelfUpdate,
}

impl App {
    fn new(cc: &eframe::CreationContext) -> Self {
        style(&cc.egui_ctx);
        App { live: spawn_monitor(cc.egui_ctx.clone()), units: read_units(), selected: String::new(), target: None, job: None, msg: None, hook: TestHook::default(), update: SelfUpdate::new() }
    }

    fn start(&mut self, job: Job, ctx: &egui::Context) {
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || { let _ = tx.send(run_job(&job)); ctx.request_repaint(); });
        self.job = Some(rx);
        self.msg = None;
    }
}

fn style(ctx: &egui::Context) {
    let mut s = (*ctx.style()).clone();
    s.visuals = egui::Visuals::dark();
    let v = &mut s.visuals;
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = ACCENT;
    v.selection.stroke = Stroke::new(1.0_f32, ON_ACCENT);
    v.slider_trailing_fill = true;
    v.widgets.inactive.bg_fill = CARD2;
    v.widgets.inactive.weak_bg_fill = CARD2;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, LINE);
    v.widgets.inactive.corner_radius = CornerRadius::same(8);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x36, 0x38, 0x3E);
    v.widgets.hovered.bg_fill = Color32::from_rgb(0x36, 0x38, 0x3E);
    v.widgets.hovered.corner_radius = CornerRadius::same(8);
    v.widgets.active.corner_radius = CornerRadius::same(8);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, LINE);
    s.spacing.item_spacing = egui::vec2(10.0, 8.0);
    s.spacing.button_padding = egui::vec2(14.0, 8.0);
    s.spacing.slider_rail_height = 8.0;
    s.spacing.interact_size.y = 26.0;
    for (ts, size) in [(egui::TextStyle::Body, 15.0), (egui::TextStyle::Button, 15.0), (egui::TextStyle::Small, 12.5)] {
        if let Some(f) = s.text_styles.get_mut(&ts) { f.size = size; }
    }
    ctx.set_style(s);
}

fn card<R>(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new().fill(CARD).stroke(Stroke::new(1.0_f32, LINE)).corner_radius(CornerRadius::same(14)).inner_margin(Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title.to_uppercase()).size(12.0).color(MUTED).strong());
            ui.add_space(6.0);
            add(ui)
        }).inner
}

fn tile(ui: &mut egui::Ui, label: &str, value: String, sub: String, color: Color32) {
    egui::Frame::new().fill(CARD2).corner_radius(CornerRadius::same(10)).inner_margin(Margin::symmetric(12, 10)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(label).size(12.5).color(MUTED));
        ui.label(RichText::new(value).size(22.0).strong().color(color));
        ui.label(RichText::new(sub).size(12.5).color(MUTED));
    });
}

fn temp_color(t: f32, warm: f32, hot: f32) -> Color32 { if t >= hot { HOT } else if t >= warm { WARM } else { TEXT } }

fn small(ui: &mut egui::Ui, text: impl Into<String>) { ui.label(RichText::new(text).size(12.5).color(MUTED)); }

fn big_button(ui: &mut egui::Ui, enabled: bool, text: &str, primary: bool, width: f32) -> bool {
    let rt = RichText::new(text).strong().color(if primary { ON_ACCENT } else { TEXT });
    let mut b = egui::Button::new(rt).min_size(egui::vec2(width, 38.0)).corner_radius(CornerRadius::same(10));
    if primary { b = b.fill(ACCENT); }
    ui.add_enabled(enabled, b).clicked()
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(rx) = &self.job {
            if let Ok(res) = rx.try_recv() {
                self.msg = Some(res);
                self.job = None;
                self.units = read_units();
            }
        }
        let live = self.live.lock().map(|l| l.clone()).unwrap_or_else(|e| e.into_inner().clone());
        let gpus = live.gpus.as_ref().ok().cloned().unwrap_or_default();
        if !gpus.iter().any(|g| g.uuid == self.selected) {
            if let Some(g) = gpus.first() { self.selected = g.uuid.clone(); self.target = None; }
        }
        let gpu = gpus.iter().find(|g| g.uuid == self.selected).cloned();
        if let (None, Some(g)) = (self.target, &gpu) {
            let saved = unit_for(&self.units, g).map(|u| u.watts as f32);
            self.target = Some(saved.unwrap_or(g.limit).clamp(g.min.min(g.limit), g.max.max(g.limit)));
        }
        let busy = self.job.is_some();

        egui::CentralPanel::default().frame(egui::Frame::new().fill(BG).inner_margin(Margin::same(18))).show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("PowerCap").size(28.0).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if gpus.len() > 1 {
                            let current = gpu.as_ref().map(|g| format!("{} · {}", g.index, g.name)).unwrap_or_default();
                            egui::ComboBox::from_id_salt("gpu").selected_text(current).show_ui(ui, |ui| {
                                for g in &gpus {
                                    if ui.selectable_label(g.uuid == self.selected, format!("{} · {}", g.index, g.name)).clicked() && g.uuid != self.selected {
                                        self.selected = g.uuid.clone();
                                        self.target = None;
                                    }
                                }
                            });
                        } else if let Some(g) = &gpu {
                            ui.label(RichText::new(&g.name).color(MUTED));
                        }
                    });
                });
                ui.label(RichText::new("Limite de puissance de la carte graphique et consommation du PC : moins de watts, moins de chaleur et de bruit.").color(MUTED));
                ui.add_space(10.0);

                let Some(g) = gpu.clone() else {
                    card(ui, "Carte graphique", |ui| {
                        ui.label(RichText::new(live.gpus.as_ref().err().cloned().unwrap_or_default()).color(WARM));
                    });
                    return;
                };
                let peaks = live.peaks.get(&g.uuid).copied().unwrap_or_default();

                // --- En direct
                let cpu = &live.cpu;
                card(ui, "Consommation en direct", |ui| {
                    ui.columns(3, |c| {
                        tile(&mut c[0], "Carte graphique", format!("{:.0} W", g.draw), format!("limite {:.0} W · max vu {:.0} W", g.limit, peaks.draw), TEXT);
                        tile(&mut c[1], "Processeur (estimé)", format!("≈ {:.0} W", cpu.watts), format!("charge {:.0} % · max vu {:.0} W", cpu.util * 100.0, live.cpu_w_max), TEXT);
                        tile(&mut c[2], "PC complet (estimé)", format!("≈ {:.0} W", live.total()), format!("à la prise · max vu {:.0} W", live.total_max), ACCENT);
                    });
                    let frac = if g.limit > 0.0 { (g.draw / g.limit).clamp(0.0, 1.0) } else { 0.0 };
                    ui.add(egui::ProgressBar::new(frac).desired_height(10.0).fill(if frac > 0.95 { WARM } else { ACCENT }).corner_radius(CornerRadius::same(5)));
                    small(ui, format!("Carte mesurée par le pilote. Processeur estimé d'après sa charge et sa fréquence. PC complet : carte + processeur + ≈ {REST_W:.0} W pour le reste (carte mère, mémoire, SSD, ventilateurs), avec une alimentation à ≈ {:.0} % de rendement ; écran non compris.", PSU_EFFICIENCY * 100.0));
                });
                ui.add_space(10.0);
                card(ui, "Températures", |ui| {
                    ui.columns(3, |c| {
                        tile(&mut c[0], "Carte graphique", format!("{:.0} °C", g.temp), format!("max vu {:.0} °C · ventilo {:.0} %", peaks.temp, g.fan), temp_color(g.temp, 75.0, 83.0));
                        match cpu.temp {
                            Some(t) => tile(&mut c[1], "Processeur", format!("{t:.0} °C"), format!("max vu {:.0} °C", live.cpu_max), temp_color(t, 85.0, 95.0)),
                            None => tile(&mut c[1], "Processeur", "—".into(), "capteur introuvable".into(), MUTED),
                        }
                        tile(&mut c[2], "Fréquences", format!("{:.1} GHz", cpu.freq), format!("processeur, max {:.1} GHz", cpu.max_freq), TEXT);
                    });
                    small(ui, "Les « max vu » comptent depuis l'ouverture de PowerCap : laisse-le ouvert pendant une partie pour voir les pics.");
                });
                ui.add_space(10.0);

                // --- Processeur
                card(ui, "Limites du processeur", |ui| {
                    if !cpu.name.is_empty() { ui.label(RichText::new(&cpu.name).strong()); }
                    let l = cpu.limits;
                    if l.pl1.is_none() && l.pl2.is_none() {
                        small(ui, "Limites non lisibles sur ce processeur (sur AMD, elles s'appellent PPT et se règlent dans le BIOS, souvent via « Eco Mode »).");
                    } else {
                        let w = |v: Option<f32>| v.map(|v| if v >= 1000.0 { "illimité".to_string() } else { format!("{v:.0} W") }).unwrap_or_else(|| "—".into());
                        ui.columns(3, |c| {
                            tile(&mut c[0], "PL1 · en continu", w(l.pl1), "puissance tenue sans limite de temps".into(), TEXT);
                            tile(&mut c[1], "PL2 · pointes", w(l.pl2), "puissance des accélérations".into(), TEXT);
                            tile(&mut c[2], "Durée des pointes", l.tau.map(|t| format!("{t:.0} s")).unwrap_or_else(|| "—".into()), "avant de redescendre à PL1".into(), TEXT);
                        });
                        if let Some((pl1, pl2)) = l.pl1.zip(l.pl2).filter(|(a, b)| b + 1.0 < *a) {
                            let pl1_advice = ((pl2 / 1.4 / 5.0).round() * 5.0).max(35.0);
                            ui.label(RichText::new(format!("PL2 ({pl2:.0} W) est plus bas que PL1 ({}) : le processeur est donc limité à {pl2:.0} W en permanence, sans pointes. Pour avoir « {pl1_advice:.0} W en continu, pointes à {pl2:.0} W », règle PL1 (Long Duration) à {pl1_advice:.0} W dans le BIOS.", if pl1 >= 1000.0 { "illimité".to_string() } else { format!("{pl1:.0} W") })).color(WARM));
                        } else if let Some(pl1) = l.pl1.filter(|p| *p < 1000.0) {
                            let advice = (pl1 * 1.4 / 5.0).round() * 5.0;
                            let hot = l.pl2.map(|p| p > advice + 2.0).unwrap_or(false);
                            ui.label(RichText::new(if hot {
                                format!("Pour un PC plus frais : PL2 ≈ {advice:.0} W (1,4 × PL1) coupe les pics de chaleur sans presque rien changer en jeu.")
                            } else {
                                "Réglage déjà raisonnable pour garder le PC au frais.".to_string()
                            }).color(if hot { WARM } else { ACCENT }));
                        }
                        small(ui, "Lecture seule : ces limites se règlent dans le BIOS (Intel : « Package Power Limit », chez ASUS : Ai Tweaker, puis Internal CPU Power Management). Pense à désactiver « MultiCore Enhancement » pour qu'elles soient respectées.");
                    }
                });
                ui.add_space(10.0);

                // --- Réglage
                if !g.adjustable() {
                    card(ui, "Limite de la carte graphique", |ui| {
                        ui.label(RichText::new("Cette carte ne permet pas de régler sa limite de puissance.").strong().color(WARM));
                        small(ui, "C'est le cas de la plupart des cartes de PC portables et de certaines cartes anciennes : le pilote NVIDIA bloque le réglage. Les mesures en direct restent disponibles.");
                    });
                } else {
                    let mut target = self.target.unwrap_or(g.limit);
                    card(ui, "Limite de la carte graphique", |ui| {
                        let ratio = target / g.default;
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("{target:.0} W")).size(36.0).strong().color(ACCENT));
                            ui.vertical(|ui| {
                                ui.add_space(6.0);
                                ui.label(RichText::new(format!("{:.0} % de l'origine ({:.0} W)", ratio * 100.0, g.default)).color(MUTED));
                            });
                        });
                        ui.spacing_mut().slider_width = ui.available_width();
                        ui.add(egui::Slider::new(&mut target, g.min..=g.max).step_by(step(&g) as f64).show_value(false));
                        ui.horizontal(|ui| {
                            small(ui, format!("{:.0} W", g.min));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| small(ui, format!("{:.0} W", g.max)));
                        });
                        ui.add_space(4.0);
                        ui.columns(PRESETS.len(), |c| {
                            for (i, (name, hint, r)) in PRESETS.iter().enumerate() {
                                let w = preset_watts(&g, *r);
                                let sel = (target - w).abs() < 0.5;
                                let text = RichText::new(format!("{name}\n{w:.0} W\n{hint}")).size(13.0).color(if sel { ACCENT } else { TEXT });
                                let b = egui::Button::new(text).min_size(egui::vec2(c[i].available_width(), 62.0))
                                    .fill(if sel { ACCENT_DIM } else { CARD2 }).stroke(Stroke::new(1.0_f32, if sel { ACCENT } else { LINE }));
                                if c[i].add(b).clicked() { target = w; }
                            }
                        });
                        ui.add_space(6.0);
                        egui::Frame::new().fill(CARD2).corner_radius(CornerRadius::same(10)).inner_margin(Margin::same(12)).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            let ratio = target / g.default;
                            let line = if (0.995..=1.005).contains(&ratio) {
                                "Réglage d'origine : toutes les performances, toute la chaleur.".to_string()
                            } else if ratio > 1.0 {
                                format!("Au-dessus de l'origine : ≈ +1 % d'images/s au mieux, pour {:.0} W de chaleur en plus.", target - g.default)
                            } else {
                                format!("Carte à fond : ≈ −{:.0} % d'images/s, et jusqu'à {:.0} W de chaleur en moins dans le boîtier.", fps_loss(ratio).max(0.5), g.default - target)
                            };
                            ui.label(RichText::new(line).strong());
                            if ratio < 0.6 {
                                ui.label(RichText::new("Sous 60 % la perte grimpe vite pour peu de fraîcheur en plus.").color(WARM));
                            }
                            small(ui, "Avec le DLSS ou un plafond de fps (60, 120…), la carte tourne souvent sous la limite : la perte réelle est plus faible, voire nulle. Estimation typique des RTX 30/40/50.");
                            small(ui, format!("Conseil : {:.0} W au quotidien, {:.0} W pendant les canicules.", preset_watts(&g, 0.8), preset_watts(&g, 0.7)));
                        });
                        ui.add_space(8.0);
                        let w = target.round() as u32;
                        let half = (ui.available_width() - 10.0) / 2.0;
                        ui.horizontal(|ui| {
                            if big_button(ui, !busy, "Appliquer maintenant", false, half) {
                                self.start(Job::Apply { w, uuid: g.uuid.clone() }, ui.ctx());
                            }
                            if big_button(ui, !busy, "Appliquer et garder au démarrage", true, half) {
                                self.start(Job::Persist { w, uuid: g.uuid.clone(), unit: g.unit() }, ui.ctx());
                            }
                        });
                        small(ui, "« Maintenant » : jusqu'au prochain redémarrage. Le mot de passe administrateur est demandé.");
                    });
                    self.target = Some(target);
                }
                ui.add_space(10.0);

                // --- Démarrage
                card(ui, "Au démarrage", |ui| {
                    let unit = unit_for(&self.units, &g).cloned();
                    match &unit {
                        Some(u) if u.enabled => {
                            ui.label(RichText::new(format!("Réglage permanent actif : {} W", u.watts)).strong().color(ACCENT));
                            ui.label(RichText::new("Appliqué à chaque démarrage et après chaque sortie de veille.").color(MUTED));
                        }
                        Some(u) => { ui.label(RichText::new(format!("Service présent ({} W) mais désactivé.", u.watts)).strong().color(WARM)); }
                        None => { ui.label(RichText::new(format!("Aucun réglage permanent : la carte revient à {:.0} W à chaque redémarrage.", g.default)).color(MUTED)); }
                    }
                    if let Some(u) = &unit {
                        if (g.limit - u.watts as f32).abs() > 1.0 {
                            ui.label(RichText::new(format!("La limite actuelle ({:.0} W) ne correspond pas au réglage permanent ({} W).", g.limit, u.watts)).color(WARM));
                        }
                        ui.add_space(4.0);
                        let half = (ui.available_width() - 10.0) / 2.0;
                        ui.horizontal(|ui| {
                            if big_button(ui, !busy && g.adjustable(), &format!("Réappliquer {} W", u.watts), false, half) {
                                self.start(Job::Persist { w: u.watts, uuid: g.uuid.clone(), unit: g.unit() }, ui.ctx());
                            }
                            if big_button(ui, !busy, "Retirer le réglage permanent", false, half) {
                                self.start(Job::Remove { w: g.default.round() as u32, uuid: g.uuid.clone(), unit: u.file.clone() }, ui.ctx());
                            }
                        });
                    }
                    let file = unit.map(|u| u.file).unwrap_or_else(|| g.unit());
                    ui.label(RichText::new(format!("{UNIT_DIR}/{file}")).monospace().size(11.5).color(MUTED));
                });

                ui.add_space(8.0);
                if busy {
                    ui.horizontal(|ui| { ui.spinner(); ui.label("En attente de l'autorisation administrateur…"); });
                } else if let Some(m) = &self.msg {
                    match m {
                        Ok(t) => ui.label(RichText::new(t).color(ACCENT)),
                        Err(t) => ui.label(RichText::new(t).color(HOT)),
                    };
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    small(ui, format!("PowerCap {}", env!("CARGO_PKG_VERSION")));
                    let label = if self.update.busy() { "Recherche…" } else { "Rechercher les mises à jour" };
                    if ui.add_enabled(!self.update.busy(), egui::Button::new(RichText::new(label).size(12.5)).small()).clicked() {
                        self.update.check_now(ui.ctx());
                    }
                    let mut auto = self.update.prefs.automatic;
                    if ui.checkbox(&mut auto, RichText::new("automatiquement").size(12.5).color(MUTED)).changed() {
                        self.update.prefs.automatic = auto;
                        self.update.prefs.save();
                    }
                });
            });
        });
        self.update.ui(ctx, ACCENT, ON_ACCENT, CARD2);
        self.hook.ui(ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) { self.update.relaunch_if_needed(); }
}

// ---------------------------------------------------------------- tests

/// Pour les tests seulement : `APP_SCREENSHOT=/chemin.png` enregistre une
/// capture après `APP_SCREENSHOT_DELAY` s (3 par défaut) puis ferme l'app.
struct TestHook { path: Option<String>, delay: f32, start: Instant, requested: bool }

impl Default for TestHook {
    fn default() -> Self {
        TestHook {
            path: std::env::var("APP_SCREENSHOT").ok().filter(|s| !s.is_empty()),
            delay: std::env::var("APP_SCREENSHOT_DELAY").ok().and_then(|s| s.parse().ok()).unwrap_or(3.0),
            start: Instant::now(), requested: false,
        }
    }
}

impl TestHook {
    fn ui(&mut self, ctx: &egui::Context) {
        let Some(path) = self.path.clone() else { return };
        if !self.requested && self.start.elapsed().as_secs_f32() >= self.delay {
            self.requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
        ctx.request_repaint_after(Duration::from_millis(200));
        let shot = ctx.input(|i| i.events.iter().find_map(|e| if let egui::Event::Screenshot { image, .. } = e { Some(image.clone()) } else { None }));
        if let Some(img) = shot {
            let [w, h] = img.size;
            let raw: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
            if let Some(buf) = image::RgbaImage::from_raw(w as u32, h as u32, raw) { let _ = buf.save(&path); }
            self.path = None;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nvidia_smi_lines() {
        let g = parse_gpu("0, GPU-ebab68d5-d9c6-1718-88bf-c79a87a14bc7, NVIDIA GeForce RTX 4070, 21.24, 200.00, 200.00, 100.00, 216.00, 39, 0, 2490, 3").unwrap();
        assert_eq!(g.name, "NVIDIA GeForce RTX 4070");
        assert_eq!((g.limit, g.default, g.min, g.max), (200.0, 200.0, 100.0, 216.0));
        assert!(g.adjustable());
        assert_eq!(g.unit(), "powercap-gpu-ebab68d5.service");
        // Carte de portable : limites non disponibles.
        let g = parse_gpu("1, GPU-1234, NVIDIA GeForce RTX 4060 Laptop GPU, [N/A], [N/A], [N/A], [N/A], [N/A], 40, [N/A], 0, 0").unwrap();
        assert_eq!((g.index, g.draw), (1, 0.0));
        assert!(!g.adjustable());
        // Nom avec virgule.
        let g = parse_gpu("0, GPU-x, A, B, 1, 2, 3, 4, 5, 6, 7, 8, 9").unwrap();
        assert_eq!((g.name.as_str(), g.util), ("A, B", 9.0));
        assert!(parse_gpu("rien").is_err());
    }

    #[test]
    fn reads_units() {
        assert_eq!(parse_unit("[Service]\nExecStart=/usr/bin/nvidia-smi -i GPU-ab-cd -pl 160\n"), Some(("GPU-ab-cd".into(), 160)));
        assert_eq!(parse_unit("ExecStart=/usr/bin/nvidia-smi -pl 150"), Some(("0".into(), 150)));
        assert_eq!(parse_unit("ExecStart=/usr/bin/true"), None);
        let g = Gpu { index: 0, uuid: "GPU-ab".into(), ..Default::default() };
        let units = vec![Unit { file: "watts-gpu-power.service".into(), target: "0".into(), watts: 150, enabled: true }];
        assert_eq!(unit_for(&units, &g).map(|u| u.watts), Some(150));
    }

    #[test]
    fn presets_and_loss() {
        let g = Gpu { default: 200.0, min: 100.0, max: 216.0, ..Default::default() };
        assert_eq!(preset_watts(&g, 0.8), 160.0);
        assert_eq!(preset_watts(&g, 0.7), 140.0);
        assert_eq!(preset_watts(&g, 0.4), 100.0);
        let small = Gpu { default: 70.0, min: 40.0, max: 75.0, ..Default::default() };
        assert_eq!(preset_watts(&small, 0.8), 56.0);
        assert_eq!(fps_loss(1.0), 0.0);
        assert!((fps_loss(0.8) - 4.0).abs() < 1e-4);
        assert!((fps_loss(0.75) - 6.5).abs() < 1e-4);
    }

    #[test]
    fn cpu_estimate() {
        let l = CpuLimits { pl1: Some(65.0), pl2: Some(148.0), tau: Some(28.0) };
        assert!(cpu_power_estimate(0.0, 0.2, &l) < 12.0);
        let game = cpu_power_estimate(0.35, 1.0, &l);
        assert!(game > 40.0 && game < 70.0, "{game}");
        assert!(cpu_power_estimate(1.0, 1.0, &l) <= 148.0);
        // PL2 « illimité » : bornée à 2,5 × PL1.
        let unl = CpuLimits { pl1: Some(125.0), pl2: Some(4095.0), tau: None };
        assert!(cpu_power_estimate(1.0, 1.0, &unl) <= 312.5);
        // Profil ASUS : PL1 253 W, PL2 90 W → jamais plus de 90 W.
        let asus = CpuLimits { pl1: Some(253.0), pl2: Some(90.0), tau: Some(16.0) };
        assert!(cpu_power_estimate(1.0, 1.0, &asus) <= 90.0);
    }

    #[test]
    fn scripts_are_valid_shell() {
        for s in [PERSIST_SCRIPT, REMOVE_SCRIPT] {
            let ok = Command::new("sh").args(["-n", "-c", s]).status().map(|s| s.success()).unwrap_or(true);
            assert!(ok);
        }
    }
}

fn main() -> eframe::Result {
    let icon = image::load_from_memory(include_bytes!("../resources/icon.png")).ok().map(|i| {
        let i = i.to_rgba8();
        let (w, h) = i.dimensions();
        egui::IconData { rgba: i.into_raw(), width: w, height: h }
    });
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("PowerCap").with_app_id("powercap").with_inner_size([640.0, if std::env::var_os("APP_SCREENSHOT").is_some() { 1400.0 } else { 900.0 }]).with_min_inner_size([560.0, 600.0]);
    if let Some(i) = icon { viewport = viewport.with_icon(i); }
    let options = eframe::NativeOptions { viewport, renderer: eframe::Renderer::Glow, vsync: true, ..Default::default() };
    eframe::run_native("PowerCap", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}
