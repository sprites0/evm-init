// Using rmp(rust-messagepack), read ~/abci_state.rmp.

use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_genesis::GenesisAccount;
use alloy_primitives::hex::ToHexExt;
use alloy_primitives::{Address, B256, Bytes};
use alloy_rlp::Encodable;
use anyhow::Context;
use aws_sdk_s3::Client;
use clap::Parser;
use evm_init::types::{AbciState, DbAccount, DbAccountInfo, EvmDb};
use evm_init::{download_block, rmp_path, HlHeader};
use reth_primitives::Bytecode;
use rocksdb::{Options, DB};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::{fs::File, io::Write};

#[derive(Serialize, Deserialize)]
enum BytecodeSerialized {
    LegacyRaw(Bytes),
    LegacyAnalyzed(LegacyAnalyzedBytecode),
}

/// Legacy analyzed
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LegacyAnalyzedBytecode {
    /// Bytecode with 32 zero bytes padding.
    bytecode: Bytes,
    /// Original bytes length.
    original_len: usize,
    // Jump table.
    // jump_table: JumpTable,
}

impl BytecodeSerialized {
    fn original_bytes(&self) -> Bytes {
        match self {
            Self::LegacyRaw(bytes) => bytes.clone(),
            Self::LegacyAnalyzed(lab) => lab
                .bytecode
                .slice(..lab.original_len),
        }
    }
}

#[derive(Parser)]
struct Args {
    /// Path to the abci state
    file: String,

    /// S3 bucket name for downloading block receipts
    #[arg(short, long, default_value = "hl-testnet-evm-blocks")]
    bucket: String,

    /// AWS region
    #[arg(short, long, default_value = "ap-northeast-1")]
    region: String,
}

/// Type to deserialize state root from state dump file.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct StateRoot {
    root: B256,
}

/// An account as in the state dump file. This contains a [`GenesisAccount`] and the account's
/// address.
#[derive(Debug, Serialize, Deserialize)]
struct GenesisAccountWithAddress {
    /// The account's balance, nonce, code, and storage.
    #[serde(flatten)]
    genesis_account: GenesisAccount,
    /// The account's address.
    address: Address,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let abci_state_path: PathBuf = args.file.into();
    let file = File::open(&abci_state_path)?;
    let mut reader = std::io::BufReader::new(file);

    let abci_state: AbciState = rmp_serde::decode::from_read(&mut reader)?;
    let evm = abci_state.exchange.hyper_evm;
    let header = evm.latest_block2.header();
    let block_number = header.number;

    // Download block from S3 to get receipts
    println!(
        "Downloading block {} from S3 to get receipts...",
        block_number
    );
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

    // Create HlHeader with actual receipts from S3
    let header = HlHeader::from_ethereum_header(header, &receipts, system_tx_count);

    let jsonl_output = format!("{}.jsonl", header.number);
    {
        match evm.state2.evm_db {
            EvmDb::InMemory {
                accounts,
                contracts,
            } => handle_in_memory_db(&jsonl_output, accounts, contracts),
            EvmDb::NoEvmDb {} => {
                println!("abci_state uses file-backed db, processing...");
                let file_db_path = abci_state_path
                    .parent()
                    .unwrap()
                    .join("evm_db_hub_slow")
                    .join("checkpoint")
                    .join(abci_state.exchange.context.height.to_string())
                    .join("EvmState");
                handle_file_backed_db(&jsonl_output, file_db_path)
            }
        }?
    }
    let rlp_output = format!("{}.rlp", header.number);
    {
        let mut buf = vec![];
        header.encode(&mut buf);
        let mut file = File::create(&rlp_output)?;
        file.write_all(&buf)?;
    }

    println!("Generated {} and {}", jsonl_output, rlp_output);
    println!("Now run:");
    println!(
        "reth-hl init-state --without-evm --chain testnet --header {rlp_output} --header-hash 0x{:x} {jsonl_output} --total-difficulty 0",
        header.hash_slow(),
    );

    Ok(())
}

