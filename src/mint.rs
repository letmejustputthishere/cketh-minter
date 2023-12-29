use crate::address::Address;
use crate::assets::Asset;
use crate::eth_logs::{report_transaction_error, MintEvent, MintEventError};
use crate::eth_rpc::{BlockSpec, HttpOutcallError};
use crate::eth_rpc_client::EthRpcClient;
use crate::guard::TimerGuard;
use crate::logs::{DEBUG, INFO};
use crate::numeric::BlockNumber;
use crate::state::{
    audit::process_event, event::EventType, mutate_state, read_state, State, TaskType,
};
use crate::storage::store_asset;
use ic_canister_log::log;

use ic_cdk::api::management_canister::main::raw_rand;
use std::cmp::{min, Ordering};
use std::time::Duration;

async fn generate_metadata_and_media(
    generate_media: fn([u8; 32]) -> Asset,
    generate_metadata: fn([u8; 32]) -> Asset,
) {
    let _guard = match TimerGuard::new(TaskType::GenerateMetadataAndAssets) {
        Ok(guard) => guard,
        Err(_) => return,
    };

    let events = read_state(|s| (s.events_to_generate.clone()));

    let error_count = 0;

    for (event_source, event) in events {
        let (raw_rand,): (Vec<u8>,) = raw_rand()
            .await
            // TODO: make sure its safe to trap here
            .unwrap_or_else(|_e| ic_cdk::trap("call to raw_rand failed"));
        let raw_rand_32_bytes: [u8; 32] = raw_rand
            .try_into()
            .unwrap_or_else(|_e| panic!("raw_rand not 32 bytes"));
        let media_asset = generate_media(raw_rand_32_bytes);
        store_asset(format!("media/{}", event.token_id), media_asset);
        let metadata_asset = generate_metadata(raw_rand_32_bytes);
        store_asset(format!("metadata/{}", event.token_id), metadata_asset);
        // let block_index = match client
        //     .transfer(TransferArg {
        //         from_subaccount: None,
        //         to: event.principal.into(),
        //         fee: None,
        //         created_at_time: None,
        //         memo: Some(event.clone().into()),
        //         amount: candid::Nat::from(event.value),
        //     })
        //     .await
        // {
        //     Ok(Ok(block_index)) => block_index.0.to_u64().expect("nat does not fit into u64"),
        //     Ok(Err(err)) => {
        //         log!(INFO, "Failed to mint ckETH: {event:?} {err}");
        //         error_count += 1;
        //         continue;
        //     }
        //     Err(err) => {
        //         log!(
        //             INFO,
        //             "Failed to send a message to the ledger ({ledger_canister_id}): {err:?}"
        //         );
        //         error_count += 1;
        //         continue;
        //     }
        // };
        mutate_state(|s| process_event(s, EventType::GeneratedMetadataAndAssets { event_source }));
        log!(
            INFO,
            "generated metadata and assets for token id {}",
            event.token_id,
        );
    }

    if error_count > 0 {
        log!(
            INFO,
            "Failed to mint {error_count} events, rescheduling the minting"
        );
        ic_cdk_timers::set_timer(crate::MINT_RETRY_DELAY, move || {
            ic_cdk::spawn(generate_metadata_and_media(
                generate_media,
                generate_metadata,
            ))
        });
    }
}

