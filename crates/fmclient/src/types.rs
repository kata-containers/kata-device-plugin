// SPDX-License-Identifier: Apache-2.0
// Copyright (c) NVIDIA CORPORATION

//! The FM SDK structs filled into the fabric view's wire types, which live
//! with their consumer in pcilibs-rs, and rendered as tables for a terminal.
//! The JSON rendering is serde's, so what this client prints is what it
//! will stream.

use std::fmt::{self, Write as _};
use std::os::raw::c_char;

use pcilibs_rs::fabric::v1;
use serde::Serialize;

use crate::ffi;

fn cstr_from_buf(buf: &[c_char]) -> String {
    // The SDK NUL-terminates within the buffer; a full buffer without a NUL
    // would read past it, so bound the scan to the buffer.
    let bytes: Vec<u8> = buf.iter().map(|&c| c as u8).collect();
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// The SDK reports a count beside a fixed-size array; a count beyond the
/// array is a malformed reply, not a reason to read past the end.
fn counted<T>(items: &[T], count: u32) -> &[T] {
    &items[..(count as usize).min(items.len())]
}

pub fn partition_list(list: &ffi::FmFabricPartitionList) -> v1::PartitionList {
    v1::PartitionList {
        version: list.version,
        max_num_partitions: list.max_num_partitions,
        partition_info: counted(&list.partition_info, list.num_partitions)
            .iter()
            .map(partition)
            .collect(),
    }
}

fn partition(p: &ffi::FmFabricPartitionInfo) -> v1::Partition {
    v1::Partition {
        partition_id: p.partition_id,
        is_active: p.is_active != 0,
        gpu_info: counted(&p.gpu_info, p.num_gpus).iter().map(gpu).collect(),
    }
}

fn gpu(g: &ffi::FmFabricPartitionGpuInfo) -> v1::PartitionGpu {
    v1::PartitionGpu {
        physical_id: g.physical_id,
        uuid: cstr_from_buf(&g.uuid),
        pci_bus_id: cstr_from_buf(&g.pci_bus_id),
        num_nvlinks_available: g.num_nvlinks_available,
        max_num_nvlinks: g.max_num_nvlinks,
        nvlink_line_rate_mbps: g.nvlink_line_rate_mbps,
    }
}

pub fn unsupported_partition_list(
    list: &ffi::FmUnsupportedFabricPartitionList,
) -> v1::UnsupportedPartitionList {
    v1::UnsupportedPartitionList {
        version: list.version,
        partition_info: counted(&list.partition_info, list.num_partitions)
            .iter()
            .map(|p| v1::UnsupportedPartition {
                partition_id: p.partition_id,
                gpu_physical_ids: counted(&p.gpu_physical_ids, p.num_gpus).to_vec(),
            })
            .collect(),
    }
}

/// What `-o json` and `-o table` print.  The wire types are generated in
/// pcilibs-rs and cannot carry a `Display` of their own, so the table
/// rendering is a trait implemented here, beside the conversions.
pub trait Render: Serialize {
    fn table(&self) -> String;
}

impl Render for v1::PartitionList {
    fn table(&self) -> String {
        let list = self;
        let mut f = String::new();
        let _ = writeln!(
            f,
            "Partitions ({} supported, max {}):\n",
            list.partition_info.len(),
            list.max_num_partitions
        );
        let _ = writeln!(
            f,
            "  {:>3}  {:>6}  {:>4}  {:>7}  {:>11}  GPU IDs",
            "ID", "Active", "GPUs", "NVLinks", "Rate (MB/s)"
        );
        for p in &list.partition_info {
            let (avail, max, rate) = p.gpu_info.first().map_or((0, 0, 0), |g| {
                (
                    g.num_nvlinks_available,
                    g.max_num_nvlinks,
                    g.nvlink_line_rate_mbps,
                )
            });
            let _ = writeln!(
                f,
                "  {:>3}  {:>6}  {:>4}  {:>3}/{:<3}  {:>11}  {}",
                p.partition_id,
                if p.is_active { "yes" } else { "no" },
                p.gpu_info.len(),
                avail,
                max,
                rate,
                ids(p.gpu_info.iter().map(|g| g.physical_id))
            );
        }
        f
    }
}

impl Render for v1::UnsupportedPartitionList {
    fn table(&self) -> String {
        let list = self;
        let mut f = String::new();
        let _ = writeln!(
            f,
            "Unsupported Partitions ({}):\n",
            list.partition_info.len()
        );
        let _ = writeln!(f, "  {:>3}  {:>4}  GPU IDs", "ID", "GPUs");
        for p in &list.partition_info {
            let _ = writeln!(
                f,
                "  {:>3}  {:>4}  {}",
                p.partition_id,
                p.gpu_physical_ids.len(),
                ids(p.gpu_physical_ids.iter().copied())
            );
        }
        f
    }
}

impl Render for NvlinkFailedDevices {
    fn table(&self) -> String {
        self.to_string()
    }
}

fn ids(ids: impl Iterator<Item = u32>) -> String {
    ids.map(|id| id.to_string()).collect::<Vec<_>>().join(",")
}

/// FM's NVLink failure report.  Diagnostics for an operator, not part of
/// the view a host consumes, so it stays this client's own type.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NvlinkFailedDevices {
    pub version: u32,
    pub gpu_info: Vec<FailedDeviceInfo>,
    pub switch_info: Vec<FailedDeviceInfo>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FailedDeviceInfo {
    pub uuid: String,
    pub pci_bus_id: String,
    pub port_num: Vec<u32>,
}

impl From<&ffi::FmNvlinkFailedDeviceInfo> for FailedDeviceInfo {
    fn from(d: &ffi::FmNvlinkFailedDeviceInfo) -> Self {
        Self {
            uuid: cstr_from_buf(&d.uuid),
            pci_bus_id: cstr_from_buf(&d.pci_bus_id),
            port_num: counted(&d.port_num, d.num_ports).to_vec(),
        }
    }
}

impl From<&ffi::FmNvlinkFailedDevices> for NvlinkFailedDevices {
    fn from(d: &ffi::FmNvlinkFailedDevices) -> Self {
        Self {
            version: d.version,
            gpu_info: counted(&d.gpu_info, d.num_gpus)
                .iter()
                .map(FailedDeviceInfo::from)
                .collect(),
            switch_info: counted(&d.switch_info, d.num_switches)
                .iter()
                .map(FailedDeviceInfo::from)
                .collect(),
        }
    }
}

impl fmt::Display for NvlinkFailedDevices {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let print_devices = |f: &mut fmt::Formatter<'_>,
                             label: &str,
                             devices: &[FailedDeviceInfo]|
         -> fmt::Result {
            if devices.is_empty() {
                return writeln!(f, "{label}: none");
            }
            writeln!(f, "{label} ({}):\n", devices.len())?;
            writeln!(f, "  {:>32}  {:>16}  Failed Ports", "UUID", "PCI Bus ID")?;
            for d in devices {
                writeln!(
                    f,
                    "  {:>32}  {:>16}  {}",
                    d.uuid,
                    d.pci_bus_id,
                    ids(d.port_num.iter().copied())
                )?;
            }
            Ok(())
        };
        print_devices(f, "Failed GPUs", &self.gpu_info)?;
        writeln!(f)?;
        print_devices(f, "Failed NVSwitches", &self.switch_info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(buf: &mut [c_char], text: &str) {
        for (dst, src) in buf.iter_mut().zip(text.bytes()) {
            *dst = src as c_char;
        }
    }

    /// Two partitions as FM hands them back: the node, active, and a pair,
    /// with the array tails left zeroed as the SDK does.
    fn sdk_partitions() -> ffi::FmFabricPartitionList {
        let mut list = unsafe { std::mem::zeroed::<ffi::FmFabricPartitionList>() };
        list.version = 7;
        list.num_partitions = 2;
        list.max_num_partitions = 15;
        let node = &mut list.partition_info[0];
        node.partition_id = 1;
        node.is_active = 1;
        node.num_gpus = 8;
        for (i, gpu) in node.gpu_info.iter_mut().take(8).enumerate() {
            gpu.physical_id = i as u32 + 1;
            gpu.num_nvlinks_available = 36;
            gpu.max_num_nvlinks = 36;
            gpu.nvlink_line_rate_mbps = 41000;
        }
        fill(&mut node.gpu_info[0].uuid, "GPU-0b91d466");
        fill(&mut node.gpu_info[0].pci_bus_id, "00000000:F0:00.0");
        let pair = &mut list.partition_info[1];
        pair.partition_id = 4;
        pair.num_gpus = 2;
        pair.gpu_info[0].physical_id = 1;
        pair.gpu_info[1].physical_id = 2;
        list
    }

    #[test]
    fn partitions_fill_the_wire_type() {
        let list = partition_list(&sdk_partitions());
        assert_eq!((list.version, list.max_num_partitions), (7, 15));
        assert_eq!(list.partition_info.len(), 2);
        let node = &list.partition_info[0];
        assert!(node.is_active);
        assert_eq!(
            node.gpu_info
                .iter()
                .map(|g| g.physical_id)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4, 5, 6, 7, 8]
        );
        assert_eq!(node.gpu_info[0].uuid, "GPU-0b91d466");
        assert_eq!(node.gpu_info[0].pci_bus_id, "00000000:F0:00.0");
        assert_eq!(node.gpu_info[1].uuid, "");
        let pair = &list.partition_info[1];
        assert!(!pair.is_active);
        assert_eq!(pair.gpu_info.len(), 2);
    }

    #[test]
    fn json_is_the_shared_schema() {
        let json = serde_json::to_value(partition_list(&sdk_partitions())).unwrap();
        assert_eq!(json["partitionInfo"][0]["partitionId"], 1);
        assert_eq!(json["partitionInfo"][0]["isActive"], true);
        assert_eq!(json["partitionInfo"][0]["gpuInfo"][0]["physicalId"], 1);
        assert_eq!(
            json["partitionInfo"][0]["gpuInfo"][0]["nvlinkLineRateMBps"],
            41000
        );
        assert!(json.get("numPartitions").is_none());
    }

    #[test]
    fn a_count_beyond_the_array_is_clamped() {
        let mut list = sdk_partitions();
        list.num_partitions = u32::MAX;
        list.partition_info[0].num_gpus = 99;
        let list = partition_list(&list);
        assert_eq!(list.partition_info.len(), ffi::FM_MAX_FABRIC_PARTITIONS);
        assert_eq!(list.partition_info[0].gpu_info.len(), ffi::FM_MAX_NUM_GPUS);

        let mut buf = [b'x' as c_char; 4];
        assert_eq!(cstr_from_buf(&buf), "xxxx");
        buf[2] = 0;
        assert_eq!(cstr_from_buf(&buf), "xx");
    }

    #[test]
    fn tables_read_like_fmpm() {
        let table = partition_list(&sdk_partitions()).table();
        assert!(
            table.starts_with("Partitions (2 supported, max 15):\n"),
            "{table}"
        );
        assert!(
            table.contains("    1     yes     8   36/36         41000  1,2,3,4,5,6,7,8\n"),
            "{table}"
        );
        assert!(
            table.contains("    4      no     2    0/0              0  1,2\n"),
            "{table}"
        );

        let mut unsupported =
            unsafe { std::mem::zeroed::<ffi::FmUnsupportedFabricPartitionList>() };
        unsupported.num_partitions = 1;
        unsupported.partition_info[0].partition_id = 5;
        unsupported.partition_info[0].num_gpus = 2;
        unsupported.partition_info[0].gpu_physical_ids[0] = 7;
        unsupported.partition_info[0].gpu_physical_ids[1] = 5;
        let list = unsupported_partition_list(&unsupported);
        assert_eq!(list.partition_info[0].gpu_physical_ids, [7, 5]);
        let table = list.table();
        assert!(table.contains("Unsupported Partitions (1):\n"), "{table}");
        assert!(table.contains("    5     2  7,5\n"), "{table}");
    }

    #[test]
    fn nvlink_failures_render_with_and_without_entries() {
        let mut failed = unsafe { std::mem::zeroed::<ffi::FmNvlinkFailedDevices>() };
        failed.num_gpus = 1;
        failed.gpu_info[0].num_ports = 2;
        failed.gpu_info[0].port_num[0] = 3;
        failed.gpu_info[0].port_num[1] = 9;
        fill(&mut failed.gpu_info[0].uuid, "GPU-x");
        fill(&mut failed.gpu_info[0].pci_bus_id, "00000000:1A:00.0");
        let devices = NvlinkFailedDevices::from(&failed);
        assert_eq!(devices.gpu_info[0].port_num, [3, 9]);
        let text = devices.to_string();
        assert!(text.contains("Failed GPUs (1):"));
        assert!(text.contains("00000000:1A:00.0  3,9\n"));
        assert!(text.ends_with("Failed NVSwitches: none\n"));
        let json = serde_json::to_value(&devices).unwrap();
        assert_eq!(json["gpuInfo"][0]["pciBusId"], "00000000:1A:00.0");
    }
}