fn handle_in_memory_db(
    jsonl_output: &String,
    accounts: Vec<(Address, DbAccount)>,
    contracts: Vec<(B256, Bytecode)>,
) -> Result<(), anyhow::Error> {
    let contracts = contracts
        .into_iter()
        .collect::<std::collections::HashMap<_, _>>();
    let file = File::create(jsonl_output)?;
    let mut file = std::io::BufWriter::new(file);
    writeln!(
        file,
        "{}",
        serde_json::to_string(&StateRoot { root: B256::ZERO })?
    )?;
    Ok(for (address, account) in accounts {
        // if account.info.balance.is_zero()
        //     && account.info.nonce.is_zero()
        //     && account.info.code_hash.is_zero()
        //     && account.storage.is_empty()
        // {
        //     continue;
        // }
        let is_eoa = account.info.code_hash == KECCAK_EMPTY;
        let storage = if is_eoa {
            None
        } else {
            Some(
                account
                    .storage
                    .into_iter()
                    .map(|(k, v)| (k.into(), v.into()))
                    .collect(),
            )
        };
        let account = to_genesis_account(account.info, &contracts, storage);
        let account = GenesisAccountWithAddress {
            genesis_account: account,
            address,
        };
        let account_json = serde_json::to_string(&account)?;
        writeln!(file, "{}", account_json)?;
    })
}

fn to_genesis_account(
    account: DbAccountInfo,
    contracts: &HashMap<B256, Bytecode>,
    storage: Option<BTreeMap<B256, B256>>,
) -> GenesisAccount {
    let is_eoa = account.code_hash == KECCAK_EMPTY;
    let code = if is_eoa {
        None
    } else {
        Some(contracts[&account.code_hash].original_bytes())
    };
    GenesisAccount {
        balance: account.balance,
        nonce: Some(account.nonce),
        code: code.map(|x| x.into()),
        storage,
        ..Default::default()
    }
}

fn handle_file_backed_db(jsonl_output: &String, db_path: impl Into<PathBuf>) -> anyhow::Result<()> {
    let file = File::create(jsonl_output)?;
    let mut file = std::io::BufWriter::new(file);
    writeln!(
        file,
        "{}",
        serde_json::to_string(&StateRoot { root: B256::ZERO })?
    )?;
    // 0x4561: account
    // 0x4563: contract
    // 0x4573: storage

    fn flush_account(
        mut file: impl Write,
        address: Address,
        account: DbAccountInfo,
        storage: BTreeMap<B256, B256>,
        contracts: &HashMap<B256, Bytecode>,
    ) -> anyhow::Result<()> {
        let is_eoa = account.code_hash == KECCAK_EMPTY;
        let storage = if is_eoa { None } else { Some(storage) };
        let account = to_genesis_account(account, contracts, storage);

        let account = GenesisAccountWithAddress {
            genesis_account: account,
            address,
        };
        let account_json = serde_json::to_string(&account)?;
        writeln!(file, "{}", account_json)?;
        Ok(())
    }

    let prefix_extractor = rocksdb::SliceTransform::create_fixed_prefix(2);
    let mut opts = Options::default();
    opts.create_if_missing(true);
    opts.set_prefix_extractor(prefix_extractor);

    let db = DB::open(&opts, db_path.into()).unwrap();

    // iterate all contracts; for convenience just load all of them into memory
    let mut contracts = HashMap::new();
    for entry in db.prefix_iterator(b"\x45\x63") {
        // Process each contract
        let entry = entry?;
        let (key, value) = entry;

        let code_hash = B256::from_slice(&key[2..34]);
        contracts.insert(
            code_hash,
            Bytecode::new_raw(
                rmp_serde::from_slice::<BytecodeSerialized>(&value)
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "Failed to deserialize contract: {} {}",
                            key.encode_hex(),
                            e
                        )
                    })?
                    .original_bytes(),
            ),
        );
    }

    // iterate all accounts that has storage
    let mut storage_prefix: [u8; 2 + 20 + 32] = [0u8; 54];
    storage_prefix[0..2].copy_from_slice(&[0x45, 0x73]);
    let mut storage_iterator = db.prefix_iterator(b"\x45\x73").peekable();
    // storage address list is subset of account list, both are sorted
    for entry in db.prefix_iterator(b"\x45\x61") {
        // Process each account
        let entry = entry?;
        let (key, value) = entry;

        let account_address = Address::from_slice(&key[2..22]);
        let value = rmp_serde::from_slice::<DbAccountInfo>(&value)?;

        // Process each account
        let mut current_storage: BTreeMap<_, _> = Default::default();

        loop {
            let Some(entry) = storage_iterator.peek() else {
                break;
            };
            let entry = (entry.clone())?;
            let (key, value) = entry;

            let storage_address = Address::from_slice(&key[2..22]);
            let storage_key = B256::from_slice(&key[22..22 + 32]);
            let value = rmp_serde::from_slice::<B256>(&value)?;

            if storage_address != account_address {
                // If the storage address does not match the account address, break
                break;
            }

            current_storage.insert(storage_key, value);
            storage_iterator.next();
        }
        flush_account(
            &mut file,
            account_address,
            value,
            std::mem::take(&mut current_storage),
            &contracts,
        )?;
    }
    Ok(())
}
