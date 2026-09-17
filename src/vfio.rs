use std::collections::BTreeMap;
use std::path::Path;

pub use pcilibs_rs::{IommufdDev, Sysfs, IOMMUFD_VFIO_DIR as VFIO_DIR, SYSFS as SYSFS_ROOT};

/// How advertised resource names are derived (--resource-naming flag).
#[derive(Clone, Copy, Debug)]
pub enum Naming {
    /// Names come from the RESOURCES table: curated alias rows first,
    /// class-generic fallbacks otherwise.  The default.
    Alias,
    /// Names carry the hardware identity, resolved from the PCI ID database:
    /// "nvidia.com/GH100_H100_SXM5_80GB".  Unknown device ids fall back to
    /// the matching table row's name.
    Sku,
}

impl Naming {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "alias" => Ok(Self::Alias),
            "sku" => Ok(Self::Sku),
            other => Err(format!(
                "--resource-naming must be alias or sku, got {other:?}"
            )),
        }
    }
}

/// One advertisable resource: a Kubernetes extended-resource name declared
/// by a PCI identity match against `/sys/class/vfio-dev`.
pub struct Resource {
    /// Kubernetes extended-resource name; also the CDI kind.
    pub name: &'static str,
    /// PCI vendor ID, e.g. `0x10de` for NVIDIA.
    pub vendor: u16,
    /// Exact PCI device id for curated alias rows, e.g. Some(0x2941);
    /// None matches the whole class.
    pub device: Option<u16>,
    /// Base class and subclass (upper 16 bits of the 24-bit class code),
    /// e.g. `0x0302` for 3D controller.
    pub class_prefix: u16,
}

/// Everything this plugin can advertise, first match wins — put curated
/// alias rows (exact device id) above their class fallback.  A resource is
/// advertised iff matching devices are VFIO-bound; supporting a new device
/// type or alias is one new row, no other code changes.
pub const RESOURCES: &[Resource] = &[
    Resource {
        name: "nvidia.com/gpu",
        vendor: 0x10de,
        device: None,
        class_prefix: 0x0302, // 3D controller
    },
    Resource {
        name: "nvidia.com/nvswitch",
        vendor: 0x10de,
        device: None,
        class_prefix: 0x0680, // bridge: other
    },
];

/// Enumerate all IOMMUFD cdevs and group them by advertised resource name.
/// Rows are first-match-wins per device, so every cdev lands in exactly one
/// resource; devices within a name keep numeric cdev order, which fixes the
/// advertised device IDs / CDI spec indices.
pub fn discover(
    vfio_dir: &Path,
    sysfs: &Sysfs,
    naming: Naming,
) -> BTreeMap<String, Vec<IommufdDev>> {
    let mut map: BTreeMap<String, Vec<IommufdDev>> = BTreeMap::new();
    for dev in pcilibs_rs::enumerate_iommufd(vfio_dir, sysfs) {
        let Some(row) = RESOURCES.iter().find(|r| {
            dev.vendor == r.vendor
                && dev.class_prefix() == r.class_prefix
                && r.device.is_none_or(|d| dev.device == d)
        }) else {
            continue;
        };
        let name = match naming {
            Naming::Alias => row.name.to_owned(),
            Naming::Sku => sku_name(row, &dev),
        };
        map.entry(name).or_default().push(dev);
    }
    map
}

/// 10de:2330 → "nvidia.com/GH100_H100_SXM5_80GB"; ids the PCI database does
/// not know — or whose name sanitizes to nothing usable — fall back to the
/// row's own name, so a bad database entry degrades to the generic resource
/// instead of producing a name the kubelet rejects.
fn sku_name(row: &Resource, dev: &IommufdDev) -> String {
    let domain = row.name.split('/').next().unwrap_or("nvidia.com");
    dev.device_name()
        .and_then(sku_segment)
        .map(|s| format!("{domain}/{s}"))
        .unwrap_or_else(|| row.name.to_owned())
}

/// Extended-resource names must start and end alphanumeric and the name part
/// is capped at 63 chars, so trim stray underscores and truncate; None if
/// nothing survives sanitization.
fn sku_segment(raw: &str) -> Option<String> {
    let s = sanitize(raw);
    let s = s.trim_matches('_');
    if s.is_empty() {
        return None;
    }
    // sanitize() output is pure ASCII, so byte truncation is safe.
    let s = &s[..s.len().min(63)];
    Some(s.trim_matches('_').to_owned())
}

/// kubevirt-gpu-device-plugin compatible sanitization, so SKU names match
/// the convention that ecosystem already uses: uppercase, '/', '.', and
/// whitespace runs become '_', everything else non-alphanumeric is dropped.
fn sanitize(raw: &str) -> String {
    let upper = raw.trim().to_ascii_uppercase().replace(['/', '.'], "_");
    let mut out = String::with_capacity(upper.len());
    let mut pending_ws = false;
    for c in upper.chars() {
        if c.is_whitespace() {
            pending_ws = true;
            continue;
        }
        if pending_ws {
            out.push('_');
            pending_ws = false;
        }
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        }
    }
    out
}

/// NVIDIA-flavoured wrappers over pcilibs-rs's `testfs` fixtures.
#[cfg(test)]
pub(crate) mod testfs {
    use std::path::Path;

    pub use pcilibs_rs::testfs::add;

    /// An H100 SXM5 80GB: pci-ids 10de:2330, so SKU naming is exercised
    /// against the real database.
    pub fn add_gpu(root: &Path, n: u32) {
        add(root, n, "0x10de", "0x2330", "0x030200");
    }

