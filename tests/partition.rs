//! The shapes a published view gives the node's GPUs, through
//! `vfio::discover`: one device per active partition named by size, a
//! single as a plain GPU, Unhealthy where a partition is not a unit, and no
//! GPU at all where the view or the labels cannot be trusted.

mod common;

use std::path::Path;

use kata_device_plugin::vfio::{discover, Entry, Health, Naming};
use kata_fabric_api::v1::{self, FabricView};
use pcilibs_rs::Sysfs;
use tempfile::TempDir;

use common::*;

/// The first four GPUs of a DGX NVL8, labeled as its firmware does:
/// cdev n at a slot whose module is Fabric Manager's physical id.
fn four_labeled_gpus(r: &Path) {
    for (n, address, module) in [
        (0, "0000:1a:00.0", 7),
        (1, "0000:3c:00.0", 6),
        (2, "0000:62:00.0", 8),
        (3, "0000:73:00.0", 5),
    ] {
        add_labeled_gpu(r, n, address, module);
    }
}

/// A view with the given partitions, `(id, active, members)`.
fn view(unit: &str, partitions: &[(u32, bool, &[u32])]) -> FabricView {
    fabric_view(unit, partitions)
}

/// The DGX table's two pairs over these GPUs: 6 = (5,6), 7 = (7,8).
const PAIRS: &[(u32, bool, &[u32])] = &[(6, true, &[5, 6]), (7, true, &[7, 8])];

/// Cdev numbers per device of the given health.
fn nums(entries: &[Entry], health: Health) -> Vec<Vec<u32>> {
    entries
        .iter()
        .filter(|entry| entry.health == health)
        .map(|entry| entry.device.iter().map(|cdev| cdev.num).collect())
        .collect()
}

fn healthy(entries: &[Entry]) -> Vec<Vec<u32>> {
    nums(entries, Health::Healthy)
}

fn unhealthy(entries: &[Entry]) -> Vec<Vec<u32>> {
    nums(entries, Health::Unhealthy)
}

fn ids(entries: &[Entry]) -> Vec<&str> {
    entries.iter().map(|entry| entry.id.as_str()).collect()
}

#[test]
fn active_partitions_are_one_device_each_named_by_size() {
    let root = TempDir::new().unwrap();
    let r = root.path();
    four_labeled_gpus(r);
    let map = discover(
        r,
        &Sysfs::new(r),
        Naming::Alias,
        Some(&view("partition", PAIRS)),
    );
    // Devices by partition id, functions in Fabric Manager's member order.
    assert_eq!(map.keys().collect::<Vec<_>>(), ["nvidia.com/nvlink-2gpu"]);
    let pairs = &map["nvidia.com/nvlink-2gpu"];
    assert_eq!(healthy(pairs), vec![vec![3, 1], vec![0, 2]]);
    assert_eq!(ids(pairs), ["p6", "p7"]);
    // An unset unit is the same.
    let map = discover(r, &Sysfs::new(r), Naming::Alias, Some(&view("", PAIRS)));
    assert_eq!(healthy(&map["nvidia.com/nvlink-2gpu"]).len(), 2);
}

#[test]
fn a_single_gpu_partition_is_a_plain_gpu_and_inactive_gpus_are_withheld() {
    let root = TempDir::new().unwrap();
    let r = root.path();
    four_labeled_gpus(r);
    let singles: &[(u32, bool, &[u32])] = &[
        (12, true, &[5]),
        (14, true, &[7]),
        (13, false, &[6]),
        (15, false, &[8]),
    ];
    let map = discover(
        r,
        &Sysfs::new(r),
        Naming::Alias,
        Some(&view("partition", singles)),
    );
    assert_eq!(map.keys().collect::<Vec<_>>(), ["nvidia.com/gpu"]);
    assert_eq!(healthy(&map["nvidia.com/gpu"]), vec![vec![3], vec![0]]);
    assert_eq!(ids(&map["nvidia.com/gpu"]), ["p12", "p14"]);
}

#[test]
fn a_half_and_two_singles_at_once() {
    let root = TempDir::new().unwrap();
    let r = root.path();
    four_labeled_gpus(r);
    let carve: &[(u32, bool, &[u32])] = &[
        (3, true, &[5, 6, 7, 8]),
        (12, false, &[5]),
        (14, false, &[7]),
    ];
    let map = discover(
        r,
        &Sysfs::new(r),
        Naming::Alias,
        Some(&view("partition", carve)),
    );
    assert_eq!(
        healthy(&map["nvidia.com/nvlink-4gpu"]),
        vec![vec![3, 1, 0, 2]]
    );
    assert!(!map.contains_key("nvidia.com/gpu"));
}

