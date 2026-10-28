use std::path::{Path, PathBuf};

use anyhow::Context;
use container_device_interface::spec::validate_spec;
use container_device_interface::specs::config::{ContainerEdits, Device, DeviceNode, Spec};
use tracing::info;

use crate::vfio;

/// Path of the CDI spec file for `resource_name` inside `cdi_dir`:
/// "nvidia.com/gpu" → `<cdi_dir>/kata.nvidia.com-gpu.yaml`.  The `kata.`
/// prefix marks the specs this plugin owns in the shared host registry.
pub fn spec_path(cdi_dir: &Path, resource_name: &str) -> PathBuf {
    cdi_dir.join(format!("kata.{}.yaml", resource_name.replace('/', "-")))
}

/// Write the host CDI spec mapping index `i` → `devices[i]` — the same IDs
/// the plugin advertises.  Kata's shim reads this registry at SandboxCreate
/// to resolve `nvidia.com/gpu=0` → `/dev/vfio/devices/vfio<N>` for VFIO
/// passthrough into the hypervisor.  Every bound function of a device is a
/// device node, so a multi-function PCI device is injected whole.  The CDI
/// `kind` is the extended-resource name itself.
pub fn write_cdi_spec(
    resource_name: &str,
    devices: &[vfio::Device],
    cdi_dir: &Path,
) -> anyhow::Result<()> {
    if devices.is_empty() {
        info!(
            resource = resource_name,
            "no devices, skipping CDI spec write"
        );
        return Ok(());
    }

    let spec = Spec {
        version: "0.6.0".to_owned(),
        kind: resource_name.to_owned(),
        devices: devices
            .iter()
            .enumerate()
            .map(|(idx, dev)| Device {
                name: idx.to_string(),
                container_edits: ContainerEdits {
                    device_nodes: Some(
                        dev.iter()
                            .map(|function| DeviceNode {
                                path: function.path.to_string_lossy().into_owned(),
                                r#type: Some("c".to_owned()),
                                permissions: Some("rw".to_owned()),
                                ..Default::default()
                            })
                            .collect(),
                    ),
                    ..Default::default()
                },
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    validate_spec(&spec).context("validate CDI spec")?;

    std::fs::create_dir_all(cdi_dir).with_context(|| format!("create {}", cdi_dir.display()))?;

    let out_path = spec_path(cdi_dir, resource_name);
    let out_yaml = serde_saphyr::to_string(&spec).context("serialize CDI spec")?;
    // Write-then-rename: the ListAndWatch poller and Allocate may both write
    // this spec, and the Kata shim may read it at any moment — a reader must
    // never see a torn file.  The tmp name is unique per write (pid +
    // counter) so concurrent writers can't rename each other's bytes or trip
    // over a shared tmp path.
    static WRITE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = WRITE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp_path = out_path.with_extension(format!("yaml.{}.{seq}.tmp", std::process::id()));
    std::fs::write(&tmp_path, out_yaml).with_context(|| format!("write {}", tmp_path.display()))?;
    if let Err(e) = std::fs::rename(&tmp_path, &out_path) {
        // Best effort: don't litter the registry with tmp files on retries.
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e).with_context(|| format!("rename to {}", out_path.display()));
    }

    info!(
        path = %out_path.display(),
        devices = devices.len(),
        "wrote CDI spec"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfio::{self, testfs, Naming, Sysfs};
    use tempfile::TempDir;

    fn gpu_devs(root: &TempDir, nums: &[u32]) -> Vec<vfio::Device> {
        for n in nums {
            testfs::add_gpu(root.path(), *n);
        }
        vfio::discover(root.path(), &Sysfs::new(root.path()), Naming::Alias)
            .remove("nvidia.com/gpu")
            .unwrap_or_default()
    }

    #[test]
    fn writes_valid_yaml_for_two_devices() {
        let root = TempDir::new().unwrap();
        let cdi_dir = TempDir::new().unwrap();

        write_cdi_spec("nvidia.com/gpu", &gpu_devs(&root, &[8, 9]), cdi_dir.path()).unwrap();

        let out = cdi_dir.path().join("kata.nvidia.com-gpu.yaml");
        assert!(out.exists(), "CDI spec file not written");

        let contents = std::fs::read_to_string(&out).unwrap();
        let spec: Spec = serde_saphyr::from_str(&contents).unwrap();
        let yaml: serde_json::Value = serde_saphyr::from_str(&contents).unwrap();
        validate_spec(&spec).unwrap();
        assert_eq!(spec.version, "0.6.0");
        assert_eq!(spec.kind, "nvidia.com/gpu");
        assert_eq!(spec.devices.len(), 2);
        for (idx, device) in spec.devices.iter().enumerate() {
            assert_eq!(device.name, idx.to_string());
            assert_eq!(
                yaml["devices"][idx]["name"].as_str(),
                Some(device.name.as_str())
            );
            let nodes = device.container_edits.device_nodes.as_ref().unwrap();
            assert_eq!(nodes.len(), 1);
            assert_eq!(
                Path::new(&nodes[0].path),
                root.path().join(format!("devices/vfio{}", idx + 8))
            );
            assert_eq!(nodes[0].r#type.as_deref(), Some("c"));
            assert_eq!(nodes[0].permissions.as_deref(), Some("rw"));
        }
    }

    #[test]
    fn indices_are_sequential_regardless_of_vfio_numbers() {
        // vfio numbers can be sparse; CDI indices must be 0, 1, 2, ...
        let root = TempDir::new().unwrap();
        let cdi_dir = TempDir::new().unwrap();

        write_cdi_spec(
            "nvidia.com/gpu",
            &gpu_devs(&root, &[42, 7, 100]),
            cdi_dir.path(),
        )
        .unwrap();

        let contents =
            std::fs::read_to_string(cdi_dir.path().join("kata.nvidia.com-gpu.yaml")).unwrap();
        let spec: Spec = serde_saphyr::from_str(&contents).unwrap();
        assert_eq!(spec.devices.len(), 3);
        for (idx, num) in [7, 42, 100].iter().enumerate() {
            let device = &spec.devices[idx];
            assert_eq!(device.name, idx.to_string());
            let nodes = device.container_edits.device_nodes.as_ref().unwrap();
            assert_eq!(
                Path::new(&nodes[0].path),
                root.path().join(format!("devices/vfio{num}"))
            );
        }
    }

    #[test]
    fn no_devices_skips_write() {
        let cdi_dir = TempDir::new().unwrap();

        write_cdi_spec("nvidia.com/gpu", &[], cdi_dir.path()).unwrap();

        assert!(
            !cdi_dir.path().join("kata.nvidia.com-gpu.yaml").exists(),
            "should not write spec when no devices"
        );
    }

    #[test]
    fn nvswitch_resource_name() {
        let root = TempDir::new().unwrap();
        let cdi_dir = TempDir::new().unwrap();
        for n in [3u32, 4] {
            testfs::add_nvswitch(root.path(), n);
        }
        let devs = vfio::discover(root.path(), &Sysfs::new(root.path()), Naming::Alias)
            .remove("nvidia.com/nvswitch-fabric")
            .unwrap_or_default();

        write_cdi_spec("nvidia.com/nvswitch-fabric", &devs, cdi_dir.path()).unwrap();

        assert!(cdi_dir
            .path()
            .join("kata.nvidia.com-nvswitch-fabric.yaml")
            .exists());
    }

    #[test]
    fn fabric_is_one_cdi_device_with_a_node_per_function() {
        // The B200/B300 management ConnectX: four PFs, one fabric.  The shim
        // must inject all of them from one CDI name.
        let root = TempDir::new().unwrap();
        let cdi_dir = TempDir::new().unwrap();
        for function in 0..4u32 {
            testfs::add_connectx(
                root.path(),
                function,
                &format!("0000:05:00.{function}"),
                function < 2,
            );
        }
        let devs = vfio::discover(root.path(), &Sysfs::new(root.path()), Naming::Alias)
            .remove("nvidia.com/nvswitch-fabric")
            .unwrap_or_default();

        write_cdi_spec("nvidia.com/nvswitch-fabric", &devs, cdi_dir.path()).unwrap();

        let contents =
            std::fs::read_to_string(cdi_dir.path().join("kata.nvidia.com-nvswitch-fabric.yaml"))
                .unwrap();
        let spec: Spec = serde_saphyr::from_str(&contents).unwrap();
        validate_spec(&spec).unwrap();
        assert_eq!(spec.devices.len(), 1);
        assert_eq!(spec.devices[0].name, "0");
        let nodes = spec.devices[0]
            .container_edits
            .device_nodes
            .as_ref()
            .unwrap();
        let cdevs: Vec<&str> = nodes
            .iter()
            .map(|node| node.path.rsplit('/').next().unwrap())
            .collect();
        assert_eq!(cdevs, ["vfio0", "vfio1", "vfio2", "vfio3"]);
    }
}
