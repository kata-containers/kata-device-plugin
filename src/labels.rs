// Written by GPU Feature Discovery (k8s-device-plugin >= v0.17.0).
// Format: <ClusterUUID>.<CliqueID>  e.g. "7b968a6d-c8aa-45e1-9e70-e1e51be99c31.1"
// Presence means the node is fabric-attached and GFD confirmed GPU_FABRIC_STATE_COMPLETED.
// Absence means plain passthrough with no multi-node NVLink (pgpu mode).
pub const LABEL_GPU_CLIQUE: &str = "nvidia.com/gpu.clique";
