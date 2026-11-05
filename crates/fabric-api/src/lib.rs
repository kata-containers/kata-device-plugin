// Copyright (c) NVIDIA CORPORATION
// SPDX-License-Identifier: Apache-2.0

//! The fabric view kata-device-plugin serves: what the holder of a node's
//! NVSwitch fabric publishes about Fabric Manager's partitions, as wire
//! types generated from `proto/kata/fabric/v1/fabric.proto`, the Publish
//! call's two ends, and the token they agree on.
//!
//! In shared NVSwitch mode Fabric Manager in a Service VM activates GPU
//! partitions, and the plugin on the host advertises each active one as a
//! schedulable unit.  The plugin cannot read Fabric Manager, so the fabric
//! holder publishes the partition lists in one call, and the plugin keeps
//! the last view until the next call or until the fabric changes hands.
//! The wire types carry serde derives under the field names of NVIDIA's
//! `fmpm` sample client (`partitionId`, `isActive`, `physicalId`), so a
//! client's JSON output and the message it publishes are one schema.
//! Resolving physical IDs to the host's PCI functions is pcilibs-rs's
//! `fabric` module.

use std::fmt;

pub mod grpc;

pub mod v1 {
    //! Generated from `proto/kata/fabric/v1/fabric.proto`: the messages, and
    //! the Publish service's client and server.
    tonic::include_proto!("kata.fabric.v1");
}

/// Environment variable the plugin sets in the container it gives the
/// fabric to: the endpoint it publishes to, `http://<node ip>:<port>`.
/// Absent when the plugin does not know its node's address.
pub const ENDPOINT_ENV: &str = "KATA_FABRIC_VIEW_ENDPOINT";
/// Environment variable carrying the token; see [`ENDPOINT_ENV`].
pub const TOKEN_ENV: &str = "KATA_FABRIC_VIEW_TOKEN";
/// gRPC metadata key the token travels under on every call.
pub const TOKEN_METADATA_KEY: &str = "kata-fabric-view-token";
/// Where the plugin listens for a publisher on the host itself; filesystem
/// permissions are the credential there.
pub const HOST_SOCKET: &str = "/var/run/kata-device-plugin/fabric-view.sock";

/// How the fabric owner wants the GPUs of active partitions consumed; the
/// `unit` word of a [`v1::FabricView`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Unit {
    /// One schedulable unit per active partition, taken whole.  The default,
    /// because tenants that share an active partition can reach each other
    /// over NVLink.
    #[default]
    Partition,
    /// One unit per GPU, any count: bare-metal semantics, for an owner who
    /// holds the whole node.
    Gpu,
}

impl Unit {
    /// The word as it travels in the view.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Partition => "partition",
            Self::Gpu => "gpu",
        }
    }

    /// An unset word is the default; a word this crate does not know is
    /// `None`, so a view from a newer publisher is refused whole rather
    /// than interpreted partially.
    pub fn from_view(word: &str) -> Option<Self> {
        match word {
            "" | "partition" => Some(Self::Partition),
            "gpu" => Some(Self::Gpu),
            _ => None,
        }
    }
}

/// The one-line log form both ends of the call print, so a publisher's
/// and the plugin's logs line up: the unit word, then each active partition
/// as `id x gpus`.
impl fmt::Display for v1::FabricView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unit {:?}, active partitions (id x gpus):", self.unit)?;
        let mut active = self
            .partitions
            .iter()
            .flat_map(|list| list.partition_info.iter())
            .filter(|partition| partition.is_active)
            .peekable();
        if active.peek().is_none() {
            return f.write_str(" none");
        }
        for partition in active {
            write!(
                f,
                " {}x{}",
                partition.partition_id,
                partition.gpu_info.len()
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A DGX NVL8's table as Fabric Manager lists it: 1 is the node, 2 and
    /// 3 the halves, 4 to 7 the pairs, 8 to 15 the singles; 1 is active.
    const RUBIN: &str = include_str!("../testdata/dgx-rubin-nvl8-partitions.json");

    fn rubin_view(unit: &str) -> v1::FabricView {
        v1::FabricView {
            partitions: Some(serde_json::from_str(RUBIN).unwrap()),
            unsupported: None,
            unit: unit.to_owned(),
        }
    }

    fn rubin_view_active(unit: &str, active: &[u32]) -> v1::FabricView {
        let mut view = rubin_view(unit);
        for p in &mut view.partitions.as_mut().unwrap().partition_info {
            p.is_active = active.contains(&p.partition_id);
        }
        view
    }

    #[test]
    fn a_view_prints_as_one_log_line() {
        assert_eq!(
            rubin_view("gpu").to_string(),
            "unit \"gpu\", active partitions (id x gpus): 1x8"
        );
        assert_eq!(
            rubin_view_active("", &[2, 6, 14]).to_string(),
            "unit \"\", active partitions (id x gpus): 2x4 6x2 14x1"
        );
        assert_eq!(
            v1::FabricView::default().to_string(),
            "unit \"\", active partitions (id x gpus): none"
        );
    }

    #[test]
    fn unit_words() {
        assert_eq!(Unit::from_view(""), Some(Unit::Partition));
        assert_eq!(Unit::from_view("partition"), Some(Unit::Partition));
        assert_eq!(Unit::from_view("gpu"), Some(Unit::Gpu));
        assert_eq!(Unit::from_view("GPU"), None);
        assert_eq!(Unit::default(), Unit::Partition);
        for unit in [Unit::Partition, Unit::Gpu] {
            assert_eq!(Unit::from_view(unit.as_str()), Some(unit));
        }
    }

    #[test]
    fn json_keeps_fmpm_names() {
        let list: v1::PartitionList = serde_json::from_str(RUBIN).unwrap();
        assert_eq!(list.partition_info.len(), 15);
        assert_eq!(list.max_num_partitions, 15);
        let node = &list.partition_info[0];
        assert!(node.is_active);
        assert_eq!(
            node.gpu_info
                .iter()
                .map(|g| g.physical_id)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4, 5, 6, 7, 8]
        );
        assert_eq!(node.gpu_info[0].num_nvlinks_available, 36);
        assert_eq!(node.gpu_info[0].nvlink_line_rate_mbps, 41000);

        let json = serde_json::to_value(rubin_view("gpu")).unwrap();
        let gpu = &json["partitions"]["partitionInfo"][0]["gpuInfo"][0];
        for key in [
            "physicalId",
            "uuid",
            "pciBusId",
            "numNvLinksAvailable",
            "maxNumNvLinks",
            "nvlinkLineRateMBps",
        ] {
            assert!(gpu.get(key).is_some(), "{key} missing: {gpu}");
        }
        assert_eq!(json["partitions"]["partitionInfo"][0]["isActive"], true);
        assert_eq!(json["unit"], "gpu");
        assert!(json["unsupported"].is_null());
    }
}
