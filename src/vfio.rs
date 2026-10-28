use std::cell::OnceCell;
use std::collections::BTreeMap;
use std::path::Path;

use pcilibs_rs::platform::Topology;
pub use pcilibs_rs::{IommufdDev, Sysfs, IOMMUFD_VFIO_DIR as VFIO_DIR, SYSFS as SYSFS_ROOT};

/// One advertised device: the bound functions it spans, in cdev order.  This
/// is both the unit the kubelet schedules and the CDI device the shim
/// injects, so a row's [`Unit`] decides what a consumer can take: a PCI
/// device whole, or the node's NVSwitch fabric whole.  Functions are never
/// advertised separately, because no consumer can take one without its
/// siblings.
pub type Device = Vec<IommufdDev>;

/// How a row's bound functions become advertised devices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    /// One device per PCI device, holding every bound function of it.
    PciDevice,
    /// One device per node, holding every bound function of every row with
    /// the name, and only once all of them are bound.  For a resource whose
    /// only consumer takes it whole: the NVSwitch fabric a Service VM
    /// manages, four switch chips on HGX H100/H200 or the management
    /// ConnectX's PFs on HGX B200/B300.
    Fabric,
}

impl Unit {
    /// The contract spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PciDevice => "pci-device",
            Self::Fabric => "fabric",
        }
    }
}

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
    pub const ALL: [Self; 2] = [Self::Alias, Self::Sku];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Alias => "alias",
            Self::Sku => "sku",
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|mode| mode.as_str() == s)
            .ok_or_else(|| {
                let modes = Self::ALL.map(Self::as_str).join(" or ");
                format!("--resource-naming must be {modes}, got {s:?}")
            })
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
    /// What one advertised device of this row spans.  A fabric row admits a
    /// cdev only when pcilibs-rs's topology scan names its function, so
    /// (vendor, class) says which cdevs may join and the scan says which do.
    pub unit: Unit,
}

impl Resource {
    /// PCI identity match.  Fabric membership is applied by `discover`,
    /// which owns the one topology scan per pass.
    fn matches(&self, dev: &IommufdDev) -> bool {
        dev.vendor == self.vendor
            && dev.class_prefix() == self.class_prefix
            && self.device.is_none_or(|d| dev.device == d)
    }
}

/// Everything this plugin can advertise, first match wins — put curated
/// alias rows (exact device id) above their class fallback.  Rows may share
/// a name when one resource has more than one PCI face; their devices are
/// grouped together.  A resource is advertised iff matching devices are
/// VFIO-bound; supporting a new device type or alias is one new row, no
/// other code changes.
pub const RESOURCES: &[Resource] = &[
    Resource {
        name: "nvidia.com/gpu",
        vendor: 0x10de,
        device: None,
        class_prefix: 0x0302, // 3D controller
        unit: Unit::PciDevice,
    },
    // The NVSwitch fabric a Service VM running Fabric Manager takes whole
    // (HGX Shared NVSwitch guide 19.0, section 3.2.2): on HGX H100/H200 the
    // switch chips, PCI bridge functions.
    Resource {
        name: "nvidia.com/nvswitch-fabric",
        vendor: 0x10de,
        device: None,
        class_prefix: 0x0680, // bridge: other
        unit: Unit::Fabric,
    },
    // On HGX B200/B300 the NVSwitches "are represented as full and limited
    // capability PFs under the management Mellanox ConnectX" (section
    // 3.2.2.2), so those PFs are the same fabric, not a second name.  The
    // node's data-path ConnectX NICs share this identity; pcilibs-rs tells
    // them apart by the VPD role the guide documents.
    Resource {
        name: "nvidia.com/nvswitch-fabric",
        vendor: 0x15b3,
        device: None,
        class_prefix: 0x0207, // InfiniBand controller
        unit: Unit::Fabric,
    },
];