#[test]
fn incomplete_unsupported_and_overlapping_partitions_are_unhealthy() {
    let root = TempDir::new().unwrap();
    let r = root.path();
    four_labeled_gpus(r);
    // GPU 6 (cdev 1) is present but no longer bound: its pair is an
    // unhealthy device holding the member that is here, under the name
    // of the size Fabric Manager gave it; the other pair stands.
    std::fs::remove_file(r.join("devices/vfio1")).unwrap();
    let map = discover(
        r,
        &Sysfs::new(r),
        Naming::Alias,
        Some(&view("partition", PAIRS)),
    );
    let pairs = &map["nvidia.com/nvlink-2gpu"];
    assert_eq!(healthy(pairs), vec![vec![0, 2]]);
    assert_eq!(unhealthy(pairs), vec![vec![3]]);
    assert_eq!(ids(pairs), ["p6", "p7"]);

    // Fabric Manager lists pair 7 as unsupported: unhealthy as well, with
    // both members, so nothing is allocatable.
    let mut unsupported = view("partition", PAIRS);
    unsupported.unsupported = Some(v1::UnsupportedPartitionList {
        version: 1,
        partition_info: vec![v1::UnsupportedPartition {
            partition_id: 7,
            gpu_physical_ids: vec![7, 8],
        }],
    });
    let map = discover(r, &Sysfs::new(r), Naming::Alias, Some(&unsupported));
    let pairs = &map["nvidia.com/nvlink-2gpu"];
    assert!(healthy(pairs).is_empty());
    assert_eq!(unhealthy(pairs), vec![vec![3], vec![0, 2]]);

    // Two active partitions naming GPU 7: neither is a unit, both are
    // unhealthy, the single under the plain GPU name.
    std::fs::write(r.join("devices/vfio1"), b"").unwrap();
    let overlap: &[(u32, bool, &[u32])] =
        &[(7, true, &[7, 8]), (14, true, &[7]), (6, true, &[5, 6])];
    let map = discover(
        r,
        &Sysfs::new(r),
        Naming::Alias,
        Some(&view("partition", overlap)),
    );
    assert_eq!(
        map.keys().collect::<Vec<_>>(),
        ["nvidia.com/gpu", "nvidia.com/nvlink-2gpu"]
    );
    let pairs = &map["nvidia.com/nvlink-2gpu"];
    assert_eq!(healthy(pairs), vec![vec![3, 1]]);
    assert_eq!(unhealthy(pairs), vec![vec![0, 2]]);
    let single = &map["nvidia.com/gpu"];
    assert!(healthy(single).is_empty());
    assert_eq!(unhealthy(single), vec![vec![0]]);
    assert_eq!(ids(single), ["p14"]);
}

#[test]
fn an_unlabeled_gpu_takes_its_partition_down_not_the_others() {
    let root = TempDir::new().unwrap();
    let r = root.path();
    four_labeled_gpus(r);
    std::fs::remove_file(Sysfs::new(r).device("0000:62:00.0").unwrap().join("label")).unwrap();
    let map = discover(
        r,
        &Sysfs::new(r),
        Naming::Alias,
        Some(&view("partition", PAIRS)),
    );
    let pairs = &map["nvidia.com/nvlink-2gpu"];
    assert_eq!(healthy(pairs), vec![vec![3, 1]]);
    assert_eq!(unhealthy(pairs), vec![vec![0]]);
}

#[test]
fn a_partition_with_no_member_here_is_not_advertised() {
    // The DGX node partition names GPUs 1 to 4 too, which this host does
    // not have; there is no row to name it by.
    let root = TempDir::new().unwrap();
    let r = root.path();
    four_labeled_gpus(r);
    let elsewhere: &[(u32, bool, &[u32])] = &[(2, true, &[1, 2, 3, 4]), (6, true, &[5, 6])];
    let map = discover(
        r,
        &Sysfs::new(r),
        Naming::Alias,
        Some(&view("partition", elsewhere)),
    );
    assert_eq!(map.keys().collect::<Vec<_>>(), ["nvidia.com/nvlink-2gpu"]);
    assert_eq!(ids(&map["nvidia.com/nvlink-2gpu"]), ["p6"]);
}

#[test]
fn unit_gpu_is_one_device_per_gpu_whatever_the_partitions() {
    let root = TempDir::new().unwrap();
    let r = root.path();
    four_labeled_gpus(r);
    let map = discover(r, &Sysfs::new(r), Naming::Alias, Some(&view("gpu", PAIRS)));
    assert_eq!(map.keys().collect::<Vec<_>>(), ["nvidia.com/gpu"]);
    assert_eq!(
        healthy(&map["nvidia.com/gpu"]),
        vec![vec![0], vec![1], vec![2], vec![3]]
    );
    assert_eq!(
        ids(&map["nvidia.com/gpu"]),
        ["0000:1a:00", "0000:3c:00", "0000:62:00", "0000:73:00"]
    );
}

#[test]
fn an_unknown_unit_or_unreadable_labels_advertise_no_gpu() {
    let root = TempDir::new().unwrap();
    let r = root.path();
    four_labeled_gpus(r);
    assert!(discover(r, &Sysfs::new(r), Naming::Alias, Some(&view("tray", PAIRS))).is_empty());
    // A label that cannot be read at all is not an unlabeled board: fail.
    let label = Sysfs::new(r).device("0000:1a:00.0").unwrap().join("label");
    std::fs::remove_file(&label).unwrap();
    std::fs::create_dir(&label).unwrap();
    assert!(discover(
        r,
        &Sysfs::new(r),
        Naming::Alias,
        Some(&view("partition", PAIRS))
    )
    .is_empty());
}

#[test]
fn sku_mode_names_singles_by_sku_and_partitions_by_size() {
    let root = TempDir::new().unwrap();
    let r = root.path();
    four_labeled_gpus(r);
    let mixed: &[(u32, bool, &[u32])] = &[(6, true, &[5, 6]), (14, true, &[7])];
    let map = discover(
        r,
        &Sysfs::new(r),
        Naming::Sku,
        Some(&view("partition", mixed)),
    );
    let names: Vec<&String> = map.keys().collect();
    assert_eq!(names.len(), 2);
    assert!(
        names.iter().any(|n| n.starts_with("nvidia.com/GH100")),
        "{names:?}"
    );
    assert_eq!(healthy(&map["nvidia.com/nvlink-2gpu"]), vec![vec![3, 1]]);
}
