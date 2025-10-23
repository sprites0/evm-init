pub mod hl_header;
pub mod serde_bincode_compat;
pub mod types;

pub use hl_header::HlHeader;
pub use types::BlockAndReceipts;

use anyhow::{Context, Result};
use aws_sdk_s3::Client;

/// Generates the RMP file path for a given block height
/// Format: {f}/{s}/{height}.rmp.lz4
/// where f = ((height - 1) / 1_000_000) * 1_000_000
///       s = ((height - 1) / 1_000) * 1_000
pub fn rmp_path(height: u64) -> String {
    let f = ((height - 1) / 1_000_000) * 1_000_000;
    let s = ((height - 1) / 1_000) * 1_000;
    format!("{f}/{s}/{height}.rmp.lz4")
}

/// Download a block from S3 and decompress it
pub async fn download_block(client: &Client, bucket: &str, key: &str) -> Result<BlockAndReceipts> {
    let request = client
        .get_object()
        .bucket(bucket)
        .key(key)
        .request_payer(aws_sdk_s3::types::RequestPayer::Requester);

    let response = request
        .send()
        .await
        .context(format!("Failed to download object: {}", key))?;

    let bytes = response
        .body
        .collect()
        .await
        .context("Failed to collect object data")?
        .into_bytes();

    // Decompress LZ4 data using frame decoder (matches nanoreth pattern)
    let mut decoder = lz4_flex::frame::FrameDecoder::new(&bytes[..]);

    // S3 files contain Vec<BlockAndReceipts>, extract first element
    let blocks: Vec<BlockAndReceipts> =
        rmp_serde::from_read(&mut decoder).context("Failed to deserialize block data")?;

    blocks.into_iter().next().context("Block data is empty")
}
