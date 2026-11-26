// The wire types carry serde derives under fmpm's field names, so the JSON a
// client prints and the message it publishes are one schema and the HGX
// guide's shell snippets keep working on either.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .type_attribute(
            ".kata.fabric.v1",
            "#[derive(serde::Serialize, serde::Deserialize)]",
        )
        .type_attribute(".kata.fabric.v1", "#[serde(rename_all = \"camelCase\")]")
        .field_attribute(
            ".kata.fabric.v1.PartitionGpu.num_nvlinks_available",
            "#[serde(rename = \"numNvLinksAvailable\")]",
        )
        .field_attribute(
            ".kata.fabric.v1.PartitionGpu.max_num_nvlinks",
            "#[serde(rename = \"maxNumNvLinks\")]",
        )
        .field_attribute(
            ".kata.fabric.v1.PartitionGpu.nvlink_line_rate_mbps",
            "#[serde(rename = \"nvlinkLineRateMBps\")]",
        )
        .compile_protos(&["proto/kata/fabric/v1/fabric.proto"], &["proto"])?;
    Ok(())
}