/// Scraps Ethereum logs between `from` and `min(from + MAX_BLOCK_SPREAD, to)` since certain RPC providers
/// require that the number of blocks queried is no greater than MAX_BLOCK_SPREAD.
/// Returns the last block number that was scraped (which is `min(from + MAX_BLOCK_SPREAD, to)`) if there
/// was no error when querying the providers, otherwise returns `None`.
async fn scrape_eth_logs_range_inclusive(
    generate_media: fn([u8; 32]) -> Asset,
    generate_metadata: fn([u8; 32]) -> Asset,
    contract_address: Address,
    minter_address: Address,
    from: BlockNumber,
    to: BlockNumber,
) -> Option<BlockNumber> {
    /// The maximum block spread is introduced by Cloudflare limits.
    /// https://developers.cloudflare.com/web3/ethereum-gateway/
    const MAX_BLOCK_SPREAD: u16 = 799;
    match from.cmp(&to) {
        Ordering::Less | Ordering::Equal => {
            let max_to = from
                .checked_add(BlockNumber::from(MAX_BLOCK_SPREAD))
                .unwrap_or(BlockNumber::MAX);
            let mut last_block_number = min(max_to, to);
            log!(
                DEBUG,
                "Scraping ETH logs from block {:?} to block {:?}...",
                from,
                last_block_number
            );

            let (mint_events, errors) = loop {
                match crate::eth_logs::last_received_eth_events(
                    contract_address,
                    minter_address,
                    from,
                    last_block_number,
                )
                .await
                {
                    Ok((events, errors)) => break (events, errors),
                    Err(e) => {
                        log!(
                            INFO,
                            "Failed to get ETH logs from block {from} to block {last_block_number}: {e:?}",
                        );
                        if e.has_http_outcall_error_matching(
                            HttpOutcallError::is_response_too_large,
                        ) {
                            if from == last_block_number {
                                mutate_state(|s| {
                                    process_event(s, EventType::SkippedBlock(last_block_number));
                                    s.last_scraped_block_number = last_block_number;
                                });
                                return Some(last_block_number);
                            } else {
                                let new_last_block_number = from
                                    .checked_add(last_block_number
                                            .checked_sub(from)
                                            .expect("last_scraped_block_number is greater or equal than from")
                                            .div_by_two())
                                    .expect("must be less than last_scraped_block_number");
                                log!(INFO, "Too many logs received in range [{from}, {last_block_number}]. Will retry with range [{from}, {new_last_block_number}]");
                                last_block_number = new_last_block_number;
                                continue;
                            }
                        }
                        return None;
                    }
                };
            };

            for mint in mint_events {
                log!(
                    INFO,
                    "Received event {mint:?}; will generate metadata and assets for token id {}",
                    mint.token_id,
                );
                mutate_state(|s| process_event(s, EventType::AcceptedMint(mint)));
            }
            if read_state(State::has_events_to_mint) {
                ic_cdk_timers::set_timer(Duration::from_secs(0), move || {
                    ic_cdk::spawn(generate_metadata_and_media(
                        generate_media,
                        generate_metadata,
                    ))
                });
            }
            for error in errors {
                if let MintEventError::InvalidEventSource { source, error } = &error {
                    mutate_state(|s| {
                        process_event(
                            s,
                            EventType::InvalidMint {
                                event_source: *source,
                                reason: error.to_string(),
                            },
                        )
                    });
                }
                report_transaction_error(error);
            }
            mutate_state(|s| s.last_scraped_block_number = last_block_number);
            Some(last_block_number)
        }
        Ordering::Greater => {
            ic_cdk::trap(&format!(
                "BUG: last scraped block number ({:?}) is greater than the last queried block number ({:?})",
                from, to
            ));
        }
    }
}

pub async fn scrape_eth_logs(
    generate_media: fn([u8; 32]) -> Asset,
    generate_metadata: fn([u8; 32]) -> Asset,
) {
    let _guard = match TimerGuard::new(TaskType::ScrapEthLogs) {
        Ok(guard) => guard,
        Err(_) => return,
    };
    let contract_address = read_state(|s| s.ethereum_contract_address);
    let minter_address = read_state(|s| s.minter_address);
    let last_block_number = match update_last_observed_block_number().await {
        Some(block_number) => block_number,
        None => {
            log!(
                DEBUG,
                "[scrape_eth_logs]: skipping scraping ETH logs: no last observed block number"
            );
            return;
        }
    };
    let mut last_scraped_block_number = read_state(|s| s.last_scraped_block_number);

    while last_scraped_block_number < last_block_number {
        let next_block_to_query = last_scraped_block_number
            .checked_increment()
            .unwrap_or(BlockNumber::MAX);
        last_scraped_block_number = match scrape_eth_logs_range_inclusive(
            generate_media,
            generate_metadata,
            contract_address,
            minter_address,
            next_block_to_query,
            last_block_number,
        )
        .await
        {
            Some(last_scraped_block_number) => last_scraped_block_number,
            None => {
                return;
            }
        };
    }
}

pub async fn update_last_observed_block_number() -> Option<BlockNumber> {
    let block_height = read_state(State::ethereum_block_height);
    match read_state(EthRpcClient::from_state)
        .eth_get_block_by_number(BlockSpec::Tag(block_height))
        .await
    {
        Ok(latest_block) => {
            let block_number = Some(latest_block.number);
            mutate_state(|s| s.last_observed_block_number = block_number);
            block_number
        }
        Err(e) => {
            log!(
                INFO,
                "Failed to get the latest {block_height} block number: {e:?}"
            );
            read_state(|s| s.last_observed_block_number)
        }
    }
}