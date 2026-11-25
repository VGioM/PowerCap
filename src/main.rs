//! PowerCap : règle la limite de puissance (en watts) des cartes graphiques
//! NVIDIA sous Linux, et montre en direct ce que consomment la carte, le
//! processeur (estimation, avec ses limites PL1/PL2) et le PC complet, ainsi
//! que la mémoire vive (occupation, fréquence, XMP). Il permet aussi un
//! overclocking encadré de la carte (décalages de fréquence par NVML). Les
//! réglages s'appliquent tout de suite et, si on le demande, à chaque démarrage
//! et après chaque mise en veille grâce à de petits services systemd par carte
//! (/etc/systemd/system/powercap-gpu-<id>.service, powercap-oc-<id>.service).
//! Les actions qui demandent les droits administrateur passent par pkexec.

use eframe::egui::{self, Color32, CornerRadius, Margin, RichText, Stroke};
use std::collections::HashMap;
use std::process::Command;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

mod nvml;
mod selfupdate;
use selfupdate::SelfUpdate;

const UNIT_DIR: &str = "/etc/systemd/system";
const UNIT_PREFIX: &str = "powercap-gpu";
/// Préfixe des services créés par les versions 1.0.0 et 1.0.1 (alors appelées Watts).
const LEGACY_PREFIX: &str = "watts-gpu-power";
const OC_PREFIX: &str = "powercap-oc";
const PYTHON: &str = "/usr/bin/python3";

/// Règle les décalages de fréquence d'une carte par NVML (lancé en administrateur).
/// Arguments : UUID de la carte, décalage du processeur graphique, de la mémoire (MHz).
/// La carte garde sa tension et ses limites de température : seul le décalage change.
const OC_PY: &str = r#"import ctypes as C, re, sys
uuid, core, mem = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
if not re.fullmatch(r"GPU-[A-Za-z0-9-]+", uuid): sys.exit("carte invalide")
if not (0 <= core <= 1000 and 0 <= mem <= 6000): sys.exit("valeur invalide")
n = C.CDLL("libnvidia-ml.so.1")
n.nvmlErrorString.restype = C.c_char_p
def ok(r, what):
    if r != 0: sys.exit(what + " : " + n.nvmlErrorString(r).decode())
ok(n.nvmlInit_v2(), "NVML")
h = C.c_void_p()
ok(n.nvmlDeviceGetHandleByUUID(uuid.encode(), C.byref(h)), "carte introuvable")
ok(n.nvmlDeviceSetGpcClkVfOffset(h, core), "fréquence du processeur graphique")
ok(n.nvmlDeviceSetMemClkVfOffset(h, mem), "fréquence de la mémoire vidéo")
"#;

/// Installe le script Python et le service qui remet l'overclocking au démarrage
/// et après la veille. `$1` = UUID, `$2` = décalage graphique, `$3` = mémoire, `$4` = service, `$5` = script.
const OC_PERSIST_SCRIPT: &str = r#"set -e
case "$1" in GPU-*) ;; *) echo "carte invalide" >&2; exit 2;; esac
case "$1" in *[!A-Za-z0-9-]*) echo "carte invalide" >&2; exit 2;; esac
case "$2" in ''|*[!0-9]*) echo "valeur invalide" >&2; exit 2;; esac
case "$3" in ''|*[!0-9]*) echo "valeur invalide" >&2; exit 2;; esac
case "$4" in powercap-oc-*.service) ;; *) echo "service invalide" >&2; exit 2;; esac
case "$4" in */*|*[!A-Za-z0-9.-]*) echo "service invalide" >&2; exit 2;; esac
mkdir -p /etc/powercap
printf '%s' "$5" > /etc/powercap/gpu-oc.py
chmod 644 /etc/powercap/gpu-oc.py
cat > "/etc/systemd/system/$4" <<UNIT
# Créé par PowerCap (https://github.com/VGioM/PowerCap) : overclocking d'une
# carte graphique NVIDIA, remis au démarrage et après chaque veille.
# Pour l'enlever : PowerCap, « Retirer », ou systemctl disable --now $4
[Unit]
Description=Overclocking NVIDIA $1 (+$2 MHz graphique, +$3 MHz mémoire, réglé avec PowerCap)
After=nvidia-persistenced.service display-manager.service suspend.target hibernate.target hybrid-sleep.target suspend-then-hibernate.target

[Service]
Type=oneshot
ExecStart=/usr/bin/python3 -I /etc/powercap/gpu-oc.py $1 $2 $3

[Install]
WantedBy=graphical.target suspend.target hibernate.target hybrid-sleep.target suspend-then-hibernate.target
UNIT
chmod 644 "/etc/systemd/system/$4"
systemctl daemon-reload
systemctl reenable "$4"
/usr/bin/python3 -I /etc/powercap/gpu-oc.py "$1" "$2" "$3"
"#;

/// Retire le service d'overclocking et remet les fréquences d'origine.
/// `$1` = UUID, `$2` = service, `$3` = script Python.
const OC_REMOVE_SCRIPT: &str = r#"set -e
case "$1" in GPU-*) ;; *) echo "carte invalide" >&2; exit 2;; esac
case "$1" in *[!A-Za-z0-9-]*) echo "carte invalide" >&2; exit 2;; esac
case "$2" in powercap-oc-*.service) ;; *) echo "service invalide" >&2; exit 2;; esac
case "$2" in */*|*[!A-Za-z0-9.-]*) echo "service invalide" >&2; exit 2;; esac
systemctl disable "$2" >/dev/null 2>&1 || true
rm -f "/etc/systemd/system/$2"
systemctl daemon-reload
if ! ls /etc/systemd/system/powercap-oc-*.service >/dev/null 2>&1; then rm -f /etc/powercap/gpu-oc.py; rmdir /etc/powercap 2>/dev/null || true; fi
/usr/bin/python3 -I -c "$3" "$1" 0 0
"#;

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
    temp: f32, fan: f32, clock: f32, mem_clock: f32, util: f32,
    /// Mémoire vidéo utilisée et totale, en Mio.
    vram_used: f32, vram_total: f32,
}

impl Gpu {
    /// Les cartes de PC portables (et certaines anciennes) refusent le réglage.
    fn adjustable(&self) -> bool { self.default > 0.0 && self.min > 0.0 && self.max > self.min }
    /// Nom du service systemd propre à cette carte (stable : tiré de l'UUID).
    fn unit(&self) -> String { format!("{UNIT_PREFIX}-{}.service", self.id()) }
    /// Service d'overclocking de cette carte.
    fn oc_unit(&self) -> String { format!("{OC_PREFIX}-{}.service", self.id()) }
    fn id(&self) -> String {
        let id: String = self.uuid.trim_start_matches("GPU-").chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect();
        if id.is_empty() { self.index.to_string() } else { id.to_lowercase() }
    }
}

fn num(s: &str) -> f32 { s.trim().parse().unwrap_or(0.0) }

const QUERY: &str = "--query-gpu=index,uuid,name,power.draw,power.limit,power.default_limit,power.min_limit,power.max_limit,temperature.gpu,fan.speed,clocks.sm,clocks.mem,utilization.gpu,memory.used,memory.total";

