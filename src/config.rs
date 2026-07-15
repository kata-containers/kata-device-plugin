use std::path::Path;

use serde::Deserialize;
use tracing::warn;

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub gpu: GpuConfig,
    pub nvswitch: NvswitchConfig,
    pub plugin: PluginConfig,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct GpuConfig {
    pub resource_name: String,
    pub device_dir: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct NvswitchConfig {
    pub enabled: bool,
    pub resource_name: String,
    pub device_dir: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct PluginConfig {
    pub socket_dir: String,
    pub cdi_dir: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            gpu: GpuConfig::default(),
            nvswitch: NvswitchConfig::default(),
            plugin: PluginConfig::default(),
        }
    }
}

impl Default for GpuConfig {
    fn default() -> Self {
        Self {
            resource_name: "nvidia.com/gpu".to_owned(),
            device_dir: "/dev/vfio".to_owned(),
        }
    }
}

impl Default for NvswitchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            resource_name: "nvidia.com/nvswitch".to_owned(),
            device_dir: "/dev/vfio".to_owned(),
        }
    }
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            socket_dir: "/var/lib/kubelet/device-plugins".to_owned(),
            cdi_dir: "/var/run/cdi".to_owned(),
        }
    }
}

impl Config {
    /// Load from `path`, falling back to defaults on any error.
    /// A missing file is not an error — compiled-in defaults are used.
    pub fn load(path: &Path) -> Self {
        let s = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                warn!(path = %path.display(), %e, "cannot read config, using defaults");
                return Self::default();
            }
        };
        match toml::from_str(&s) {
            Ok(c) => c,
            Err(e) => {
                warn!(%e, "config parse error, using defaults");
                Self::default()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn defaults_when_file_missing() {
        let cfg = Config::load(Path::new("/nonexistent/config.toml"));
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn full_override() {
        let mut f = NamedTempFile::new().unwrap();
        write!(f, r#"
            [gpu]
            resource_name = "example.com/gpu"
            device_dir = "/tmp/vfio"

            [nvswitch]
            enabled = true
            resource_name = "example.com/nvswitch"
            device_dir = "/tmp/vfio"

            [plugin]
            socket_dir = "/tmp/dp"
            cdi_dir = "/tmp/cdi"
        "#).unwrap();
        let cfg = Config::load(f.path());
        assert_eq!(cfg.gpu.resource_name, "example.com/gpu");
        assert_eq!(cfg.gpu.device_dir, "/tmp/vfio");
        assert!(cfg.nvswitch.enabled);
        assert_eq!(cfg.nvswitch.resource_name, "example.com/nvswitch");
        assert_eq!(cfg.plugin.socket_dir, "/tmp/dp");
        assert_eq!(cfg.plugin.cdi_dir, "/tmp/cdi");
    }

    #[test]
    fn partial_override_keeps_defaults() {
        let mut f = NamedTempFile::new().unwrap();
        write!(f, r#"
            [nvswitch]
            enabled = true
        "#).unwrap();
        let cfg = Config::load(f.path());
        // Unmentioned sections keep their defaults.
        assert_eq!(cfg.gpu, GpuConfig::default());
        assert_eq!(cfg.plugin, PluginConfig::default());
        assert!(cfg.nvswitch.enabled);
        // Unmentioned fields within the section keep their defaults.
        assert_eq!(cfg.nvswitch.resource_name, NvswitchConfig::default().resource_name);
    }

    #[test]
    fn bad_toml_falls_back_to_defaults() {
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "this is not valid toml ][[[").unwrap();
        let cfg = Config::load(f.path());
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn hot_reload_detects_change() {
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "[gpu]\nresource_name = \"a.com/gpu\"").unwrap();
        let cfg1 = Config::load(f.path());

        // Overwrite in place — simulates a ConfigMap update settling.
        f.as_file_mut().set_len(0).unwrap();
        use std::io::Seek;
        f.seek(std::io::SeekFrom::Start(0)).unwrap();
        write!(f, "[gpu]\nresource_name = \"b.com/gpu\"").unwrap();
        let cfg2 = Config::load(f.path());

        assert_ne!(cfg1, cfg2);
        assert_eq!(cfg2.gpu.resource_name, "b.com/gpu");
    }
}
}
