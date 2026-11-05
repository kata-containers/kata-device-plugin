//! The manifests cannot import the contract constants, so they repeat them;
//! these tests keep the copies equal to pcilibs-rs.

use pcilibs_rs::fabric::{FM_VSOCK_PORT, SERVICE_VM_CID};

const IN_BAND: &str = include_str!("../deploy/service-vm.yaml");
const OUT_OF_BAND: &str = include_str!("../deploy/service-vm-out-of-band.yaml");

#[test]
fn the_out_of_band_manifest_carries_the_contract_cid() {
    let annotation = format!("io.katacontainers.config.hypervisor.vsock_cid: \"{SERVICE_VM_CID}\"");
    assert!(OUT_OF_BAND.contains(&annotation), "{annotation}");
    assert!(OUT_OF_BAND.contains("nvrc.fm.vsock=on"));
    // FM's default port is dialed; the manifest must not override it.
    assert!(!OUT_OF_BAND.contains(&FM_VSOCK_PORT.to_string()));
}

#[test]
fn the_in_band_manifest_opens_no_vsock_listener() {
    assert!(!IN_BAND.contains("hypervisor.vsock_cid"));
    assert!(!IN_BAND.contains("nvrc.fm.vsock"));
    // The container idles; fmclient runs through kubectl exec.
    assert!(IN_BAND.contains(r#"command: ["sleep", "infinity"]"#));
}
