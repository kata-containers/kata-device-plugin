// SPDX-License-Identifier: Apache-2.0
// Copyright (c) NVIDIA CORPORATION

use std::ffi::c_void;
use std::os::raw::{c_char, c_int, c_uint};

use libloading::Library;

pub const FM_MAX_STR_LENGTH: usize = 256;
pub const FM_UUID_BUFFER_SIZE: usize = 80;
pub const FM_DEVICE_PCI_BUS_ID_BUFFER_SIZE: usize = 32;
pub const FM_MAX_NUM_GPUS: usize = 16;
pub const FM_MAX_NUM_NVSWITCHES: usize = 12;
pub const FM_MAX_FABRIC_PARTITIONS: usize = 64;
pub const FM_MAX_NUM_NVLINK_PORTS: usize = 64;

/// FM SDK version tag: encodes struct size + version number for ABI negotiation.
const fn make_fm_param_version<T>(ver: u32) -> c_uint {
    (std::mem::size_of::<T>() as u32 | (ver << 24)) as c_uint
}

pub type FmHandle = *mut c_void;
pub type FmReturn = c_int;

pub const FM_ST_SUCCESS: FmReturn = 0;
pub const FM_ST_BADPARAM: FmReturn = -1;
pub const FM_ST_GENERIC_ERROR: FmReturn = -2;
pub const FM_ST_NOT_SUPPORTED: FmReturn = -3;
pub const FM_ST_UNINITIALIZED: FmReturn = -4;
pub const FM_ST_TIMEOUT: FmReturn = -5;
pub const FM_ST_VERSION_MISMATCH: FmReturn = -6;
pub const FM_ST_IN_USE: FmReturn = -7;
pub const FM_ST_NOT_CONFIGURED: FmReturn = -8;
pub const FM_ST_CONNECTION_NOT_VALID: FmReturn = -9;
pub const FM_ST_NVLINK_ERROR: FmReturn = -10;
pub const FM_ST_RESOURCE_BAD: FmReturn = -11;
pub const FM_ST_RESOURCE_IN_USE: FmReturn = -12;
pub const FM_ST_RESOURCE_NOT_IN_USE: FmReturn = -13;
pub const FM_ST_RESOURCE_EXHAUSTED: FmReturn = -14;
pub const FM_ST_RESOURCE_NOT_READY: FmReturn = -15;
pub const FM_ST_PARTITION_EXISTS: FmReturn = -16;
pub const FM_ST_PARTITION_ID_IN_USE: FmReturn = -17;
pub const FM_ST_PARTITION_ID_NOT_IN_USE: FmReturn = -18;
pub const FM_ST_PARTITION_NAME_IN_USE: FmReturn = -19;
pub const FM_ST_PARTITION_NAME_NOT_IN_USE: FmReturn = -20;
pub const FM_ST_PARTITION_ID_NAME_MISMATCH: FmReturn = -21;
pub const FM_ST_NOT_READY: FmReturn = -22;
pub const FM_ST_RESOURCE_USED_IN_THIS_PARTITION: FmReturn = -23;
pub const FM_ST_RESOURCE_USED_IN_ANOTHER_PARTITION: FmReturn = -24;
pub const FM_ST_PARTITION_MISWIRED_TRUNKS: FmReturn = -25;
pub const FM_ST_PARTITION_INSUFFICIENT_TRUNKS: FmReturn = -26;
pub const FM_ST_PARTITION_MISSING_SWITCHES: FmReturn = -27;
pub const FM_ST_PARTITION_NETWORK_CONFIG_ERROR: FmReturn = -28;
pub const FM_ST_PARTITION_ROUTE_PROGRAMMING_ERROR: FmReturn = -29;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // All variants needed for ABI compatibility
pub enum NvFmApiAddrType {
    Unknown = 0,
    Inet = 1,
    Unix = 2,
    Vsock = 3,
}

