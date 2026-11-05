// SPDX-License-Identifier: Apache-2.0
// Copyright (c) NVIDIA CORPORATION

use std::ffi::c_void;
use std::os::raw::{c_char, c_int, c_uint};

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

#[link(name = "nvfm")]
extern "C" {
    pub fn fmLibInit() -> FmReturn;
    pub fn fmLibShutdown() -> FmReturn;
    pub fn fmConnect(params: *mut FmConnectParams, handle: *mut FmHandle) -> FmReturn;
    pub fn fmDisconnect(handle: FmHandle) -> FmReturn;
    pub fn fmGetSupportedFabricPartitions(
        handle: FmHandle,
        list: *mut FmFabricPartitionList,
    ) -> FmReturn;
    pub fn fmActivateFabricPartition(handle: FmHandle, partition_id: c_uint) -> FmReturn;
    pub fn fmActivateFabricPartitionWithVFs(
        handle: FmHandle,
        partition_id: c_uint,
        vf_list: *mut FmPciDevice,
        num_vfs: c_uint,
    ) -> FmReturn;
    pub fn fmDeactivateFabricPartition(handle: FmHandle, partition_id: c_uint) -> FmReturn;
    pub fn fmSetActivatedFabricPartitions(
        handle: FmHandle,
        list: *mut FmActivatedFabricPartitionList,
    ) -> FmReturn;
    pub fn fmGetNvlinkFailedDevices(
        handle: FmHandle,
        devices: *mut FmNvlinkFailedDevices,
    ) -> FmReturn;
    pub fn fmGetUnsupportedFabricPartitions(
        handle: FmHandle,
        list: *mut FmUnsupportedFabricPartitionList,
    ) -> FmReturn;
}
