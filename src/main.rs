// Using rmp(rust-messagepack), read ~/abci_state.rmp.

use alloy::consensus::constants::KECCAK_EMPTY;
use alloy::genesis::GenesisAccount;
use alloy::primitives::{Address, B256};
use alloy::rlp::Encodable;
use clap::Parser;
use evm_init::types::{AbciState, DbAccount, DbAccountInfo, EvmBlock, EvmDb};
use revm::primitives::Bytecode;
use rocksdb::DB;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::{fs::File, io::Write};

#[derive(Parser)]
struct Args {
    /// Path to the abci state
    file: String,
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

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let abci_state_path: PathBuf = args.file.into();
    let file = File::open(&abci_state_path)?;
    let mut reader = std::io::BufReader::new(file);

    let abci_state: AbciState = rmp_serde::decode::from_read(&mut reader)?;
    let evm = abci_state.exchange.hyper_evm;
    let header = match &evm.latest_block2 {
        EvmBlock::Reth115(block) => block.header(),
    };

    let jsonl_output = format!("{}.jsonl", header.number);
    {
        match evm.state2.evm_db {
            EvmDb::InMemory {
                accounts,
                contracts,
            } => handle_in_memory_db(&jsonl_output, accounts, contracts),
            EvmDb::NoEvmDb => {
                println!("abci_state uses file-backed db, processing...");
                let file_db_path = abci_state_path
                    .parent()
                    .unwrap()
                    .join("evm_db_hub_fast")
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
        "reth init-state --without-evm --chain testnet --header {rlp_output} --header-hash 0x{:x} {jsonl_output} --total-difficulty 0",
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
    let db = DB::open_default(db_path.into()).unwrap();
    let file = File::create(jsonl_output)?;
    let mut file = std::io::BufWriter::new(file);
    // 0x4561: account
    // 0x4563: contract
    // 0x4573: storage

    // iterate all contracts; for convenience just load all of them into memory
    let mut contracts = HashMap::new();
    for entry in db.prefix_iterator(b"\x45\x63") {
        // Process each contract
        let entry = entry?;
        let (key, value) = entry;

        let code_hash = B256::from_slice(&key[2..34]);
        contracts.insert(
            code_hash,
            rmp_serde::from_slice::<Bytecode>(&value)
                .map_err(|e| anyhow::anyhow!("Failed to deserialize contract: {}", e))?,
        );
    }

    // iterate all accounts
    let mut storage_prefix: [u8; 2 + 20 + 32] = [0u8; 54];
    storage_prefix[0..2].copy_from_slice(&[0x45, 0x73]);
    for entry in db.prefix_iterator(b"\x45\x61") {
        // Process each account
        let entry = entry?;
        let (key, value) = entry;

        let address = Address::from_slice(&key[2..22]);
        let account = rmp_serde::from_slice::<DbAccountInfo>(&value)
            .map_err(|e| anyhow::anyhow!("Failed to deserialize account: {}", e))?;
        let is_eoa = account.code_hash == KECCAK_EMPTY;
        let storage = if is_eoa {
            None
        } else {
            Some(
                db.prefix_iterator(storage_prefix)
                    .map(|entry| {
                        let (key, value) = entry.unwrap();
                        let storage_key = B256::from_slice(&key[2..34]);
                        let storage_value = B256::from_slice(&value);
                        (storage_key, storage_value)
                    })
                    .collect(),
            )
        };

        let account = to_genesis_account(account, &contracts, storage);

        let account = GenesisAccountWithAddress {
            genesis_account: account,
            address,
        };
        let account_json = serde_json::to_string(&account)?;
        writeln!(file, "{}", account_json)?;
    }

    Ok(())
}
