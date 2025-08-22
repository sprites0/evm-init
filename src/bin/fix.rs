use alloy::genesis::GenesisAccount;
use alloy::primitives::{Address, B256};
use clap::Parser;
use evm_init::types::{AbciState, EvmDb};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs::File, io::Write};

#[derive(Parser)]
struct Args {
    /// Path to the abci state
    abci_state: String,
    /// Path to the original genesis jsonl file e.g., 10000000.jsonl
    original: String,
    /// Path to the fixed genesis jsonl file e.g., 10000000.fixed.jsonl
    fixed: String,
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
    let file = File::open(args.abci_state)?;
    let mut reader = std::io::BufReader::new(file);

    let abci_state: AbciState = rmp_serde::decode::from_read(&mut reader)?;
    let evm = abci_state.exchange.hyper_evm;

    let original_file = File::open(args.original)?;
    let original_reader = std::io::BufReader::new(original_file);
    let mut original_lines = std::io::BufRead::lines(original_reader);

    let mut fixed_output = {
        let file = File::create(args.fixed)?;
        std::io::BufWriter::new(file)
    };

    writeln!(fixed_output, "{}", original_lines.next().unwrap()?)?;

    let EvmDb::InMemory {
        accounts,
        contracts,
    } = evm.state2.evm_db
    else {
        println!("This tool is written before file-backed DB is introduced, so you can skip this.");
        return Ok(());
    };
    let mut count = 0;
    let contracts: HashMap<_, _> = contracts.into_iter().collect();
    let accounts: HashMap<_, _> = accounts.into_iter().collect();
    for line in original_lines {
        let line = line?;
        let mut account: GenesisAccountWithAddress = serde_json::from_str(&line)?;
        let fixed_account = if let Some(code) = &account.genesis_account.code {
            let account_in_db = accounts[&account.address].clone();
            let contract = &contracts[&account_in_db.info.code_hash];
            let real_code = contract.original_bytes();
            if code != &real_code {
                println!("Fixed account: {:?}", account.address);
                count += 1;
            }
            account.genesis_account.code = Some(real_code.into());
            account
        } else {
            account
        };
        let account_json = serde_json::to_string(&fixed_account)?;
        writeln!(fixed_output, "{}", account_json)?;
    }

    println!("Fixed {} accounts", count);

    Ok(())
}