    pub fn add_nvswitch(root: &Path, n: u32) {
        add(root, n, "0x10de", "0x22a3", "0x068000");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn nums(devs: &[IommufdDev]) -> Vec<u32> {
        devs.iter().map(|d| d.num).collect()
    }

    #[test]
    fn alias_mode_classifies_and_sorts() {
        let root = TempDir::new().unwrap();
        testfs::add_gpu(root.path(), 42);
        testfs::add_gpu(root.path(), 7);
        testfs::add_nvswitch(root.path(), 9);
        // A VFIO-bound NIC must not be advertised under any resource.
        testfs::add(root.path(), 3, "0x15b3", "0x101e", "0x020000");

        let map = discover(root.path(), &Sysfs::new(root.path()), Naming::Alias);
        assert_eq!(map.len(), 2);
        assert_eq!(nums(&map["nvidia.com/gpu"]), vec![7, 42]);
        assert!(map["nvidia.com/gpu"][0].path.ends_with("devices/vfio7"));
        assert_eq!(nums(&map["nvidia.com/nvswitch"]), vec![9]);
    }

    #[test]
    fn sku_mode_names_from_pci_ids() {
        let root = TempDir::new().unwrap();
        testfs::add_gpu(root.path(), 0);
        testfs::add_gpu(root.path(), 1);

        let map = discover(root.path(), &Sysfs::new(root.path()), Naming::Sku);
        assert_eq!(map.len(), 1);
        let (name, devs) = map.iter().next().unwrap();
        // Loose on the marketing suffix: the database may reword it, but the
        // die name and domain are stable.
        assert!(
            name.starts_with("nvidia.com/GH100"),
            "unexpected SKU name: {name}"
        );
        assert!(!name.contains(['[', ']', ' ']), "unsanitized: {name}");
        assert_eq!(nums(devs), vec![0, 1]);
    }

    #[test]
    fn sku_mode_unknown_device_falls_back_to_row_name() {
        let root = TempDir::new().unwrap();
        testfs::add(root.path(), 0, "0x10de", "0xdead", "0x030200");

        let map = discover(root.path(), &Sysfs::new(root.path()), Naming::Sku);
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("nvidia.com/gpu"));
    }

    #[test]
    fn curated_alias_row_wins_over_class_fallback() {
        // A hypothetical curated row for the H100 placed above the generic
        // GPU row must capture exactly the matching device id.
        const CURATED: &[Resource] = &[
            Resource {
                name: "nvidia.com/h100",
                vendor: 0x10de,
                device: Some(0x2330),
                class_prefix: 0x0302,
            },
            Resource {
                name: "nvidia.com/gpu",
                vendor: 0x10de,
                device: None,
                class_prefix: 0x0302,
            },
        ];
        let dev_h100 = IommufdDev {
            num: 0,
            path: "/dev/vfio/devices/vfio0".into(),
            vendor: 0x10de,
            device: 0x2330,
            class: 0x030200,
        };
        let dev_other = IommufdDev {
            device: 0x1234,
            ..dev_h100.clone()
        };
        let pick = |dev: &IommufdDev| {
            CURATED
                .iter()
                .find(|r| {
                    dev.vendor == r.vendor
                        && dev.class_prefix() == r.class_prefix
                        && r.device.is_none_or(|d| dev.device == d)
                })
                .unwrap()
                .name
        };
        assert_eq!(pick(&dev_h100), "nvidia.com/h100");
        assert_eq!(pick(&dev_other), "nvidia.com/gpu");
    }

    #[test]
    fn missing_sysfs_entry_is_not_advertised() {
        let root = TempDir::new().unwrap();
        let devices = root.path().join("devices");
        std::fs::create_dir_all(&devices).unwrap();
        std::fs::write(devices.join("vfio0"), b"").unwrap();

        for naming in [Naming::Alias, Naming::Sku] {
            assert!(discover(root.path(), &Sysfs::new(root.path()), naming).is_empty());
        }
    }

    #[test]
    fn missing_devices_dir_is_empty() {
        let root = TempDir::new().unwrap();
        assert!(discover(root.path(), &Sysfs::new(root.path()), Naming::Alias).is_empty());
    }

    #[test]
    fn sku_segment_rejects_degenerate_names() {
        assert_eq!(sku_segment("###"), None);
        assert_eq!(sku_segment("  "), None);
        assert_eq!(sku_segment("._x_."), Some("X".to_owned()));
        // 63-char cap, no trailing underscore left by the cut.
        let long = format!("{}_TAIL", "A".repeat(62));
        assert_eq!(sku_segment(&long), Some("A".repeat(62)));
    }

    #[test]
    fn sanitize_matches_kubevirt_convention() {
        assert_eq!(sanitize("GH100 [H100 SXM5 80GB]"), "GH100_H100_SXM5_80GB");
        assert_eq!(sanitize("GB100 [HGX GB200]"), "GB100_HGX_GB200");
        assert_eq!(sanitize("a/b.c  d"), "A_B_C_D");
        assert_eq!(sanitize("  trimmed  "), "TRIMMED");
    }

    #[test]
    fn naming_parses_only_the_two_modes() {
        assert!(matches!(Naming::parse("alias"), Ok(Naming::Alias)));
        assert!(matches!(Naming::parse("sku"), Ok(Naming::Sku)));
        assert!(Naming::parse("SKU").is_err());
        assert!(Naming::parse("").is_err());
    }
}