/// Enumerate all IOMMUFD cdevs and group them into advertised devices by
/// resource name.  Rows are first-match-wins per cdev, so every cdev lands
/// in exactly one resource.  Within a name the row's unit decides the
/// grouping: PCI-device rows order their devices by PCI address, which
/// survives a reboot where cdev numbers do not, and a fabric is one device,
/// advertised whole or not at all.  That order fixes the advertised device
/// IDs / CDI spec indices.
pub fn discover(vfio_dir: &Path, sysfs: &Sysfs, naming: Naming) -> BTreeMap<String, Vec<Device>> {
    // Name, then PCI slot, then the slot's bound functions.  No row has a
    // multi-function PCI device today; the slot is the principle
    // ARCHITECTURE.md states, functions are never separate devices, and the
    // order key.
    let mut pci: BTreeMap<String, BTreeMap<String, Device>> = BTreeMap::new();
    let mut fabrics: BTreeMap<String, Device> = BTreeMap::new();
    // The topology scan reads every Mellanox PF's VPD, so it runs at most
    // once per pass, and not at all unless a fabric row has a candidate.
    let scanned: OnceCell<Option<Topology>> = OnceCell::new();
    let topology = || scanned.get_or_init(|| fabric_topology(sysfs)).as_ref();
    for dev in pcilibs_rs::enumerate_iommufd(vfio_dir, sysfs) {
        // A fabric row admits a cdev only when the scan names its function,
        // so the fabric has one definition, the scan's, which NVRC reads in
        // the guest too.
        let Some(row) = RESOURCES.iter().find(|r| {
            r.matches(&dev)
                && (r.unit != Unit::Fabric
                    || topology().is_some_and(|t| fabric(t).any(|bdf| bdf == dev.bdf)))
        }) else {
            continue;
        };
        match row.unit {
            Unit::PciDevice => {
                let name = match naming {
                    Naming::Alias => row.name.to_owned(),
                    Naming::Sku => sku_name(row, &dev),
                };
                let slot = slot(&dev.bdf).to_owned();
                pci.entry(name)
                    .or_default()
                    .entry(slot)
                    .or_default()
                    .push(dev);
            }
            // A fabric spans PCI devices, so one function's SKU would misname
            // it; fabric rows keep their alias in both modes.
            Unit::Fabric => fabrics.entry(row.name.to_owned()).or_default().push(dev),
        }
    }
    // A Service VM holding part of a fabric cannot bring it up, and the
    // kubelet cannot be told to wait, so the one device exists only once
    // every function the scan names is bound.  Admission needed the scan, so
    // a fabric here always has one, and its bound functions are a subset of
    // the named ones by construction.
    fabrics.retain(|name, device| {
        let missing: Vec<&str> = topology()
            .into_iter()
            .flat_map(fabric)
            .filter(|bdf| !device.iter().any(|cdev| cdev.bdf == *bdf))
            .collect();
        if !missing.is_empty() {
            tracing::warn!(
                resource = %name,
                bound = device.len(),
                present = device.len() + missing.len(),
                ?missing,
                "NVSwitch fabric is not completely bound, advertising none"
            );
        }
        missing.is_empty()
    });
    pci.into_iter()
        .map(|(name, devices)| (name, devices.into_values().collect()))
        .chain(
            fabrics
                .into_iter()
                .map(|(name, device)| (name, vec![device])),
        )
        .collect()
}

/// `0000:27:00.1` → `0000:27:00`: the PCI device its functions share.
fn slot(bdf: &str) -> &str {
    bdf.rsplit_once('.').map_or(bdf, |(slot, _)| slot)
}

/// The fabric a Service VM is given whole (HGX guide 19.0, section 3.2.2):
/// the switch chips where they are PCI functions, the management ConnectX
/// PFs where they are not.
fn fabric(topology: &Topology) -> impl Iterator<Item = &str> {
    topology
        .switches
        .iter()
        .map(String::as_str)
        .chain(topology.management_functions.iter().map(|f| f.bdf.as_str()))
}

