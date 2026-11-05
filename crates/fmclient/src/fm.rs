// SPDX-License-Identifier: Apache-2.0
// Copyright (c) NVIDIA CORPORATION

use std::fmt;
use std::ptr;
use std::time::{Duration, Instant};

use kata_fabric_api::v1;

use crate::ffi;
use crate::types;

#[derive(Debug)]
pub enum FmError {
    /// libnvfm could not be loaded or lacks an entry point.
    Library(libloading::Error),
    /// A call returned a status other than success.
    Status(ffi::FmReturn),
}

impl FmError {
    fn from_return(ret: ffi::FmReturn) -> Result<(), Self> {
        match ret {
            ffi::FM_ST_SUCCESS => Ok(()),
            code => Err(Self::Status(code)),
        }
    }
}

impl fmt::Display for FmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let code = match self {
            Self::Library(e) => return write!(f, "Fabric Manager SDK library: {e}"),
            Self::Status(code) => *code,
        };
        let msg = match code {
            ffi::FM_ST_BADPARAM => "a supplied argument is invalid",
            ffi::FM_ST_GENERIC_ERROR => "a generic, unspecified error",
            ffi::FM_ST_NOT_SUPPORTED => "the requested operation/feature not supported",
            ffi::FM_ST_UNINITIALIZED => "object is in undefined state/uninitialized",
            ffi::FM_ST_TIMEOUT => "requested operation timed out",
            ffi::FM_ST_VERSION_MISMATCH => "version mismatch between received and understood API",
            ffi::FM_ST_IN_USE => "resource is in use",
            ffi::FM_ST_NOT_CONFIGURED => "setting not configured",
            ffi::FM_ST_CONNECTION_NOT_VALID => "connection to the FM instance is not valid",
            ffi::FM_ST_NVLINK_ERROR => "NVLink error",
            ffi::FM_ST_RESOURCE_BAD => "referenced resource does not exist",
            ffi::FM_ST_RESOURCE_IN_USE => "referenced resource is already in use",
            ffi::FM_ST_RESOURCE_NOT_IN_USE => "referenced resource is not in use",
            ffi::FM_ST_RESOURCE_EXHAUSTED => "resources exhausted",
            ffi::FM_ST_RESOURCE_NOT_READY => "resource is not ready",
            ffi::FM_ST_PARTITION_EXISTS => "partition already exists",
            ffi::FM_ST_PARTITION_ID_IN_USE => "partition id already in use",
            ffi::FM_ST_PARTITION_ID_NOT_IN_USE => "partition id not found",
            ffi::FM_ST_PARTITION_NAME_IN_USE => "partition name already in use",
            ffi::FM_ST_PARTITION_NAME_NOT_IN_USE => "partition name not found",
            ffi::FM_ST_PARTITION_ID_NAME_MISMATCH => {
                "partition id and name refer to different partitions"
            }
            ffi::FM_ST_NOT_READY => "fabric manager is not ready",
            ffi::FM_ST_RESOURCE_USED_IN_THIS_PARTITION => "resource already used in this partition",
            ffi::FM_ST_RESOURCE_USED_IN_ANOTHER_PARTITION => {
                "resource already used in another partition"
            }
            ffi::FM_ST_PARTITION_MISWIRED_TRUNKS => "partition has miswired trunks",
            ffi::FM_ST_PARTITION_INSUFFICIENT_TRUNKS => "partition has insufficient trunks",
            ffi::FM_ST_PARTITION_MISSING_SWITCHES => "partition has missing switches",
            ffi::FM_ST_PARTITION_NETWORK_CONFIG_ERROR => {
                "partition has network configuration error"
            }
            ffi::FM_ST_PARTITION_ROUTE_PROGRAMMING_ERROR => "partition has route programming error",
            code => return write!(f, "unknown FM error ({})", code),
        };
        write!(f, "{msg} ({code})")
    }
}

impl std::error::Error for FmError {}

/// The SDK, loaded and initialized; dropping it shuts the SDK down.
pub struct FmLib {
    api: ffi::Api,
}

impl FmLib {
    pub fn init() -> Result<Self, FmError> {
        let api = ffi::Api::load().map_err(FmError::Library)?;
        FmError::from_return(unsafe { (api.fm_lib_init)() })?;
        Ok(Self { api })
    }
}

impl Drop for FmLib {
    fn drop(&mut self) {
        unsafe { (self.api.fm_lib_shutdown)() };
    }
}

pub struct FmConnection<'l> {
    api: &'l ffi::Api,
    handle: ffi::FmHandle,
}