fn parse_gpu(line: &str) -> Result<Gpu, String> {
    let f: Vec<&str> = line.split(',').map(str::trim).collect();
    // Le nom peut, en théorie, contenir des virgules : les 12 derniers champs sont numériques.
    if f.len() < 15 { return Err(format!("Réponse inattendue de nvidia-smi : {line}")); }
    let v: Vec<f32> = f[f.len() - 12..].iter().map(|x| num(x)).collect();
    Ok(Gpu {
        index: f[0].parse().unwrap_or(0), uuid: f[1].to_string(), name: f[2..f.len() - 12].join(", "),
        draw: v[0], limit: v[1], default: v[2], min: v[3], max: v[4],
        temp: v[5], fan: v[6], clock: v[7], mem_clock: v[8], util: v[9], vram_used: v[10], vram_total: v[11],
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

// ---------------------------------------------------------------- mémoire vive

/// Barrettes installées, d'après la table DMI du BIOS (que udev rend lisible
/// sans droits administrateur). Vitesses en MT/s (« DDR5-6000 » = 6000).
#[derive(Clone, Default, Debug, PartialEq)]
struct RamKit {
    kind: String, modules: u32, module_gb: f32,
    /// Vitesse max annoncée par le BIOS, vitesse réglée, vitesse inscrite dans la référence.
    speed: u32, configured: u32, rated: u32,
    /// Tension maximale des barrettes (celle du profil XMP), en V.
    voltage: f32,
    maker: String, part: String,
}

impl RamKit {
    /// Vitesse la plus haute que les barrettes peuvent tenir, d'après ce qu'on sait.
    fn max(&self) -> u32 { self.speed.max(self.rated).max(self.configured) }
}

/// Vitesse maximale sans profil XMP/EXPO (norme JEDEC des processeurs grand public).
fn jedec_max(kind: &str) -> Option<u32> {
    match kind { "DDR3" => Some(1600), "DDR4" => Some(3200), "DDR5" => Some(5600), _ => None }
}

/// Vitesse inscrite dans la référence du fabricant, repérée par « <vitesse>C<latence> »
/// (ou J chez G.Skill) : CP16G60C36 → 6000, KF560C36 → 6000, CMK32GX5M2B6000C36 → 6000, F5-6000J3038 → 6000.
fn rated_from_part(part: &str) -> u32 {
    let b = part.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() { i += 1; continue; }
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() { i += 1; }
        let digits = &part[start..i];
        let cl = i + 1 < b.len() && matches!(b[i], b'C' | b'J') && b[i + 1].is_ascii_digit();
        if !cl { continue; }
        let mts = match digits.len() {
            2 => digits.parse::<u32>().unwrap_or(0) * 100,
            3 => digits[1..].parse::<u32>().unwrap_or(0) * 100,
            4 => digits.parse().unwrap_or(0),
            _ => 0,
        };
        if (1600..=10000).contains(&mts) { return mts; }
    }
    0
}

/// Lit les propriétés MEMORY_DEVICE_* (format udev « E:CLÉ=VALEUR » ou « CLÉ=VALEUR »).
fn parse_ram_kit(text: &str) -> RamKit {
    let mut dev: HashMap<u32, HashMap<String, String>> = HashMap::new();
    for l in text.lines() {
        let l = l.trim().trim_start_matches("E:");
        let Some(rest) = l.strip_prefix("MEMORY_DEVICE_") else { continue };
        let Some((key, value)) = rest.split_once('=') else { continue };
        let Some((n, field)) = key.split_once('_') else { continue };
        let Ok(n) = n.parse() else { continue };
        dev.entry(n).or_default().insert(field.to_string(), value.trim().to_string());
    }
    let mut kit = RamKit::default();
    let mut ids: Vec<_> = dev.keys().copied().collect();
    ids.sort();
    for id in ids {
        let d = &dev[&id];
        let get = |k: &str| d.get(k).map(String::as_str).unwrap_or("");
        let size: f64 = get("SIZE").parse().unwrap_or(0.0);
        if size <= 0.0 { continue; } // emplacement vide
        kit.modules += 1;
        kit.module_gb = (size / 1073741824.0) as f32;
        if kit.kind.is_empty() { kit.kind = get("TYPE").to_string(); }
        let speed: u32 = get("SPEED_MTS").parse().unwrap_or(0);
        let conf: u32 = get("CONFIGURED_SPEED_MTS").parse().unwrap_or(0);
        kit.speed = kit.speed.max(speed);
        // Plusieurs barrettes tournent toujours à la même vitesse : la plus basse fait foi.
        if conf > 0 { kit.configured = if kit.configured == 0 { conf } else { kit.configured.min(conf) }; }
        kit.voltage = kit.voltage.max(get("MAXIMUM_VOLTAGE").parse().unwrap_or(0.0));
        if kit.part.is_empty() { kit.part = get("PART_NUMBER").to_string(); }
        if kit.maker.is_empty() { kit.maker = get("MANUFACTURER").to_string(); }
    }
    kit.rated = rated_from_part(&kit.part);
    kit
}

fn ram_kit() -> RamKit {
    let text = std::fs::read_to_string("/run/udev/data/+dmi:id").ok().filter(|t| t.contains("MEMORY_DEVICE_")).unwrap_or_else(|| {
        Command::new("udevadm").args(["info", "--query=property", "--path=/sys/class/dmi/id"]).output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default()
    });
    parse_ram_kit(&text)
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Xmp {
    /// Au-dessus de la norme JEDEC : un profil XMP (Intel) ou EXPO (AMD) est actif.
    On,
    /// Les barrettes peuvent aller plus vite que leur réglage actuel.
    Off,
    /// Vitesse standard, et rien n'indique que les barrettes peuvent faire mieux.
    Standard,
    Unknown,
}

fn xmp_state(k: &RamKit) -> Xmp {
    if k.configured == 0 { return Xmp::Unknown; }
    if k.max() > k.configured + 50 { return Xmp::Off; }
    match jedec_max(&k.kind) {
        Some(j) if k.configured > j => Xmp::On,
        Some(_) => Xmp::Standard,
        None => Xmp::Unknown,
    }
}

/// Tension de travail des barrettes : la norme, ou celle du profil XMP s'il est actif.
fn ram_voltage(k: &RamKit) -> Option<f32> {
    let nominal = match k.kind.as_str() { "DDR5" => 1.1, "DDR4" => 1.2, "DDR3" => 1.5, _ => return None };
    Some(if xmp_state(k) == Xmp::On && k.voltage > nominal { k.voltage } else { nominal })
}

/// Consommation estimée de la mémoire vive, en W. Les barrettes n'ont pas de
/// capteur de puissance : on part d'environ 1,2 W par barrette DDR5 au repos et
/// 4 W en pleine activité (DDR4 : 1 à 3 W), selon la charge du processeur, la
/// taille des barrettes et le carré de la tension (1,35 V en XMP ≈ +50 %).
fn ram_power_estimate(k: &RamKit, util: f32) -> f32 {
    if k.modules == 0 { return 0.0; }
    let (idle, load, nominal) = match k.kind.as_str() { "DDR5" => (1.2, 4.0, 1.1), "DDR3" => (1.0, 3.0, 1.5), _ => (1.0, 3.0, 1.2) };
    let volt = ram_voltage(k).map(|v| (v / nominal).powi(2)).unwrap_or(1.0);
    let size = (k.module_gb / 16.0).clamp(0.5, 2.0).sqrt();
    k.modules as f32 * (idle + (load - idle) * util.clamp(0.0, 1.0)) * volt * size
}

/// Mémoire utilisée et totale, en Go (Gio), d'après /proc/meminfo.
fn ram_usage() -> Option<(f32, f32)> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb = |key: &str| info.lines().find_map(|l| l.strip_prefix(key)).and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<f32>().ok());
    let (total, avail) = (kb("MemTotal:")?, kb("MemAvailable:")?);
    Some(((total - avail) / 1048576.0, total / 1048576.0))
}

/// Température des barrettes (capteur des DDR5, ou jc42 sur certaines DDR4), en °C.
fn ram_temp() -> Option<f32> {
    std::fs::read_dir("/sys/class/hwmon").ok()?.flatten().filter_map(|e| {
        let name = std::fs::read_to_string(e.path().join("name")).ok()?;
        if !matches!(name.trim(), "spd5118" | "jc42") { return None; }
        read_f(&e.path().join("temp1_input")).map(|v| v / 1000.0)
    }).reduce(f32::max)
}

/// Températures des SSD NVMe, de la carte mère, et vitesse des ventilateurs.
#[derive(Clone, Default, Debug, PartialEq)]
struct Board { ssd: Vec<f32>, chipset: Option<f32>, board: Option<f32>, fans: Vec<u32> }

fn board_sensors() -> Board {
    let mut b = Board::default();
    let Ok(dir) = std::fs::read_dir("/sys/class/hwmon") else { return b };
    let mut dirs: Vec<_> = dir.flatten().map(|e| e.path()).collect();
    dirs.sort();
    for p in dirs {
        let name = std::fs::read_to_string(p.join("name")).unwrap_or_default().trim().to_string();
        // Les disques durs SATA (drivetemp) sont laissés de côté : les lire peut les réveiller.
        if name == "nvme" {
            if let Some(t) = read_f(&p.join("temp1_input")) { b.ssd.push(t / 1000.0); }
            continue;
        }
        if matches!(name.as_str(), "amdgpu" | "radeon" | "nouveau" | "coretemp" | "k10temp" | "spd5118" | "jc42") || name.starts_with("hidpp") { continue; }
        for i in 1..=32 {
            let Some(t) = read_f(&p.join(format!("temp{i}_input"))).map(|v| v / 1000.0).filter(|t| (5.0..=125.0).contains(t)) else { continue };
            match std::fs::read_to_string(p.join(format!("temp{i}_label"))).unwrap_or_default().trim() {
                "PCH_CHIP_TEMP" | "Chipset" | "PCH" => { b.chipset.get_or_insert(t); }
                "SYSTIN" | "Motherboard" => { b.board.get_or_insert(t); }
                _ => {}
            }
        }
        for i in 1..=16 {
            if let Some(r) = read_f(&p.join(format!("fan{i}_input"))).filter(|r| *r > 0.0 && *r < 20000.0) { b.fans.push(r as u32); }
        }
    }
    b
}

/// Le reste du PC (carte mère, SSD, ventilateurs, USB), en W, et le
/// rendement typique d'une alimentation 80 Plus Gold à charge moyenne.
const REST_W: f32 = 30.0;
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
    ram: RamKit,
    /// Mémoire utilisée / totale (Go), température des barrettes.
    ram_used: Option<(f32, f32)>,
    ram_temp: Option<f32>,
    ram_temp_max: f32,
    ram_w: f32,
    ram_w_max: f32,
    board: Board,
}