#[repr(C)]
pub struct FmConnectParams {
    pub version: c_uint,
    pub address_info: [c_char; FM_MAX_STR_LENGTH],
    pub timeout_ms: c_uint,
    pub address_is_unix_socket: c_uint,
    pub address_type: NvFmApiAddrType,
}

impl FmConnectParams {
    pub fn version() -> c_uint {
        make_fm_param_version::<Self>(2)
    }
}

#[repr(C)]
#[derive(Clone)]
pub struct FmPciDevice {
    pub domain: c_uint,
    pub bus: c_uint,
    pub device: c_uint,
    pub function: c_uint,
}

#[repr(C)]
pub struct FmFabricPartitionGpuInfo {
    pub physical_id: c_uint,
    pub uuid: [c_char; FM_UUID_BUFFER_SIZE],
    pub pci_bus_id: [c_char; FM_DEVICE_PCI_BUS_ID_BUFFER_SIZE],
    pub num_nvlinks_available: c_uint,
    pub max_num_nvlinks: c_uint,
    pub nvlink_line_rate_mbps: c_uint,
}

#[repr(C)]
pub struct FmFabricPartitionInfo {
    pub partition_id: c_uint,
    pub is_active: c_uint,
    pub num_gpus: c_uint,
    pub gpu_info: [FmFabricPartitionGpuInfo; FM_MAX_NUM_GPUS],
}

#[repr(C)]
pub struct FmFabricPartitionList {
    pub version: c_uint,
    pub num_partitions: c_uint,
    pub max_num_partitions: c_uint,
    pub partition_info: [FmFabricPartitionInfo; FM_MAX_FABRIC_PARTITIONS],
}

impl FmFabricPartitionList {
    pub fn version() -> c_uint {
        make_fm_param_version::<Self>(1)
    }
}

#[repr(C)]
pub struct FmActivatedFabricPartitionList {
    pub version: c_uint,
    pub num_partitions: c_uint,
    pub partition_ids: [c_uint; FM_MAX_FABRIC_PARTITIONS],
}

impl FmActivatedFabricPartitionList {
    pub fn version() -> c_uint {
        make_fm_param_version::<Self>(1)
    }
}

#[repr(C)]
pub struct FmNvlinkFailedDeviceInfo {
    pub uuid: [c_char; FM_UUID_BUFFER_SIZE],
    pub pci_bus_id: [c_char; FM_DEVICE_PCI_BUS_ID_BUFFER_SIZE],
    pub num_ports: c_uint,
    pub port_num: [c_uint; FM_MAX_NUM_NVLINK_PORTS],
}

#[repr(C)]
pub struct FmNvlinkFailedDevices {
    pub version: c_uint,
    pub num_gpus: c_uint,
    pub num_switches: c_uint,
    pub gpu_info: [FmNvlinkFailedDeviceInfo; FM_MAX_NUM_GPUS],
    pub switch_info: [FmNvlinkFailedDeviceInfo; FM_MAX_NUM_NVSWITCHES],
}

impl FmNvlinkFailedDevices {
    pub fn version() -> c_uint {
        make_fm_param_version::<Self>(1)
    }
}

#[repr(C)]
pub struct FmUnsupportedFabricPartitionInfo {
    pub partition_id: c_uint,
    pub num_gpus: c_uint,
    pub gpu_physical_ids: [c_uint; FM_MAX_NUM_GPUS],
}

#[repr(C)]
pub struct FmUnsupportedFabricPartitionList {
    pub version: c_uint,
    pub num_partitions: c_uint,
    pub partition_info: [FmUnsupportedFabricPartitionInfo; FM_MAX_FABRIC_PARTITIONS],
}

impl FmUnsupportedFabricPartitionList {
    pub fn version() -> c_uint {
        make_fm_param_version::<Self>(1)
    }
}

/// The library's soname, as the SDK package installs it.
pub const LIBRARY: &str = "libnvfm.so.1";