/// The node's switch chips and NVSwitch management PFs, bound or not.  The
/// scan fails closed: an unreadable or malformed VPD advertises no fabric
/// this tick and the poller retries next tick.  NVRC applies the same policy
/// inside the Service VM, so both ends agree that unsure means not a switch.
fn fabric_topology(sysfs: &Sysfs) -> Option<Topology> {
    match pcilibs_rs::platform::discover_topology(sysfs) {
        Ok(topology) => Some(topology),
        Err(e) => {
            tracing::warn!("NVSwitch fabric scan failed, advertising none: {e}");
            None
        }
    }
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

    pub use pcilibs_rs::testfs::{add, add_connectx};

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

    /// Cdev numbers per advertised device.
    fn nums(devices: &[Device]) -> Vec<Vec<u32>> {
        devices
            .iter()
            .map(|device| device.iter().map(|cdev| cdev.num).collect())
            .collect()
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
        assert_eq!(nums(&map["nvidia.com/gpu"]), vec![vec![7], vec![42]]);
        assert!(map["nvidia.com/gpu"][0][0].path.ends_with("devices/vfio7"));
        assert_eq!(nums(&map["nvidia.com/nvswitch-fabric"]), vec![vec![9]]);
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
        assert_eq!(nums(devs), vec![vec![0], vec![1]]);
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
                unit: Unit::PciDevice,
            },
            Resource {
                name: "nvidia.com/gpu",
                vendor: 0x10de,
                device: None,
                class_prefix: 0x0302,
                unit: Unit::PciDevice,
            },
        ];
        let dev_h100 = IommufdDev {
            num: 0,
            path: "/dev/vfio/devices/vfio0".into(),
            bdf: "0000:01:00.0".to_owned(),
            vendor: 0x10de,
            device: 0x2330,
            class: 0x030200,
        };
        let dev_other = IommufdDev {
            device: 0x1234,
            ..dev_h100.clone()
        };
        let pick = |dev: &IommufdDev| CURATED.iter().find(|r| r.matches(dev)).unwrap().name;
        assert_eq!(pick(&dev_h100), "nvidia.com/h100");
        assert_eq!(pick(&dev_other), "nvidia.com/gpu");
    }

    #[test]
    fn b200_fabric_is_the_management_connectx_as_one_device() {
        // HGX B200/B300: the two limited PFs carry the marker, the two
        // full-capability siblings do not, and all four form the fabric,
        // however their cdev numbers interleave with other devices.  A
        // data-path ConnectX on another slot has the same vendor and class,
        // no role, and must not be advertised anywhere.
        let root = TempDir::new().unwrap();
        let r = root.path();
        for (n, function, marker) in [(1, 0, true), (3, 1, true), (4, 2, false), (5, 3, false)] {
            testfs::add_connectx(r, n, &format!("0000:05:00.{function}"), marker);
        }
        testfs::add_gpu(r, 2);
        testfs::add_connectx(r, 6, "0000:18:00.0", false);

        let map = discover(r, &Sysfs::new(r), Naming::Alias);
        assert_eq!(map.len(), 2);
        assert_eq!(nums(&map["nvidia.com/gpu"]), vec![vec![2]]);
        assert_eq!(
            nums(&map["nvidia.com/nvswitch-fabric"]),
            vec![vec![1, 3, 4, 5]]
        );
    }

    #[test]
    fn h100_fabric_is_the_four_switch_chips_as_one_device() {
        // Four PCI devices, one fabric: the Service VM takes them together
        // or not at all, so they are one advertised device.
        let root = TempDir::new().unwrap();
        let r = root.path();
        for n in [4u32, 1, 3, 2] {
            testfs::add_nvswitch(r, n);
        }

        let map = discover(r, &Sysfs::new(r), Naming::Alias);
        assert_eq!(
            nums(&map["nvidia.com/nvswitch-fabric"]),
            vec![vec![1, 2, 3, 4]]
        );
    }

    #[test]
    fn partially_bound_fabric_is_not_advertised() {
        // A switch chip that exists on the bus but has no cdev yet: the
        // trusted side is still binding, or bound only part of the fabric.
        // Either way no fabric is advertised, since a Service VM holding part
        // of one cannot bring it up; the GPU is unaffected.  Binding the last
        // chip completes it.
        let root = TempDir::new().unwrap();
        let r = root.path();
        testfs::add_gpu(r, 0);
        for n in 1..=4u32 {
            testfs::add_nvswitch(r, n);
        }
        std::fs::remove_file(r.join("devices/vfio4")).unwrap();

        let map = discover(r, &Sysfs::new(r), Naming::Alias);
        assert_eq!(map.len(), 1);
        assert_eq!(nums(&map["nvidia.com/gpu"]), vec![vec![0]]);

        std::fs::write(r.join("devices/vfio4"), b"").unwrap();
        let map = discover(r, &Sysfs::new(r), Naming::Alias);
        assert_eq!(
            nums(&map["nvidia.com/nvswitch-fabric"]),
            vec![vec![1, 2, 3, 4]]
        );

        // The same for the management ConnectX: two of its four PFs bound.
        let root = TempDir::new().unwrap();
        let r = root.path();
        for function in 0..4u32 {
            testfs::add_connectx(r, function, &format!("0000:05:00.{function}"), function < 2);
        }
        for function in 2..4u32 {
            std::fs::remove_file(r.join(format!("devices/vfio{function}"))).unwrap();
        }
        assert!(discover(r, &Sysfs::new(r), Naming::Alias).is_empty());
    }

    #[test]
    fn fabric_scan_failure_advertises_no_fabric() {
        // A directory where a VPD attribute should be fails the read the way
        // an unreadable sysfs attribute does.  The scan is all or nothing,
        // so the marked PF and a bound switch chip are withheld too, while
        // the GPU row is untouched.
        let root = TempDir::new().unwrap();
        let r = root.path();
        testfs::add_gpu(r, 0);
        testfs::add_connectx(r, 1, "0000:05:00.0", true);
        testfs::add_connectx(r, 2, "0000:05:00.1", false);
        testfs::add_nvswitch(r, 3);
        let vpd = Sysfs::new(r).device("0000:05:00.1").unwrap().join("vpd");
        std::fs::create_dir(vpd).unwrap();

        let map = discover(r, &Sysfs::new(r), Naming::Alias);
        assert_eq!(map.len(), 1);
        assert_eq!(nums(&map["nvidia.com/gpu"]), vec![vec![0]]);
    }

    #[test]
    fn sku_mode_keeps_the_alias_for_the_fabric() {
        // A fabric spans PCI devices; a ConnectX SKU would misname it.
        let root = TempDir::new().unwrap();
        let r = root.path();
        testfs::add_connectx(r, 0, "0000:05:00.0", true);

        let map = discover(r, &Sysfs::new(r), Naming::Sku);
        assert_eq!(
            map.keys().collect::<Vec<_>>(),
            ["nvidia.com/nvswitch-fabric"]
        );
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
    fn unit_contract_spelling() {
        assert_eq!(Unit::PciDevice.as_str(), "pci-device");
        assert_eq!(Unit::Fabric.as_str(), "fabric");
    }

    #[test]
    fn naming_parses_only_the_two_modes() {
        assert!(matches!(Naming::parse("alias"), Ok(Naming::Alias)));
        assert!(matches!(Naming::parse("sku"), Ok(Naming::Sku)));
        assert!(Naming::parse("SKU").is_err());
        assert!(Naming::parse("").is_err());
    }
}
