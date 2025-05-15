// Using rmp(rust-messagepack), read ~/abci_state.rmp.

use alloy::consensus::constants::KECCAK_EMPTY;
use alloy::genesis::GenesisAccount;
use alloy::primitives::{Address, B256};
use alloy::rlp::Encodable;
use clap::Parser;
use revm::primitives::Bytecode;
use serde::{Deserialize, Serialize};
use std::{fs::File, io::Write};
use types::{AbciState, EvmBlock, EvmDb};

mod types;

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
    let file = File::open(args.file)?;
    let mut reader = std::io::BufReader::new(file);

    let abci_state: AbciState = rmp_serde::decode::from_read(&mut reader)?;
    let evm = abci_state.exchange.hyper_evm;
    let header = match &evm.latest_block2 {
        EvmBlock::Reth115(block) => block.header(),
    };

    let output = format!("{}.jsonl", header.number);
    {
        let EvmDb::InMemory {
            accounts,
            contracts,
        } = evm.state2.evm_db;
        let contracts = contracts
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>();
        let file = File::create(&output)?;
        let mut file = std::io::BufWriter::new(file);
        writeln!(
            file,
            "{}",
            serde_json::to_string(&StateRoot { root: B256::ZERO })?
        )?;
        for (address, account) in accounts {
            // if account.info.balance.is_zero()
            //     && account.info.nonce.is_zero()
            //     && account.info.code_hash.is_zero()
            //     && account.storage.is_empty()
            // {
            //     continue;
            // }
            let is_eoa = account.info.code_hash == KECCAK_EMPTY;
            let code = if is_eoa {
                None
            } else {
                match &contracts[&account.info.code_hash] {
                    Bytecode::LegacyAnalyzed(x) => Some(x.bytecode().clone()),
                    Bytecode::LegacyRaw(x) => Some(x.clone()),
                    _ => panic!(
                        "Unexpected bytecode type: {:?}",
                        contracts[&account.info.code_hash]
                    ),
                }
            };
            let account = GenesisAccount {
                balance: account.info.balance,
                nonce: Some(account.info.nonce),
                code: code.map(|x| x.into()),
                storage: if is_eoa {
                    None
                } else {
                    Some(
                        account
                            .storage
                            .into_iter()
                            .map(|(k, v)| (k.into(), v.into()))
                            .collect(),
                    )
                },
                ..Default::default()
            };
            let account = GenesisAccountWithAddress {
                genesis_account: account,
                address,
            };
            let account_json = serde_json::to_string(&account)?;
            writeln!(file, "{}", account_json)?;
        }
    }
    let output = format!("{}.rlp", header.number);
    {
        let mut buf = vec![];
        header.encode(&mut buf);
        let mut file = File::create(&output)?;
        file.write_all(&buf)?;
    }

    Ok(())
}