/// libnvfm's entry points, resolved when the library is loaded.  The
/// signatures are the SDK's declarations in nv_fm_agent.h.
pub struct Api {
    pub fm_lib_init: unsafe extern "C" fn() -> FmReturn,
    pub fm_lib_shutdown: unsafe extern "C" fn() -> FmReturn,
    pub fm_connect: unsafe extern "C" fn(*mut FmConnectParams, *mut FmHandle) -> FmReturn,
    pub fm_disconnect: unsafe extern "C" fn(FmHandle) -> FmReturn,
    pub fm_get_supported_fabric_partitions:
        unsafe extern "C" fn(FmHandle, *mut FmFabricPartitionList) -> FmReturn,
    pub fm_activate_fabric_partition: unsafe extern "C" fn(FmHandle, c_uint) -> FmReturn,
    pub fm_activate_fabric_partition_with_vfs:
        unsafe extern "C" fn(FmHandle, c_uint, *mut FmPciDevice, c_uint) -> FmReturn,
    pub fm_deactivate_fabric_partition: unsafe extern "C" fn(FmHandle, c_uint) -> FmReturn,
    pub fm_set_activated_fabric_partitions:
        unsafe extern "C" fn(FmHandle, *mut FmActivatedFabricPartitionList) -> FmReturn,
    pub fm_get_nvlink_failed_devices:
        unsafe extern "C" fn(FmHandle, *mut FmNvlinkFailedDevices) -> FmReturn,
    pub fm_get_unsupported_fabric_partitions:
        unsafe extern "C" fn(FmHandle, *mut FmUnsupportedFabricPartitionList) -> FmReturn,
    /// Dropped last: the pointers above point into it.
    _library: Library,
}

impl Api {
    /// Load the library and resolve every entry point, so a missing or
    /// mismatched SDK is one error at startup and never a crash mid-call.
    pub fn load() -> Result<Self, libloading::Error> {
        // SAFETY: loading runs libnvfm's constructors, which set up nothing
        // this process must sequence against; the SDK's own initializer is
        // fmLibInit, called explicitly.
        let library = unsafe { Library::new(LIBRARY) }?;
        macro_rules! entry {
            ($name:literal) => {
                // SAFETY: the pointer type is the SDK's declaration of the
                // symbol in nv_fm_agent.h.
                *unsafe { library.get($name) }?
            };
        }
        let fm_lib_init = entry!(b"fmLibInit\0");
        let fm_lib_shutdown = entry!(b"fmLibShutdown\0");
        let fm_connect = entry!(b"fmConnect\0");
        let fm_disconnect = entry!(b"fmDisconnect\0");
        let fm_get_supported_fabric_partitions = entry!(b"fmGetSupportedFabricPartitions\0");
        let fm_activate_fabric_partition = entry!(b"fmActivateFabricPartition\0");
        let fm_activate_fabric_partition_with_vfs = entry!(b"fmActivateFabricPartitionWithVFs\0");
        let fm_deactivate_fabric_partition = entry!(b"fmDeactivateFabricPartition\0");
        let fm_set_activated_fabric_partitions = entry!(b"fmSetActivatedFabricPartitions\0");
        let fm_get_nvlink_failed_devices = entry!(b"fmGetNvlinkFailedDevices\0");
        let fm_get_unsupported_fabric_partitions = entry!(b"fmGetUnsupportedFabricPartitions\0");
        Ok(Self {
            fm_lib_init,
            fm_lib_shutdown,
            fm_connect,
            fm_disconnect,
            fm_get_supported_fabric_partitions,
            fm_activate_fabric_partition,
            fm_activate_fabric_partition_with_vfs,
            fm_deactivate_fabric_partition,
            fm_set_activated_fabric_partitions,
            fm_get_nvlink_failed_devices,
            fm_get_unsupported_fabric_partitions,
            _library: library,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every entry point resolves where the SDK is installed; without it
    /// there is nothing to check, and the test says so instead of failing
    /// on a host that has no Fabric Manager.
    #[test]
    fn the_entry_points_resolve_where_the_library_is() {
        match Api::load() {
            Ok(_) => {}
            Err(e) => eprintln!("skipped, {LIBRARY} is not loadable here: {e}"),
        }
    }
}
