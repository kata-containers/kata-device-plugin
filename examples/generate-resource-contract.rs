use kata_device_plugin::vfio::{Naming, RESOURCES};
use serde_json::json;
use serde_yaml::Value;
use std::error::Error;
use std::fs;
use std::path::Path;

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, Box<dyn Error>> {
    value[key]
        .as_str()
        .ok_or_else(|| format!("chart value {key} is missing or not a string").into())
}

fn main() -> Result<(), Box<dyn Error>> {
    let chart_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("deploy/helm/kata-device-plugin");
    let chart: Value = serde_yaml::from_str(&fs::read_to_string(chart_dir.join("Chart.yaml"))?)?;
    let values: Value = serde_yaml::from_str(&fs::read_to_string(chart_dir.join("values.yaml"))?)?;
    let version = string(&chart, "version")?;
    let app_version = string(&chart, "appVersion")?;
    if version != env!("CARGO_PKG_VERSION") || app_version != version {
        return Err("chart and Cargo package versions differ".into());
    }
    let default_naming = string(&values, "resourceNaming")?;
    Naming::parse(default_naming)?;
    let modes: Vec<_> = Naming::ALL.iter().map(|mode| mode.as_str()).collect();
    let image = &values["image"];
    let image_tag = string(image, "tag")?;
    let image_tag = if image_tag.is_empty() {
        format!("v{app_version}")
    } else {
        image_tag.to_owned()
    };
    let resources: Vec<_> = RESOURCES
        .iter()
        .map(|resource| {
            json!({
                "name": resource.name,
                "pciVendorId": format!("0x{:04x}", resource.vendor),
                "pciDeviceId": resource.device.map(|id| format!("0x{id:04x}")),
                "pciClassPrefix": format!("0x{:04x}", resource.class_prefix),
            })
        })
        .collect();
    let index = json!({
        "schemaVersion": 1,
        "chart": {
            "name": string(&chart, "name")?,
            "version": version,
            "appVersion": app_version,
        },
        "image": {
            "repository": string(image, "repository")?,
            "defaultTag": image_tag,
        },
        "resourceNaming": {
            "default": default_naming,
            "modes": modes,
            "aliasResources": resources,
        },
    });
    let schema = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "resourceNaming": {
                "type": "string",
                "enum": modes,
                "default": default_naming,
            }
        }
    });
    for (name, contents) in [("index.json", index), ("values.schema.json", schema)] {
        fs::write(
            chart_dir.join(name),
            format!("{}\n", serde_json::to_string_pretty(&contents)?),
        )?;
    }
    Ok(())
}
