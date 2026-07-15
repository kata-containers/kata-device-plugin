use std::path::Path;

use anyhow::Context;
use container_device_interface::spec::validate_spec;
use container_device_interface::specs::config::Spec as CdiSpec;
use tracing::info;

/// Write `/var/run/cdi/<vendor>-<class>.yaml` (e.g. `nvidia.com-gpu.yaml`)
/// mapping sequential device indices to IOMMUFD char device paths.
///
/// The host CDI registry is what Kata's shim reads at SandboxCreate to
/// resolve `nvidia.com/gpu=0` → `/dev/vfio/devices/vfio<N>` for VFIO
/// passthrough into the hypervisor.
///
/// `resource_name` must be the Kubernetes extended-resource name
/// (e.g. `"nvidia.com/gpu"`); the CDI `kind` field is set to the same value.
///
/// `vfio_dir` is the parent VFIO directory (e.g. `/dev/vfio`); this
/// function scans `<vfio_dir>/devices/` for `vfio[0-9]+` entries.
pub fn write_cdi_spec(
    resource_name: &str,
    vfio_dir: &Path,
    cdi_dir: &Path,
) -> anyhow::Result<()> {
    let devices_dir = vfio_dir.join("devices");

    let mut vfio_nums: Vec<u32> = std::fs::read_dir(&devices_dir)
        .with_context(|| format!("read {}", devices_dir.display()))?
        .flatten()
        .filter_map(|e| {
            e.file_name().to_str()?.strip_prefix("vfio")?.parse::<u32>().ok()
        })
        .collect();
    vfio_nums.sort();

    if vfio_nums.is_empty() {
        info!("no IOMMUFD devices found, skipping CDI spec write");
        return Ok(());
    }

    // Build YAML via string template — CDI crate fields are pub(crate) so
    // struct-literal construction from external crates is not possible.
    let devices_yaml: String = vfio_nums
        .iter()
        .enumerate()
        .map(|(idx, n)| {
            let path = devices_dir.join(format!("vfio{n}")).to_string_lossy().into_owned();
            format!(
                "  - name: \"{idx}\"\n\
                 \x20   containerEdits:\n\
                 \x20     deviceNodes:\n\
                 \x20       - path: {path}\n\
                 \x20         type: c\n\
                 \x20         permissions: rw\n"
            )
        })
        .collect();

    // /dev/iommu is the IOMMUFD control device; all containers using IOMMUFD
    // devices need it, so it lives in the top-level containerEdits.
    let yaml = format!(
        "cdiVersion: \"1.1.0\"\n\
         kind: \"{resource_name}\"\n\
         devices:\n\
         {devices_yaml}\
         containerEdits:\n\
         \x20 deviceNodes:\n\
         \x20   - path: /dev/iommu\n\
         \x20     type: c\n\
         \x20     permissions: rw\n"
    );

    let spec: CdiSpec = serde_yaml::from_str(&yaml).context("parse CDI spec")?;
    validate_spec(&spec).context("validate CDI spec")?;

    std::fs::create_dir_all(cdi_dir)
        .with_context(|| format!("create {}", cdi_dir.display()))?;

    // File name: "nvidia.com/gpu" → "nvidia.com-gpu.yaml"
    let file_name = format!("{}.yaml", resource_name.replace('/', "-"));
    let out_path = cdi_dir.join(&file_name);
    let out_yaml = serde_yaml::to_string(&spec).context("serialize CDI spec")?;
    std::fs::write(&out_path, out_yaml)
        .with_context(|| format!("write {}", out_path.display()))?;

    info!(
        path = %out_path.display(),
        devices = vfio_nums.len(),
        "wrote CDI spec"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fake_vfio_devices(nums: &[u32]) -> TempDir {
        let dir = TempDir::new().unwrap();
        let devices = dir.path().join("devices");
        std::fs::create_dir(&devices).unwrap();
        for n in nums {
            std::fs::write(devices.join(format!("vfio{n}")), b"").unwrap();
        }
        dir
    }

    #[test]
    fn writes_valid_yaml_for_two_devices() {
        let vfio = fake_vfio_devices(&[8, 9]);
        let cdi_dir = TempDir::new().unwrap();

        write_cdi_spec("nvidia.com/gpu", vfio.path(), cdi_dir.path()).unwrap();

        let out = cdi_dir.path().join("nvidia.com-gpu.yaml");
        assert!(out.exists(), "CDI spec file not written");

        let contents = std::fs::read_to_string(&out).unwrap();
        assert!(contents.contains("kind: nvidia.com/gpu"));
        assert!(contents.contains("name: \"0\""));
        assert!(contents.contains("name: \"1\""));
        assert!(contents.contains("vfio8"));
        assert!(contents.contains("vfio9"));
        assert!(contents.contains("/dev/iommu"));
    }

    #[test]
    fn indices_are_sequential_regardless_of_vfio_numbers() {
        // vfio numbers can be sparse; CDI indices must be 0, 1, 2, ...
        let vfio = fake_vfio_devices(&[42, 7, 100]);
        let cdi_dir = TempDir::new().unwrap();

        write_cdi_spec("nvidia.com/gpu", vfio.path(), cdi_dir.path()).unwrap();

        let contents =
            std::fs::read_to_string(cdi_dir.path().join("nvidia.com-gpu.yaml")).unwrap();
        // Sorted numerically: vfio7, vfio42, vfio100 → indices 0, 1, 2
        assert!(contents.contains("name: \"0\""));
        assert!(contents.contains("name: \"1\""));
        assert!(contents.contains("name: \"2\""));
        // vfio7 must map to index 0 (it's the lowest)
        let idx0 = contents.find("name: \"0\"").unwrap();
        let vfio7_pos = contents.find("vfio7").unwrap();
        assert!(vfio7_pos > idx0, "vfio7 should appear after index 0 entry");
    }

    #[test]
    fn no_devices_skips_write() {
        let vfio = fake_vfio_devices(&[]);
        let cdi_dir = TempDir::new().unwrap();

        write_cdi_spec("nvidia.com/gpu", vfio.path(), cdi_dir.path()).unwrap();

        assert!(
            !cdi_dir.path().join("nvidia.com-gpu.yaml").exists(),
            "should not write spec when no devices"
        );
    }

    #[test]
    fn nvswitch_resource_name() {
        let vfio = fake_vfio_devices(&[3, 4]);
        let cdi_dir = TempDir::new().unwrap();

        write_cdi_spec("nvidia.com/nvswitch", vfio.path(), cdi_dir.path()).unwrap();

        assert!(cdi_dir.path().join("nvidia.com-nvswitch.yaml").exists());
    }
}
