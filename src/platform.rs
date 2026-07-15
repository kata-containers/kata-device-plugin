use tracing::info;

const DMI_PRODUCT_NAME: &str = "/sys/class/dmi/id/product_name";

#[derive(Debug, Clone, PartialEq)]
pub enum Platform {
    HgxH100,
    HgxB200,
    HgxB300,
    Gb200Nvl72,
    Gb300,
    VeraRubin,
    Unknown(String),
}

impl Platform {
    /// Detect the platform from the DMI product name sysfs node.
    /// No subprocess, no root required.
    pub fn detect() -> Self {
        let raw = std::fs::read_to_string(DMI_PRODUCT_NAME).unwrap_or_default();
        let p = Self::from_product_name(raw.trim());
        info!(platform = p.name(), "detected platform");
        p
    }

    fn from_product_name(s: &str) -> Self {
        if s.contains("HGX H100") {
            Platform::HgxH100
        } else if s.contains("HGX B200") {
            Platform::HgxB200
        } else if s.contains("HGX B300") {
            Platform::HgxB300
        } else if s.contains("GB200 NVL72") || s.contains("GB200NVL72") {
            Platform::Gb200Nvl72
        } else if s.contains("GB300") {
            Platform::Gb300
        } else if s.contains("Vera Rubin") || s.contains("VR200") {
            Platform::VeraRubin
        } else {
            Platform::Unknown(s.to_owned())
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Platform::HgxH100    => "HGX H100",
            Platform::HgxB200    => "HGX B200",
            Platform::HgxB300    => "HGX B300",
            Platform::Gb200Nvl72 => "GB200 NVL72",
            Platform::Gb300      => "GB300",
            Platform::VeraRubin  => "Vera Rubin",
            Platform::Unknown(s) => s.as_str(),
        }
    }

    /// Expected number of NVLink partitions on this platform.
    /// TODO: derive dynamically from NVLink sysfs topology instead of
    ///       returning the static known value per SKU.
    pub fn nvlink_partitions(&self) -> Option<u32> {
        match self {
            Platform::HgxH100    => Some(1),
            Platform::HgxB200    => Some(1),
            Platform::HgxB300    => Some(1),
            Platform::Gb200Nvl72 => Some(1),
            Platform::Gb300      => Some(1),
            Platform::VeraRubin  => Some(1),
            Platform::Unknown(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_product_names() {
        let cases = [
            ("NVIDIA HGX H100 80GB SXM5", Platform::HgxH100),
            ("NVIDIA HGX B200", Platform::HgxB200),
            ("NVIDIA HGX B300", Platform::HgxB300),
            ("NVIDIA GB200 NVL72", Platform::Gb200Nvl72),
            ("NVIDIA GB200NVL72", Platform::Gb200Nvl72),
            ("NVIDIA GB300", Platform::Gb300),
            ("NVIDIA Vera Rubin", Platform::VeraRubin),
        ];
        for (name, expected) in cases {
            assert_eq!(Platform::from_product_name(name), expected, "input: {name}");
        }
    }

    #[test]
    fn unknown_falls_through() {
        assert_eq!(
            Platform::from_product_name("Dell PowerEdge R750"),
            Platform::Unknown("Dell PowerEdge R750".to_owned()),
        );
    }
}
