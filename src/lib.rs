#![forbid(unsafe_code)]

pub mod cdi;
pub mod plugin;
pub mod vfio;
pub mod view;

pub mod dp {
    pub mod v1beta1 {
        tonic::include_proto!("v1beta1");
    }
}
