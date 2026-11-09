
use super::*;

pub const SETTINGS_FILE: &str = "./AppOpt.json";

static SAVE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone)]
pub struct Settings {
    pub web_enable: bool,
    pub mode: u8,
    pub check_interval: u64,
    pub cpuset_name: String,
    pub config_file: String,
    pub web_port: u16,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            web_enable: false,
            mode: 0,
            check_interval: 2,
            cpuset_name: DEFAULT_CPUSET_NAME.to_string(),
            config_file: "./applist.conf".to_string(),
            web_port: 8889,
        }
    }
}

pub(super) fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.len() < 64 && !s.contains('/') && !s.bytes().any(|b| b <= b' ')
}

pub(super) fn valid_path(s: &str) -> bool {
    !s.is_empty() && s.len() < 256 && !s.bytes().any(|b| b < 0x20 || b == 0x7f)
}

impl Settings {
    fn from_json(v: &Value) -> Self {
        let d = Settings::default();
        Self {
            web_enable: v["web_enable"].as_bool().unwrap_or(d.web_enable),
            mode: v["mode"].as_u64().unwrap_or(d.mode as u64).min(2) as u8,
            check_interval: v["check_interval"]
                .as_u64()
                .unwrap_or(d.check_interval)
                .clamp(1, 3600),
            cpuset_name: v["cpuset_name"]
                .as_str()
                .filter(|s| valid_name(s))
                .unwrap_or(&d.cpuset_name)
                .to_string(),
            config_file: v["config_file"]
                .as_str()
                .filter(|s| valid_path(s))
                .unwrap_or(&d.config_file)
                .to_string(),
            web_port: v["web_port"]
                .as_u64()
                .filter(|n| (1024..=65535).contains(n))
                .unwrap_or(d.web_port as u64) as u16,
        }
    }

    fn to_value(&self) -> Value {
        json!({
            "web_enable": self.web_enable,
            "mode": self.mode,
            "check_interval": self.check_interval,
            "cpuset_name": self.cpuset_name,
            "config_file": self.config_file,
            "web_port": self.web_port,
        })
    }

    fn save(&self, path: &str) {
        let _guard = lock_ignore_poison(&SAVE_LOCK);
        let json = serde_json::to_string_pretty(&self.to_value()).unwrap_or_default();
        let tmp = format!("{}.tmp", path);
        let res = fs::File::create(&tmp)
            .and_then(|mut f| {
                f.write_all(format!("{}\n", json).as_bytes())?;
                f.sync_all()
            })
            .and_then(|_| fs::rename(&tmp, path));
        if let Err(e) = res {
            eprintln!("警告: 设置写入 {} 失败: {}", path, e);
        }
    }
}

pub fn settings_load(path: &str) -> Settings {
    match fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(v) => Settings::from_json(&v),
            Err(e) => {
                eprintln!("警告: {} 已损坏({})，使用默认设置", path, e);
                Settings::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let d = Settings::default();
            d.save(path);
            println!("设置文件不存在，已写入默认设置: {}", path);
            d
        }
        Err(e) => {
            eprintln!("警告: 读取 {} 失败({})，使用默认设置", path, e);
            Settings::default()
        }
    }
}

pub fn settings_save() {
    Settings {
        web_enable: WEB_ENABLED.load(Ordering::Relaxed),
        mode: MODE_FORCE.load(Ordering::Relaxed),
        check_interval: CHECK_INTERVAL.load(Ordering::Relaxed).max(1),
        cpuset_name: base_cpuset().rsplit('/').next().unwrap_or_default().to_string(),
        config_file: lock_ignore_poison(&CONFIG_FILE).clone(),
        web_port: WEB_PORT_TARGET.load(Ordering::Relaxed),
    }
    .save(SETTINGS_FILE);
}
