pub mod cdi;
pub mod config;
pub mod labels;
pub mod platform;
pub mod plugin;

pub mod dp {
    pub mod v1beta1 {
        tonic::include_proto!("v1beta1");
    }
}
