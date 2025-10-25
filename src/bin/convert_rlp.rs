use alloy_consensus::Header;
use alloy_rlp::{Decodable, Encodable};
use anyhow::Context;
use aws_sdk_s3::Client;
use clap::Parser;
use evm_init::{download_block, rmp_path, HlHeader};
use std::fs::File;
use std::io::{Read, Write};
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "Convert existing .rlp files from ethereum Header to HlHeader format")]
struct Args {
    /// Path to the .rlp file to convert
    input: PathBuf,

    /// Optional output path (defaults to overwriting input file)
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// S3 bucket name for downloading block receipts
    #[arg(short, long, default_value = "hl-testnet-evm-blocks")]
    bucket: String,

    /// AWS region
    #[arg(short, long, default_value = "ap-northeast-1")]
    region: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    println!("Reading .rlp file: {}", args.input.display());

    // Read the old .rlp file
    let mut file = File::open(&args.input)
        .context(format!("Failed to open file: {}", args.input.display()))?;
    let mut rlp_data = Vec::new();
    file.read_to_end(&mut rlp_data)
        .context("Failed to read .rlp file")?;

    // Decode as ethereum Header
    let header = Header::decode(&mut &rlp_data[..])
        .context("Failed to decode .rlp file as ethereum Header")?;

    let block_number = header.number;
    println!("Found header for block {}", block_number);

    // Download block receipts from S3
    println!("Downloading block {} from S3 to get receipts...", block_number);
    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(args.region.clone()))
        .load()
        .await;
    let client = Client::new(&config);
    let key = rmp_path(block_number);
    let block_and_receipts = download_block(&client, &args.bucket, &key)
        .await
        .context(format!("Failed to download block {}", block_number))?;

    // Convert LegacyReceipt to reth receipts for processing
    let receipts: Vec<_> = block_and_receipts
        .receipts
        .into_iter()
        .map(|r| r.into())
        .collect();

    let system_tx_count = block_and_receipts.system_txs.len() as u64;
    println!("Found {} system transactions", system_tx_count);

    // Create HlHeader with actual receipts from S3
    let hl_header = HlHeader::from_ethereum_header(header, &receipts, system_tx_count);

    // Encode the new HlHeader
    let mut buf = Vec::new();
    hl_header.encode(&mut buf);

    // Write to output file
    let output_path = args.output.as_ref().unwrap_or(&args.input);
    let mut output_file = File::create(output_path)
        .context(format!("Failed to create output file: {}", output_path.display()))?;
    output_file.write_all(&buf)
        .context("Failed to write HlHeader to file")?;

    println!("Successfully converted to HlHeader format: {}", output_path.display());
    println!("Block hash: 0x{:x}", hl_header.hash_slow());

    Ok(())
}
