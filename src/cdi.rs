use std::path::{Path, PathBuf};

use anyhow::Context;
use container_device_interface::spec::validate_spec;
use container_device_interface::specs::config::{
    ContainerEdits, Device, DeviceNode, Spec, CURRENT_VERSION,
};
use tracing::info;

use pcilibs_rs::IommufdDev;

/// Path of the CDI spec file for `resource_name` inside `cdi_dir`:
/// "nvidia.com/gpu" → `<cdi_dir>/kata.nvidia.com-gpu.yaml`.  The `kata.`
/// prefix marks the specs this plugin owns in the shared host registry.
pub fn spec_path(cdi_dir: &Path, resource_name: &str) -> PathBuf {
    cdi_dir.join(format!("kata.{}.yaml", resource_name.replace('/', "-")))
}

/// Write the host CDI spec mapping index `i` → `devices[i]` — the same IDs
/// the plugin advertises.  Kata's shim reads this registry at SandboxCreate
/// to resolve `nvidia.com/gpu=0` → `/dev/vfio/devices/vfio<N>` for VFIO
/// passthrough into the hypervisor.  The CDI `kind` is the extended-resource
/// name itself.
pub fn write_cdi_spec(
    resource_name: &str,
    devices: &[IommufdDev],
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
        version: CURRENT_VERSION.to_owned(),
        kind: resource_name.to_owned(),
        devices: devices
            .iter()
            .enumerate()
            .map(|(idx, dev)| Device {
                name: idx.to_string(),
                container_edits: ContainerEdits {
                    device_nodes: Some(vec![DeviceNode {
                        path: dev.path.to_string_lossy().into_owned(),
                        r#type: Some("c".to_owned()),
                        permissions: Some("rw".to_owned()),
                        ..Default::default()
                    }]),
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
    let out_yaml = serde_yaml::to_string(&spec).context("serialize CDI spec")?;
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
    use crate::vfio::{self, testfs, Resource, RESOURCES};
    use pcilibs_rs::IommufdDev;
    use tempfile::TempDir;

    fn by_name(name: &str) -> &'static Resource {
        RESOURCES.iter().find(|r| r.name == name).unwrap()
    }

    fn gpu_devs(root: &TempDir, nums: &[u32]) -> Vec<IommufdDev> {
        for n in nums {
            testfs::add_gpu(root.path(), *n);
        }
        vfio::enumerate(
            root.path(),
            &testfs::sysfs(root.path()),
            by_name("nvidia.com/gpu"),
        )
    }

    #[test]
    fn writes_valid_yaml_for_two_devices() {
        let root = TempDir::new().unwrap();
        let cdi_dir = TempDir::new().unwrap();

        write_cdi_spec("nvidia.com/gpu", &gpu_devs(&root, &[8, 9]), cdi_dir.path()).unwrap();

        let out = cdi_dir.path().join("kata.nvidia.com-gpu.yaml");
        assert!(out.exists(), "CDI spec file not written");

        let contents = std::fs::read_to_string(&out).unwrap();
        assert!(contents.contains("kind: nvidia.com/gpu"));
        // serde_yaml quotes numeric-looking strings with single quotes.
        assert!(contents.contains("name: '0'"));
        assert!(contents.contains("name: '1'"));
        assert!(contents.contains("vfio8"));
        assert!(contents.contains("vfio9"));
        // Only the passthrough cdevs are edited in — nothing else.
        assert!(!contents.contains("/dev/iommu"));
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
        // Sorted numerically: vfio7, vfio42, vfio100 → indices 0, 1, 2
        assert!(contents.contains("name: '0'"));
        assert!(contents.contains("name: '1'"));
        assert!(contents.contains("name: '2'"));
        // vfio7 must map to index 0 (it's the lowest)
        let idx0 = contents.find("name: '0'").unwrap();
        let vfio7_pos = contents.find("vfio7").unwrap();
        assert!(vfio7_pos > idx0, "vfio7 should appear after index 0 entry");
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
        let devs = vfio::enumerate(
            root.path(),
            &testfs::sysfs(root.path()),
            by_name("nvidia.com/nvswitch"),
        );

        write_cdi_spec("nvidia.com/nvswitch", &devs, cdi_dir.path()).unwrap();

        assert!(cdi_dir
            .path()
            .join("kata.nvidia.com-nvswitch.yaml")
            .exists());
    }
}
