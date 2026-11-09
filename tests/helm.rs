use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn template(args: &[&str]) -> std::process::Output {
    Command::new("helm")
        .args(["template", "compatibility-check"])
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("deploy/helm/kata-device-plugin"))
        .args(args)
        .output()
        .expect("chart tests require Helm on PATH")
}

#[test]
fn distribution_host_paths_preserve_container_contracts() {
    for (args, socket_dir, cdi_dir) in [
        (vec![], "/var/lib/kubelet/device-plugins", "/var/run/cdi"),
        (
            vec!["--set-string", "k8sDistribution=k8s"],
            "/var/lib/kubelet/device-plugins",
            "/var/run/cdi",
        ),
        (
            vec!["--set-string", "k8sDistribution=k3s"],
            "/var/lib/kubelet/device-plugins",
            "/var/run/cdi",
        ),
        (
            vec!["--set-string", "k8sDistribution=rke2"],
            "/var/lib/kubelet/device-plugins",
            "/var/run/cdi",
        ),
        (
            vec!["--set-string", "k8sDistribution=k0s"],
            "/var/lib/kubelet/device-plugins",
            "/var/run/cdi",
        ),
        (
            vec!["--set-string", "k8sDistribution=microk8s"],
            "/var/snap/microk8s/common/var/lib/kubelet/device-plugins",
            "/var/snap/microk8s/common/run/cdi",
        ),
        (
            vec![
                "--set-string",
                "k8sDistribution=microk8s",
                "--set-string",
                "hostPaths.devicePlugin=/custom/device-plugins",
                "--set-string",
                "hostPaths.cdi=/custom/cdi",
                "--set",
                "resourceNaming=sku",
            ],
            "/custom/device-plugins",
            "/custom/cdi",
        ),
        (
            vec![
                "--set-string",
                "k8sDistribution=microk8s",
                "--set-string",
                "hostPaths.devicePlugin=/custom/device-plugins",
            ],
            "/custom/device-plugins",
            "/var/snap/microk8s/common/run/cdi",
        ),
        (
            vec![
                "--set-string",
                "k8sDistribution=microk8s",
                "--set-string",
                "hostPaths.cdi=/custom/cdi",
            ],
            "/var/snap/microk8s/common/var/lib/kubelet/device-plugins",
            "/custom/cdi",
        ),
    ] {
        let output = template(&args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let ds: Value =
            serde_saphyr::from_str(std::str::from_utf8(&output.stdout).unwrap()).unwrap();
        let pod = &ds["spec"]["template"]["spec"];
        let container = &pod["containers"][0];
        let volumes = pod["volumes"].as_array().unwrap();
        let mounts = container["volumeMounts"].as_array().unwrap();
        for (name, host, target, kind) in [
            (
                "kubelet-socket",
                socket_dir,
                "/var/lib/kubelet/device-plugins",
                "Directory",
            ),
            ("cdi", cdi_dir, "/var/run/cdi", "DirectoryOrCreate"),
            ("vfio", "/dev/vfio", "/dev/vfio", "DirectoryOrCreate"),
        ] {
            let volume = volumes.iter().find(|v| v["name"] == name).unwrap();
            let mount = mounts.iter().find(|v| v["name"] == name).unwrap();
            assert_eq!(volume["hostPath"]["path"], host);
            assert_eq!(volume["hostPath"]["type"], kind);
            assert_eq!(mount["mountPath"], target);
            assert_eq!(mount["readOnly"].as_bool().unwrap_or(false), name == "vfio");
        }
        assert_eq!(volumes.len(), 3);
        assert_eq!(mounts.len(), 3);
        for key in ["hostPID", "hostNetwork"] {
            assert!(!pod[key].as_bool().unwrap_or(false));
        }
        assert_eq!(pod["automountServiceAccountToken"], false);
        let security = &container["securityContext"];
        assert!(!security["privileged"].as_bool().unwrap_or(false));
        assert_eq!(security["allowPrivilegeEscalation"], false);
        assert_eq!(security["readOnlyRootFilesystem"], true);
        assert_eq!(security["capabilities"]["drop"], serde_json::json!(["ALL"]));
    }
}

#[test]
fn host_directory_overrides_must_be_absolute() {
    for key in ["devicePlugin", "cdi"] {
        for value in ["/", "relative/path", "/path\nother"] {
            let output = template(&["--set-string", &format!("hostPaths.{key}={value}")]);
            assert!(
                !output.status.success(),
                "accepted hostPaths.{key}={value:?}"
            );
            assert!(String::from_utf8_lossy(&output.stderr).contains("hostPaths"));
        }
    }
}

#[test]
fn unknown_distribution_is_rejected() {
    let output = template(&["--set-string", "k8sDistribution=microk8"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("k8sDistribution"));
}