impl Live {
    /// Consommation du PC complet à la prise (hors écran), en W.
    fn total(&self) -> f32 {
        let gpus: f32 = self.gpus.as_ref().map(|l| l.iter().map(|g| g.draw).sum()).unwrap_or(0.0);
        (gpus + self.cpu.watts + self.ram_w + REST_W) / PSU_EFFICIENCY
    }
}

fn spawn_monitor(ctx: egui::Context) -> Arc<Mutex<Live>> {
    let live = Arc::new(Mutex::new(Live { gpus: Err("Lecture des cartes graphiques…".into()), cpu: Cpu { name: cpu_name(), ..Default::default() },
        cpu_max: 0.0, cpu_w_max: 0.0, total_max: 0.0, peaks: HashMap::new(), ram: ram_kit(), ram_used: ram_usage(), ram_temp: None, ram_temp_max: 0.0,
        ram_w: 0.0, ram_w_max: 0.0, board: Board::default() }));
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
        let (ram_used, ram_t, board) = (ram_usage(), ram_temp(), board_sensors());
        if let Ok(mut l) = shared.lock() {
            l.ram_used = ram_used;
            l.ram_temp = ram_t;
            if let Some(t) = ram_t { l.ram_temp_max = l.ram_temp_max.max(t); }
            l.ram_w = ram_power_estimate(&l.ram, util);
            l.ram_w_max = l.ram_w_max.max(l.ram_w);
            l.board = board;
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

/// Service d'overclocking : « ExecStart=… gpu-oc.py <UUID> <graphique> <mémoire> ».
#[derive(Clone, Debug, PartialEq)]
struct OcUnit { file: String, uuid: String, core: i32, mem: i32, enabled: bool }

fn parse_oc_unit(text: &str) -> Option<(String, i32, i32)> {
    text.lines().find_map(|l| {
        let words: Vec<&str> = l.trim().strip_prefix("ExecStart=")?.split_whitespace().collect();
        let i = words.iter().position(|w| w.ends_with("gpu-oc.py"))?;
        Some((words.get(i + 1)?.to_string(), words.get(i + 2)?.parse().ok()?, words.get(i + 3)?.parse().ok()?))
    })
}

fn read_oc_units() -> Vec<OcUnit> {
    let Ok(dir) = std::fs::read_dir(UNIT_DIR) else { return vec![] };
    dir.flatten().filter_map(|e| {
        let file = e.file_name().to_string_lossy().to_string();
        if !file.starts_with(OC_PREFIX) || !file.ends_with(".service") { return None; }
        let (uuid, core, mem) = parse_oc_unit(&std::fs::read_to_string(e.path()).ok()?)?;
        let enabled = Command::new("systemctl").args(["is-enabled", &file]).output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "enabled").unwrap_or(false);
        Some(OcUnit { file, uuid, core, mem, enabled })
    }).collect()
}

#[derive(Clone)]
enum Job {
    Apply { w: u32, uuid: String }, Persist { w: u32, uuid: String, unit: String }, Remove { w: u32, uuid: String, unit: String },
    Oc { uuid: String, core: i32, mem: i32 }, OcPersist { uuid: String, core: i32, mem: i32, unit: String }, OcRemove { uuid: String, unit: String },
}

fn run_job(job: &Job) -> Result<String, String> {
    if Command::new("pkexec").arg("--version").output().is_err() {
        return Err("pkexec est introuvable : impossible de demander les droits administrateur.".into());
    }
    if matches!(job, Job::Oc { .. } | Job::OcPersist { .. } | Job::OcRemove { .. }) && !std::path::Path::new(PYTHON).exists() {
        return Err(format!("{PYTHON} est introuvable : il sert à régler les fréquences de la carte."));
    }
    let mut cmd = Command::new("pkexec");
    match job {
        Job::Apply { w, uuid } => cmd.args([nvsmi(), "-i".into(), uuid.clone(), "-pl".into(), w.to_string()]),
        Job::Persist { w, uuid, unit } => cmd.args(["/bin/sh", "-c", PERSIST_SCRIPT, "sh", &w.to_string(), uuid, unit]),
        Job::Remove { w, uuid, unit } => cmd.args(["/bin/sh", "-c", REMOVE_SCRIPT, "sh", &w.to_string(), uuid, unit]),
        Job::Oc { uuid, core, mem } => cmd.args([PYTHON, "-I", "-c", OC_PY, uuid, &core.to_string(), &mem.to_string()]),
        Job::OcPersist { uuid, core, mem, unit } => cmd.args(["/bin/sh", "-c", OC_PERSIST_SCRIPT, "sh", uuid, &core.to_string(), &mem.to_string(), unit, OC_PY]),
        Job::OcRemove { uuid, unit } => cmd.args(["/bin/sh", "-c", OC_REMOVE_SCRIPT, "sh", uuid, unit, OC_PY]),
    };
    let out = cmd.output().map_err(|e| format!("Impossible de lancer pkexec ({e})."))?;
    match out.status.code() {
        Some(0) => Ok(match job {
            Job::Apply { w, .. } => format!("Limite réglée à {w} W jusqu'au prochain redémarrage."),
            Job::Persist { w, .. } => format!("Limite réglée à {w} W, et gardée au démarrage et après la veille."),
            Job::Remove { w, .. } => format!("Réglage permanent retiré, carte revenue à {w} W."),
            Job::Oc { core: 0, mem: 0, .. } => "Fréquences d'origine remises.".into(),
            Job::Oc { core, mem, .. } => format!("Overclocking appliqué (+{core} MHz graphique, +{mem} MHz mémoire) jusqu'au prochain redémarrage. Teste en jouant !"),
            Job::OcPersist { core, mem, .. } => format!("Overclocking appliqué (+{core} / +{mem} MHz), gardé au démarrage et après la veille."),
            Job::OcRemove { .. } => "Overclocking retiré : fréquences d'origine.".into(),
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

/// Paliers d'overclocking : (prudent, courant, élevé), en MHz de décalage.
/// Au-delà du palier « élevé », presque toutes les cartes deviennent instables.
#[derive(Clone, Copy, Debug, PartialEq)]
struct OcSteps { core: [i32; 3], mem: [i32; 3] }

fn oc_steps(name: &str) -> OcSteps {
    // Les RTX 50 gagnent plus de MHz que les RTX 30/40 avant de décrocher.
    let core = if name.contains("RTX 50") { [150, 300, 400] } else { [100, 200, 300] };
    OcSteps { core, mem: [1000, 2000, 3000] }
}

#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
enum Risk { Origin, Safe, Typical, High, Unstable }

fn risk(v: i32, steps: [i32; 3]) -> Risk {
    if v <= 0 { Risk::Origin } else if v <= steps[0] { Risk::Safe } else if v <= steps[1] { Risk::Typical } else if v <= steps[2] { Risk::High } else { Risk::Unstable }
}

/// Overclockings conseillés (prudent, courant, élevé) pour une limite de
/// `ratio` × l'origine. Le processeur graphique garde les mêmes paliers : sa
/// stabilité se joue en haut de sa courbe, atteinte dans les menus et les jeux
/// légers quelle que soit la limite. La mémoire vidéo, elle, prend ses watts
/// au processeur graphique : plus la limite est basse, moins elle doit monter.
fn oc_plan(s: &OcSteps, ratio: f32) -> [(i32, i32); 3] {
    let mem = useful_mem(s, ratio);
    [0, 1, 2].map(|i| (s.core[i], s.mem[i].min([mem / 2, mem * 3 / 4, mem][i] / 250 * 250)))
}

/// Au-delà de ce décalage mémoire, le gain d'images/s est nul à cette limite.
fn useful_mem(s: &OcSteps, ratio: f32) -> i32 {
    if ratio < 0.7 { 1500 } else if ratio < 0.9 { 2250 } else { s.mem[2] }
}

/// Gain d'images/s estimé (en %) d'un overclocking à pleine charge. Avec une
/// limite basse, le décalage agit comme un sous-voltage : mêmes watts, plus de
/// MHz, donc il rapporte plus qu'à la puissance d'origine.
fn oc_gain(s: &OcSteps, core: i32, mem: i32, ratio: f32) -> f32 {
    let t = ((ratio - 0.7) / 0.3).clamp(0.0, 1.0);
    let per_100 = 3.5 + (2.0 - 3.5) * t;
    let per_100 = if s.core[0] > 100 { per_100 * 0.8 } else { per_100 };
    let core = core.clamp(0, s.core[2]) as f32 / 100.0 * per_100;
    let mem_per_1000 = if ratio < 0.9 { 0.7 } else { 1.5 };
    let mem = mem.clamp(0, useful_mem(s, ratio).min(s.mem[2])) as f32 / 1000.0 * mem_per_1000;
    core + mem
}

/// Le curseur va un peu au-delà du palier « élevé » (zone rouge), sans dépasser ce que le pilote permet.
fn oc_caps(s: &OcSteps, o: &nvml::Offsets) -> (i32, i32) {
    ((s.core[2] + 100).min(o.core_range.1).max(0), (s.mem[2] + 500).min(o.mem_range.1).max(0))
}

// ---------------------------------------------------------------- interface

struct App {
    live: Arc<Mutex<Live>>,
    units: Vec<Unit>,
    selected: String,
    target: Option<f32>,
    job: Option<mpsc::Receiver<Result<String, String>>>,
    msg: Option<Result<String, String>>,
    oc_units: Vec<OcUnit>,
    /// Décalages lus sur la carte sélectionnée (UUID, résultat).
    oc_now: Option<(String, Result<nvml::Offsets, String>)>,
    /// Réglage en cours sur les curseurs : (graphique, mémoire).
    oc_target: Option<(i32, i32)>,
    hook: TestHook,
    update: SelfUpdate,
}

impl App {
    fn new(cc: &eframe::CreationContext) -> Self {
        style(&cc.egui_ctx);
        App { live: spawn_monitor(cc.egui_ctx.clone()), units: read_units(), selected: String::new(), target: None, job: None, msg: None, oc_units: read_oc_units(), oc_now: None, oc_target: None, hook: TestHook::default(), update: SelfUpdate::new() }
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

/// Tuiles rangées par 3 ; seules celles qui ont une mesure sont affichées.
fn tiles(ui: &mut egui::Ui, items: &[(&str, String, String, Color32)]) {
    for (r, row) in items.chunks(3).enumerate() {
        if r > 0 { ui.add_space(2.0); }
        ui.columns(3, |c| {
            for (i, (label, value, sub, color)) in row.iter().enumerate() { tile(&mut c[i], label, value.clone(), sub.clone(), *color); }
        });
    }
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
                self.oc_units = read_oc_units();
                self.oc_now = None;
            }
        }
        let live = self.live.lock().map(|l| l.clone()).unwrap_or_else(|e| e.into_inner().clone());
        let gpus = live.gpus.as_ref().ok().cloned().unwrap_or_default();
        if !gpus.iter().any(|g| g.uuid == self.selected) {
            if let Some(g) = gpus.first() { self.selected = g.uuid.clone(); self.target = None; self.oc_target = None; }
        }
        let gpu = gpus.iter().find(|g| g.uuid == self.selected).cloned();
        if let (None, Some(g)) = (self.target, &gpu) {
            let saved = unit_for(&self.units, g).map(|u| u.watts as f32);
            self.target = Some(saved.unwrap_or(g.limit).clamp(g.min.min(g.limit), g.max.max(g.limit)));
        }
        if let Some(g) = &gpu {
            if self.oc_now.as_ref().map_or(true, |(u, _)| *u != g.uuid) { self.oc_now = Some((g.uuid.clone(), nvml::offsets(&g.uuid))); }
        }
        let busy = self.job.is_some();

        egui::CentralPanel::default().frame(egui::Frame::new().fill(BG).inner_margin(Margin::same(18))).show(ctx, |ui| {
            let mut scroll = egui::ScrollArea::vertical().auto_shrink(false);
            if let Some(y) = self.hook.scroll.filter(|_| self.hook.path.is_some()) { scroll = scroll.vertical_scroll_offset(y); }
            scroll.show(ui, |ui| {
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
                                        self.oc_target = None;
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
                    let ram = live.ram.modules > 0;
                    ui.columns(if ram { 4 } else { 3 }, |c| {
                        tile(&mut c[0], "Carte graphique", format!("{:.0} W", g.draw), format!("limite {:.0} W\nmax vu {:.0} W", g.limit, peaks.draw), TEXT);
                        tile(&mut c[1], "Processeur", format!("≈ {:.0} W", cpu.watts), format!("charge {:.0} %\nmax vu {:.0} W", cpu.util * 100.0, live.cpu_w_max), TEXT);
                        if ram {
                            tile(&mut c[2], "Mémoire vive", format!("≈ {:.0} W", live.ram_w), format!("{} barrettes\nmax vu {:.0} W", live.ram.modules, live.ram_w_max), TEXT);
                        }
                        tile(&mut c[if ram { 3 } else { 2 }], "PC complet", format!("≈ {:.0} W", live.total()), format!("à la prise\nmax vu {:.0} W", live.total_max), ACCENT);
                    });
                    let frac = if g.limit > 0.0 { (g.draw / g.limit).clamp(0.0, 1.0) } else { 0.0 };
                    ui.add(egui::ProgressBar::new(frac).desired_height(10.0).fill(if frac > 0.95 { WARM } else { ACCENT }).corner_radius(CornerRadius::same(5)));
                    small(ui, format!("La barre montre la carte graphique par rapport à sa limite. Carte mesurée par le pilote ; « ≈ » = estimé (processeur et mémoire n'ont pas de capteur de watts lisible). PC complet : tout ça + ≈ {REST_W:.0} W pour le reste (carte mère, SSD, ventilateurs), avec une alimentation à ≈ {:.0} % de rendement ; écran non compris.", PSU_EFFICIENCY * 100.0));
                });
                ui.add_space(10.0);
                card(ui, "Températures", |ui| {
                    let b = &live.board;
                    let mut items = vec![("Carte graphique", format!("{:.0} °C", g.temp), format!("max vu {:.0} °C · ventilo {:.0} %", peaks.temp, g.fan), temp_color(g.temp, 75.0, 83.0))];
                    items.push(match cpu.temp {
                        Some(t) => ("Processeur", format!("{t:.0} °C"), format!("max vu {:.0} °C", live.cpu_max), temp_color(t, 85.0, 95.0)),
                        None => ("Processeur", "—".into(), "capteur introuvable".into(), MUTED),
                    });
                    if let Some(t) = live.ram_temp {
                        items.push(("Mémoire vive", format!("{t:.0} °C"), format!("max vu {:.0} °C", live.ram_temp_max), temp_color(t, 60.0, 75.0)));
                    }
                    if let Some(t) = b.ssd.iter().copied().reduce(f32::max) {
                        let sub = if b.ssd.len() > 1 { format!("le plus chaud des {} SSD", b.ssd.len()) } else { "SSD NVMe".into() };
                        items.push(("Stockage", format!("{t:.0} °C"), sub, temp_color(t, 60.0, 70.0)));
                    }
                    if let Some(t) = b.chipset.or(b.board) {
                        let sub = match (b.chipset, b.board) { (Some(_), Some(m)) => format!("chipset · capteur carte {m:.0} °C"), (Some(_), None) => "chipset".into(), _ => "capteur de la carte".into() };
                        items.push(("Carte mère", format!("{t:.0} °C"), sub, temp_color(t, 75.0, 90.0)));
                    }
                    if !b.fans.is_empty() {
                        let value = if b.fans.len() <= 3 { b.fans.iter().map(u32::to_string).collect::<Vec<_>>().join(" · ") } else { format!("{}–{}", b.fans.iter().min().unwrap(), b.fans.iter().max().unwrap()) };
                        items.push(("Ventilateurs", value, format!("tr/min · {} en marche", b.fans.len()), TEXT));
                    }
                    tiles(ui, &items);
                    small(ui, "Orange : chaud, rouge : très chaud. Les « max vu » comptent depuis l'ouverture de PowerCap : laisse-le ouvert pendant une partie pour voir les pics.");
                });
                ui.add_space(10.0);
                card(ui, "Fréquences et charge", |ui| {
                    let mut items = vec![
                        ("Processeur", format!("{:.1} GHz", cpu.freq), format!("max {:.1} GHz · charge {:.0} %", cpu.max_freq, cpu.util * 100.0), TEXT),
                        ("Carte graphique", format!("{:.0} MHz", g.clock), format!("charge {:.0} %", g.util), TEXT),
                    ];
                    if g.vram_total > 0.0 {
                        let frac = g.vram_used / g.vram_total;
                        items.push(("Mémoire vidéo", format!("{:.1} / {:.0} Go", g.vram_used / 1024.0, g.vram_total / 1024.0), format!("{:.0} % occupée", frac * 100.0), temp_color(frac, 0.85, 0.95)));
                    }
                    tiles(ui, &items);
                    small(ui, "Au repos, fréquences et charge sont basses : c'est normal, elles montent en jeu. Mémoire vidéo presque pleine : baisse la qualité des textures.");
                });
                ui.add_space(10.0);

                // --- Mémoire vive
                card(ui, "Mémoire vive", |ui| {
                    let k = &live.ram;
                    let xmp = xmp_state(k);
                    let kit = if k.modules > 0 { format!("{} × {:.0} Go {}", k.modules, k.module_gb, k.kind) } else { "barrettes non détectées".into() };
                    ui.columns(3, |c| {
                        match live.ram_used {
                            Some((used, total)) => tile(&mut c[0], "Utilisée", format!("{used:.1} / {total:.1} Go"), format!("{:.0} % · {kit}", used / total * 100.0), temp_color(used / total, 0.80, 0.92)),
                            None => tile(&mut c[0], "Utilisée", "—".into(), kit, MUTED),
                        }
                        if k.configured > 0 {
                            tile(&mut c[1], "Fréquence", format!("{} MT/s", k.configured), format!("max {} MT/s", k.max()), if xmp == Xmp::Off { WARM } else { TEXT });
                        } else {
                            tile(&mut c[1], "Fréquence", "—".into(), "non lisible".into(), MUTED);
                        }
                        let (name, sub, color) = match xmp {
                            Xmp::On => ("XMP actif", "profil de la barrette", ACCENT),
                            Xmp::Off => ("XMP désactivé", "vitesse standard (JEDEC)", WARM),
                            Xmp::Standard => ("Standard", "JEDEC, sans XMP", TEXT),
                            Xmp::Unknown => ("—", "non déterminé", MUTED),
                        };
                        let sub = match ram_voltage(k) { Some(v) => format!("{sub} · {v:.2} V"), None => sub.into() };
                        tile(&mut c[2], "Profil", name.into(), sub, color);
                    });
                    match xmp {
                        Xmp::On => { ui.label(RichText::new(format!("La mémoire tourne à {} MT/s, au-dessus de la norme ({} MT/s) : le profil XMP (ou EXPO chez AMD) est bien activé dans le BIOS.", k.configured, jedec_max(&k.kind).unwrap_or(0))).color(ACCENT)); }
                        Xmp::Off => { ui.label(RichText::new(format!("La mémoire tourne à {} MT/s alors qu'elle est prévue pour {} MT/s : active le profil XMP dans le BIOS (ASUS : Ai Tweaker → Ai Overclock Tuner → XMP I ; AMD : EXPO).", k.configured, k.max())).color(WARM)); }
                        Xmp::Standard => small(ui, "Vitesse standard. Si la boîte de tes barrettes annonce plus (par ex. 6000 MT/s), active le profil XMP dans le BIOS."),
                        Xmp::Unknown => {}
                    }
                    let info = [k.maker.as_str(), k.part.as_str()].iter().filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("Not Specified")).copied().collect::<Vec<_>>().join(" ");
                    small(ui, format!("{}MT/s = mégatransferts par seconde, le chiffre des boîtes (DDR5-6000 = 6000 MT/s, soit 3000 MHz réels).", if info.is_empty() { String::new() } else { format!("{info}. ") }));
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

                // --- Overclocking
                card(ui, "Overclocking de la carte (facultatif)", |ui| {
                    let oc_unit = self.oc_units.iter().find(|u| u.uuid == g.uuid).cloned();
                    let now = match self.oc_now.as_ref().map(|(_, r)| r.clone()) {
                        Some(Ok(o)) => o,
                        Some(Err(e)) => {
                            ui.label(RichText::new("Overclocking indisponible sur cette carte.").strong().color(MUTED));
                            small(ui, e);
                            return;
                        }
                        None => return,
                    };
                    let active = now.core != 0 || now.mem != 0;
                    if active {
                        ui.label(RichText::new(format!("Actif : +{} MHz processeur graphique, +{} MHz mémoire vidéo", now.core, now.mem)).strong().color(ACCENT));
                    } else {
                        ui.label(RichText::new("Pas d'overclocking : la carte tourne à ses fréquences d'origine.").color(MUTED));
                    }
                    match &oc_unit {
                        Some(u) if u.enabled => small(ui, format!("Gardé au démarrage : +{} / +{} MHz.", u.core, u.mem)),
                        Some(u) => small(ui, format!("Réglage de démarrage présent (+{} / +{} MHz) mais désactivé.", u.core, u.mem)),
                        None if active => small(ui, "Seulement jusqu'au prochain redémarrage."),
                        None => {}
                    }

                    let steps = oc_steps(&g.name);
                    // Les conseils suivent le curseur de watts, même pas encore appliqué.
                    let watts = if g.adjustable() { self.target.unwrap_or(g.limit) } else { g.limit };
                    let ratio = if g.default > 0.0 { watts / g.default } else { 1.0 };
                    let plan = oc_plan(&steps, ratio);
                    let pending_w = (watts - g.limit).abs() >= 0.5;
                    let (core_cap, mem_cap) = oc_caps(&steps, &now);
                    let (mut core, mut mem) = self.oc_target.unwrap_or_else(|| oc_unit.as_ref().map(|u| (u.core, u.mem)).unwrap_or((now.core, now.mem)));
                    // Carte bridée en watts : l'overclocking ne chauffe pas plus, il récupère des images/s.
                    let limited = g.adjustable() && watts < g.default * 0.97;
                    egui::CollapsingHeader::new(RichText::new("Afficher les réglages").strong()).id_salt(("oc", &g.uuid)).default_open(active).show(ui, |ui| {
                        let boxed = |ui: &mut egui::Ui, add: &mut dyn FnMut(&mut egui::Ui)| {
                            egui::Frame::new().fill(CARD2).corner_radius(CornerRadius::same(10)).inner_margin(Margin::same(12)).show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                add(ui);
                            });
                        };
                        boxed(ui, &mut |ui| {
                            ui.label(RichText::new("À quoi ça sert ?").strong());
                            small(ui, "La carte fait tourner ses puces un peu plus vite que prévu à l'usine : quelques images/s en plus, gratuitement.");
                            if limited {
                                ui.label(RichText::new(format!("Ta carte est limitée à {:.0} W ({:.0} % de l'origine) : l'overclocking ne la fera pas chauffer plus, elle reste plafonnée à {:.0} W. Elle tire simplement plus d'images/s de ces mêmes watts, et rattrape une partie de ce que la limite fait perdre.", watts, ratio * 100.0, watts)).color(ACCENT));
                            } else {
                                ui.label(RichText::new(format!("Ta carte est à {:.0} W ({:.0} % de l'origine) : elle gagne quelques images/s et chauffe un peu plus, sans jamais dépasser {:.0} W.", watts, ratio * 100.0, watts)).color(TEXT));
                                if g.adjustable() {
                                    small(ui, format!("Astuce : limite « Équilibré » ({:.0} W) + overclocking « Prudent » = presque toutes les performances, avec moins de chaleur et de bruit.", preset_watts(&g, 0.8)));
                                }
                            }
                            if pending_w {
                                small(ui, format!("Conseils calculés pour {watts:.0} W, la valeur du curseur de limite ci-dessus (pas encore appliquée)."));
                            }
                            ui.add_space(4.0);
                            ui.label(RichText::new("Sans danger pour la carte").strong());
                            small(ui, "Elle garde sa tension maximale, sa limite de watts et sa protection contre la surchauffe : PowerCap n'y touche pas. Si on va trop loin, le jeu plante ou l'image se dérègle, rien de plus.");
                        });
                        ui.add_space(6.0);
                        boxed(ui, &mut |ui| {
                            ui.label(RichText::new("Comment faire").strong());
                            for (i, s) in [
                                "Clique sur « Prudent » ci-dessous.",
                                "Clique sur « Essayer maintenant » (mot de passe demandé).",
                                "Joue au moins 30 min à un jeu exigeant.",
                                "Aucun souci ? Clique sur « Garder au démarrage ». Un plantage ? Redémarre le PC : l'essai s'efface tout seul.",
                            ].iter().enumerate() {
                                small(ui, format!("{}. {s}", i + 1));
                            }
                        });
                        ui.add_space(8.0);
                        ui.spacing_mut().slider_width = ui.available_width();
                        let slider = |ui: &mut egui::Ui, label: &str, hint: &str, v: &mut i32, cap: i32, step: f64, steps: [i32; 3]| {
                            let color = match risk(*v, steps) { Risk::Origin => TEXT, Risk::Safe | Risk::Typical => ACCENT, Risk::High => WARM, Risk::Unstable => HOT };
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(label).strong());
                                ui.label(RichText::new(format!("+{v} MHz")).strong().color(color));
                            });
                            small(ui, hint);
                            ui.add(egui::Slider::new(v, 0..=cap).step_by(step).show_value(false));
                            ui.horizontal(|ui| {
                                small(ui, format!("vert jusqu'à +{}, orange jusqu'à +{}, rouge au-delà", steps[1], steps[2]));
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| small(ui, format!("+{cap}")));
                            });
                        };
                        slider(ui, "Processeur graphique", "Le cœur de la carte : c'est lui qui donne le plus d'images/s.", &mut core, core_cap, 5.0, steps.core);
                        ui.add_space(4.0);
                        let mem_hint = if limited { format!("Gain plus faible. Avec une limite de watts réduite, elle prend des watts au processeur graphique : à {watts:.0} W, inutile de dépasser +{}.", useful_mem(&steps, ratio).min(steps.mem[2])) } else { "Gain plus faible, surtout utile en haute résolution.".to_string() };
                        slider(ui, "Mémoire vidéo", &mem_hint, &mut mem, mem_cap, 50.0, steps.mem);
                        ui.add_space(4.0);
                        ui.label(RichText::new(format!("Conseillé pour {watts:.0} W")).strong());
                        let presets = [("D'origine", "aucun", 0usize), ("Prudent", "conseillé", 1), ("Courant", "à tester", 2), ("Élevé", "risqué", 3)];
                        ui.columns(presets.len(), |c| {
                            for (i, (name, hint, lvl)) in presets.iter().enumerate() {
                                let (pc, pm) = if *lvl == 0 { (0, 0) } else { plan[lvl - 1] };
                                let sel = core == pc && mem == pm;
                                let text = RichText::new(format!("{name}\n+{pc} / +{pm}\n{hint}")).size(13.0).color(if sel { ACCENT } else { TEXT });
                                let b = egui::Button::new(text).min_size(egui::vec2(c[i].available_width(), 62.0))
                                    .fill(if sel { ACCENT_DIM } else { CARD2 }).stroke(Stroke::new(1.0_f32, if sel { ACCENT } else { LINE }));
                                if c[i].add(b).clicked() { core = pc; mem = pm; }
                            }
                        });
                        ui.add_space(6.0);
                        let (rc, rm) = (risk(core, steps.core), risk(mem, steps.mem));
                        let worst = if rc > rm { rc } else { rm };
                        boxed(ui, &mut |ui| {
                            let (line, color) = match worst {
                                Risk::Origin => ("Fréquences d'origine : rien à tester.", TEXT),
                                Risk::Safe => ("Prudent : la quasi-totalité des cartes le tiennent.", ACCENT),
                                Risk::Typical => ("Courant : beaucoup de cartes le tiennent, mais chacune est différente. Teste bien avant de le garder.", ACCENT),
                                Risk::High => ("Élevé : beaucoup de cartes plantent ici (jeu qui se ferme, écran noir, petits carrés colorés). Sans danger pour la carte, mais à tester longuement.", WARM),
                                Risk::Unstable => ("Trop haut : le jeu ou le PC va presque sûrement planter. Ça n'abîme pas la carte, mais ce réglage ne peut pas être gardé au démarrage.", HOT),
                            };
                            ui.label(RichText::new(line).strong().color(color));
                            if worst != Risk::Origin && worst != Risk::Unstable {
                                let gain = oc_gain(&steps, core, mem, ratio);
                                if ratio < 0.995 {
                                    let loss = fps_loss(ratio);
                                    let net = ((1.0 - loss / 100.0) * (1.0 + gain / 100.0) - 1.0) * 100.0;
                                    ui.label(RichText::new(format!("À {watts:.0} W, carte à fond : ≈ −{loss:.0} % d'images/s sans overclocking, ≈ {}{:.0} % avec (gain ≈ +{gain:.0} %), sans chaleur en plus.", if net < -0.5 { "−" } else { "+" }, net.abs())).color(TEXT));
                                } else {
                                    ui.label(RichText::new(format!("Gain ≈ +{gain:.0} % d'images/s à pleine charge, pour un peu plus de chaleur (toujours sous {watts:.0} W).")).color(TEXT));
                                }
                            }
                            if rm >= Risk::High {
                                small(ui, "Mémoire vidéo trop poussée : elle corrige ses erreurs en silence et les images/s baissent au lieu de monter.");
                            } else if limited && mem > useful_mem(&steps, ratio) {
                                small(ui, format!("Avec la limite à {watts:.0} W, au-delà de +{} sur la mémoire vidéo le gain est nul : mieux vaut monter le processeur graphique.", useful_mem(&steps, ratio)));
                            }
                        });
                        ui.add_space(8.0);
                        if (core, mem) != (now.core, now.mem) {
                            ui.label(RichText::new("Pas encore appliqué : clique sur « Essayer maintenant ».").color(WARM));
                        } else if active {
                            ui.label(RichText::new("Ce réglage est appliqué en ce moment.").color(ACCENT));
                        }
                        let half = (ui.available_width() - 10.0) / 2.0;
                        let persist_ok = worst != Risk::Unstable && worst != Risk::Origin;
                        ui.horizontal(|ui| {
                            if big_button(ui, !busy, "Essayer maintenant", false, half) {
                                self.start(Job::Oc { uuid: g.uuid.clone(), core, mem }, ui.ctx());
                            }
                            if big_button(ui, !busy && persist_ok, "Garder au démarrage", true, half) {
                                self.start(Job::OcPersist { uuid: g.uuid.clone(), core, mem, unit: g.oc_unit() }, ui.ctx());
                            }
                        });
                        if active || oc_unit.is_some() {
                            ui.add_space(4.0);
                            if big_button(ui, !busy, if oc_unit.is_some() { "Tout remettre d'origine (et retirer du démarrage)" } else { "Remettre les fréquences d'origine" }, false, ui.available_width()) {
                                let job = match &oc_unit {
                                    Some(u) => Job::OcRemove { uuid: g.uuid.clone(), unit: u.file.clone() },
                                    None => Job::Oc { uuid: g.uuid.clone(), core: 0, mem: 0 },
                                };
                                core = 0;
                                mem = 0;
                                self.start(job, ui.ctx());
                            }
                        }
                        small(ui, format!("Fréquences en ce moment : {:.0} MHz (processeur graphique) · {:.0} MHz (mémoire). Au repos elles sont basses, c'est normal : elles montent en jeu.", g.clock, g.mem_clock));
                    });
                    self.oc_target = Some((core, mem));
                });
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
/// capture après `APP_SCREENSHOT_DELAY` s (3 par défaut) puis ferme l'app ;
/// `APP_SCREENSHOT_SCROLL=<px>` fait d'abord défiler la page.
struct TestHook { path: Option<String>, delay: f32, start: Instant, requested: bool, scroll: Option<f32> }

impl Default for TestHook {
    fn default() -> Self {
        TestHook {
            path: std::env::var("APP_SCREENSHOT").ok().filter(|s| !s.is_empty()),
            delay: std::env::var("APP_SCREENSHOT_DELAY").ok().and_then(|s| s.parse().ok()).unwrap_or(3.0),
            start: Instant::now(), requested: false,
            scroll: std::env::var("APP_SCREENSHOT_SCROLL").ok().and_then(|s| s.parse().ok()),
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
        let g = parse_gpu("0, GPU-ebab68d5-d9c6-1718-88bf-c79a87a14bc7, NVIDIA GeForce RTX 4070, 21.24, 200.00, 200.00, 100.00, 216.00, 39, 0, 2490, 10501, 3, 1191, 12282").unwrap();
        assert_eq!(g.name, "NVIDIA GeForce RTX 4070");
        assert_eq!((g.limit, g.default, g.min, g.max, g.mem_clock, g.util), (200.0, 200.0, 100.0, 216.0, 10501.0, 3.0));
        assert_eq!((g.vram_used, g.vram_total), (1191.0, 12282.0));
        assert!(g.adjustable());
        assert_eq!(g.unit(), "powercap-gpu-ebab68d5.service");
        assert_eq!(g.oc_unit(), "powercap-oc-ebab68d5.service");
        // Carte de portable : limites non disponibles.
        let g = parse_gpu("1, GPU-1234, NVIDIA GeForce RTX 4060 Laptop GPU, [N/A], [N/A], [N/A], [N/A], [N/A], 40, [N/A], 0, 0, 0, [N/A], [N/A]").unwrap();
        assert_eq!((g.index, g.draw), (1, 0.0));
        assert!(!g.adjustable());
        // Nom avec virgule.
        let g = parse_gpu("0, GPU-x, A, B, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12").unwrap();
        assert_eq!((g.name.as_str(), g.util, g.vram_total), ("A, B", 10.0, 12.0));
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
    fn ram_and_xmp() {
        for (part, mts) in [("CP16G60C36U5B.M8D1", 6000), ("KF560C36BBE-16", 6000), ("CMK32GX5M2B6000C36", 6000),
            ("F5-6000J3038F16G", 6000), ("KF432C16BB/8", 3200), ("BL8G32C16U4B", 3200), ("M425R2GA3BB0-CQKOL", 0)] {
            assert_eq!(rated_from_part(part), mts, "{part}");
        }
        let dmi = "E:MEMORY_DEVICE_0_SIZE=17179869184\nE:MEMORY_DEVICE_0_TYPE=DDR5\nE:MEMORY_DEVICE_0_SPEED_MTS=6000\n\
            E:MEMORY_DEVICE_0_CONFIGURED_SPEED_MTS=6000\nE:MEMORY_DEVICE_0_PART_NUMBER=CP16G60C36U5B.M8D1\n\
            E:MEMORY_DEVICE_0_MANUFACTURER=Micron Technology\nE:MEMORY_DEVICE_0_MAXIMUM_VOLTAGE=1.35\nE:MEMORY_DEVICE_1_SIZE=17179869184\n\
            E:MEMORY_DEVICE_1_CONFIGURED_SPEED_MTS=6000\nE:MEMORY_DEVICE_2_SIZE=0\n";
        let k = parse_ram_kit(dmi);
        assert_eq!((k.modules, k.module_gb, k.kind.as_str(), k.configured, k.max()), (2, 16.0, "DDR5", 6000, 6000));
        assert_eq!(xmp_state(&k), Xmp::On);
        // Même kit, XMP coupé : le BIOS règle 4800 alors que la barrette fait 6000.
        let off = RamKit { speed: 4800, configured: 4800, ..k.clone() };
        assert_eq!(xmp_state(&off), Xmp::Off);
        let plain = RamKit { kind: "DDR4".into(), speed: 3200, configured: 3200, rated: 0, ..Default::default() };
        assert_eq!(xmp_state(&plain), Xmp::Standard);
        assert_eq!(xmp_state(&RamKit::default()), Xmp::Unknown);
        // Tension : 1,35 V avec XMP, 1,1 V sans.
        assert_eq!(ram_voltage(&k), Some(1.35));
        assert_eq!(ram_voltage(&off), Some(1.1));
        // Consommation : 2 barrettes DDR5 XMP, ≈ 3,6 W au repos, ≈ 12 W à fond ; moins sans XMP.
        let (idle, full) = (ram_power_estimate(&k, 0.0), ram_power_estimate(&k, 1.0));
        assert!((3.0..4.5).contains(&idle) && (10.0..13.0).contains(&full), "{idle} {full}");
        assert!(ram_power_estimate(&off, 1.0) < full);
        assert_eq!(ram_power_estimate(&RamKit::default(), 1.0), 0.0);
    }

    #[test]
    fn overclocking() {
        assert_eq!(parse_oc_unit("[Service]\nExecStart=/usr/bin/python3 -I /etc/powercap/gpu-oc.py GPU-ab-cd 150 1000\n"), Some(("GPU-ab-cd".into(), 150, 1000)));
        assert_eq!(parse_oc_unit("ExecStart=/usr/bin/nvidia-smi -pl 150"), None);
        let s = oc_steps("NVIDIA GeForce RTX 4070");
        assert_eq!([0, 100, 150, 300, 305].map(|v| risk(v, s.core)), [Risk::Origin, Risk::Safe, Risk::Typical, Risk::High, Risk::Unstable]);
        assert_eq!(risk(3500, s.mem), Risk::Unstable);
        assert!(oc_steps("NVIDIA GeForce RTX 5070").core[0] > s.core[0]);
        // Curseurs : un peu au-delà du palier élevé, jamais au-delà du pilote.
        let o = nvml::Offsets { core_range: (-1000, 1000), mem_range: (-2000, 6000), ..Default::default() };
        assert_eq!(oc_caps(&s, &o), (400, 3500));
        let tight = nvml::Offsets { core_range: (0, 250), mem_range: (0, 1500), ..Default::default() };
        assert_eq!(oc_caps(&s, &tight), (250, 1500));
        // Conseils selon les watts : la mémoire baisse avec la limite, le cœur reste.
        assert_eq!(oc_plan(&s, 1.0), [(100, 1000), (200, 2000), (300, 3000)]);
        assert_eq!(oc_plan(&s, 0.6), [(100, 750), (200, 1000), (300, 1500)]);
        assert_eq!(oc_plan(&s, 0.8), [(100, 1000), (200, 1500), (300, 2250)]);
        // Un overclocking rapporte plus à 120 W qu'à 200 W, et la mémoire inutile ne compte pas.
        assert!(oc_gain(&s, 100, 750, 0.6) > oc_gain(&s, 100, 750, 1.0));
        assert_eq!(oc_gain(&s, 0, 3000, 0.6), oc_gain(&s, 0, 1500, 0.6));
    }

    #[test]
    fn scripts_are_valid_shell() {
        for s in [PERSIST_SCRIPT, REMOVE_SCRIPT, OC_PERSIST_SCRIPT, OC_REMOVE_SCRIPT] {
            let ok = Command::new("sh").args(["-n", "-c", s]).status().map(|s| s.success()).unwrap_or(true);
            assert!(ok);
        }
        let ok = Command::new("python3").args(["-I", "-c", "import ast, sys; ast.parse(sys.argv[1])", OC_PY]).status().map(|s| s.success()).unwrap_or(true);
        assert!(ok);
        // Le script refuse ce qui n'est pas une carte ou une valeur permise, avant de toucher au pilote.
        for args in [["rm -rf", "0", "0"], ["GPU-ab", "5000", "0"], ["GPU-ab", "0", "-5"]] {
            if let Ok(out) = Command::new("python3").args(["-I", "-c", OC_PY]).args(args).output() {
                assert!(!out.status.success(), "{args:?}");
            }
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