pub enum FmAddress<'a> {
    Tcp(&'a str),
    Unix(&'a str),
    Vsock(&'a str),
}

impl<'l> FmConnection<'l> {
    /// Borrows the SDK, so a connection never outlives its initialization.
    /// Retries on FM_ST_CONNECTION_NOT_VALID until `retry_timeout` elapses.
    pub fn connect(
        lib: &'l FmLib,
        address: FmAddress<'_>,
        connect_timeout_ms: u32,
        retry_timeout: Duration,
    ) -> Result<Self, FmError> {
        let api = &lib.api;
        let mut params = unsafe { std::mem::zeroed::<ffi::FmConnectParams>() };
        params.version = ffi::FmConnectParams::version();
        params.timeout_ms = connect_timeout_ms;

        let (addr_str, addr_type, is_unix) = match address {
            FmAddress::Tcp(s) => (s, ffi::NvFmApiAddrType::Inet, 0),
            FmAddress::Unix(s) => (s, ffi::NvFmApiAddrType::Unix, 1),
            FmAddress::Vsock(s) => (s, ffi::NvFmApiAddrType::Vsock, 0),
        };

        let bytes = addr_str.as_bytes();
        let len = bytes.len().min(ffi::FM_MAX_STR_LENGTH - 1);
        for (dst, &src) in params.address_info[..len].iter_mut().zip(bytes) {
            *dst = src as i8;
        }
        params.address_type = addr_type;
        params.address_is_unix_socket = is_unix;

        let deadline = Instant::now() + retry_timeout;
        let mut retried = false;

        loop {
            let mut handle: ffi::FmHandle = ptr::null_mut();
            let ret = unsafe { (api.fm_connect)(&mut params, &mut handle) };

            if ret == ffi::FM_ST_SUCCESS {
                return Ok(Self { api, handle });
            }

            if ret != ffi::FM_ST_CONNECTION_NOT_VALID || Instant::now() >= deadline {
                return Err(FmError::Status(ret));
            }

            if !retried {
                eprintln!("waiting for Fabric Manager...");
                retried = true;
            }

            std::thread::sleep(Duration::from_secs(5));
        }
    }

    pub fn list_partitions(&self) -> Result<v1::PartitionList, FmError> {
        let mut list = unsafe { std::mem::zeroed::<ffi::FmFabricPartitionList>() };
        list.version = ffi::FmFabricPartitionList::version();
        FmError::from_return(unsafe {
            (self.api.fm_get_supported_fabric_partitions)(self.handle, &mut list)
        })?;
        Ok(types::partition_list(&list))
    }

    pub fn list_unsupported_partitions(&self) -> Result<v1::UnsupportedPartitionList, FmError> {
        let mut list = unsafe { std::mem::zeroed::<ffi::FmUnsupportedFabricPartitionList>() };
        list.version = ffi::FmUnsupportedFabricPartitionList::version();
        FmError::from_return(unsafe {
            (self.api.fm_get_unsupported_fabric_partitions)(self.handle, &mut list)
        })?;
        Ok(types::unsupported_partition_list(&list))
    }

    /// The view the plugin is given: both partition lists and the unit word.
    pub fn fabric_view(&self, unit: &str) -> Result<v1::FabricView, FmError> {
        Ok(v1::FabricView {
            partitions: Some(self.list_partitions()?),
            unsupported: Some(self.list_unsupported_partitions()?),
            unit: unit.to_owned(),
        })
    }

    pub fn activate_partition(&self, partition_id: u32) -> Result<(), FmError> {
        FmError::from_return(unsafe {
            (self.api.fm_activate_fabric_partition)(self.handle, partition_id)
        })
    }

    pub fn activate_partition_with_vfs(
        &self,
        partition_id: u32,
        vfs: &[ffi::FmPciDevice],
    ) -> Result<(), FmError> {
        let mut vf_list: Vec<ffi::FmPciDevice> = vfs.to_vec();
        FmError::from_return(unsafe {
            (self.api.fm_activate_fabric_partition_with_vfs)(
                self.handle,
                partition_id,
                vf_list.as_mut_ptr(),
                vf_list.len() as u32,
            )
        })
    }

    pub fn deactivate_partition(&self, partition_id: u32) -> Result<(), FmError> {
        FmError::from_return(unsafe {
            (self.api.fm_deactivate_fabric_partition)(self.handle, partition_id)
        })
    }

    pub fn set_activated_partitions(&self, ids: &[u32]) -> Result<(), FmError> {
        let mut list = unsafe { std::mem::zeroed::<ffi::FmActivatedFabricPartitionList>() };
        list.version = ffi::FmActivatedFabricPartitionList::version();
        list.num_partitions = ids.len() as u32;
        list.partition_ids[..ids.len()].copy_from_slice(ids);
        FmError::from_return(unsafe {
            (self.api.fm_set_activated_fabric_partitions)(self.handle, &mut list)
        })
    }

    pub fn get_nvlink_failed_devices(&self) -> Result<types::NvlinkFailedDevices, FmError> {
        let mut devices = unsafe { std::mem::zeroed::<ffi::FmNvlinkFailedDevices>() };
        devices.version = ffi::FmNvlinkFailedDevices::version();
        FmError::from_return(unsafe {
            (self.api.fm_get_nvlink_failed_devices)(self.handle, &mut devices)
        })?;
        Ok(types::NvlinkFailedDevices::from(&devices))
    }
}

impl Drop for FmConnection<'_> {
    fn drop(&mut self) {
        unsafe { (self.api.fm_disconnect)(self.handle) };
    }
}
