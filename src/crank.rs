use crate::{
    account_write_filter::{self, AccountWriteRoute},
    grpc_plugin_source::FilterConfig,
    helpers::to_sp_pk,
    mango::GroupConfig,
    mango_v3_perp_crank_sink::MangoV3PerpCrankSink,
    metrics,
    states::{KeeperInstruction, TransactionSendRecord},
    tpu_manager::TpuManager,
    websocket_source::{self, KeeperConfig},
};
use async_channel::unbounded;
use chrono::Utc;
use log::*;
use solana_sdk::{
    hash::Hash, instruction::Instruction, pubkey::Pubkey, signature::Keypair, signer::Signer,
    transaction::Transaction,
};
use std::{
    str::FromStr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::RwLock;

pub fn start(
    config: KeeperConfig,
    exit_signal: Arc<AtomicBool>,
    blockhash: Arc<RwLock<Hash>>,
    current_slot: Arc<AtomicU64>,
    tpu_manager: TpuManager,
    group: &GroupConfig,
    identity: &Keypair,
    prioritization_fee: u64,
) {
    let perp_queue_pks: Vec<_> = group
        .perp_markets
        .iter()
        .map(|m| {
            (
                Pubkey::from_str(&m.public_key).unwrap(),
                Pubkey::from_str(&m.events_key).unwrap(),
            )
        })
        .collect();
    let group_pk = Pubkey::from_str(&group.public_key).unwrap();
    let cache_pk = Pubkey::from_str(&group.cache_key).unwrap();
    let mango_program_id = Pubkey::from_str(&group.mango_program_id).unwrap();
    let filter_config = FilterConfig {
        program_ids: vec![group.mango_program_id.clone()],
        account_ids: group
            .perp_markets
            .iter()
            .map(|m| m.events_key.clone())
            .collect(),
    };

    let (instruction_sender, instruction_receiver) = unbounded::<(Pubkey, Vec<Instruction>)>();
    let identity = Keypair::from_bytes(identity.to_bytes().as_slice()).unwrap();
    tokio::spawn(async move {
        info!(
            "crank-tx-sender signing with keypair pk={:?}",
            identity.pubkey()
        );
        let prioritization_fee_ix =
            solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_price(
                prioritization_fee,
            );

        loop {
            if exit_signal.load(Ordering::Acquire) {
                break;
            }

            if let Ok((market, mut ixs)) = instruction_receiver.recv().await {
                ixs.insert(0, prioritization_fee_ix.clone());

                let tx = Transaction::new_signed_with_payer(
                    &ixs,
                    Some(&identity.pubkey()),
                    &[&identity],
                    *blockhash.read().await,
                );

                let tx_send_record = TransactionSendRecord {
                    signature: tx.signatures[0],
                    sent_at: Utc::now(),
                    sent_slot: current_slot.load(Ordering::Acquire),
                    market_maker: None,
                    market: Some(to_sp_pk(&market)),
                    priority_fees: prioritization_fee,
                    keeper_instruction: Some(KeeperInstruction::ConsumeEvents),
                };

                let ok = tpu_manager.send_transaction(&tx, tx_send_record).await;
                trace!("send tx={:?} ok={ok}", tx.signatures[0]);
            }
        }
    });

    tokio::spawn(async move {
        let metrics_tx = metrics::start(
            metrics::MetricsConfig {
                output_stdout: true,
                output_http: false,
            },
            "crank".into(),
        );

        let routes = vec![AccountWriteRoute {
            matched_pubkeys: perp_queue_pks
                .iter()
                .map(|(_, evq_pk)| evq_pk.clone())
                .collect(),
            sink: Arc::new(MangoV3PerpCrankSink::new(
                perp_queue_pks,
                group_pk,
                cache_pk,
                mango_program_id,
                instruction_sender,
            )),
            timeout_interval: Duration::default(),
        }];

        let (account_write_queue_sender, slot_queue_sender) =
            account_write_filter::init(routes, metrics_tx.clone()).expect("filter initializes");

        info!("start processing grpc events");

        // grpc_plugin_source::process_events(
        //     &config,
        //     &filter_config,
        //     account_write_queue_sender,
        //     slot_queue_sender,
        //     metrics_tx.clone(),
        // ).await;

        websocket_source::process_events(
            config,
            &filter_config,
            account_write_queue_sender,
            slot_queue_sender,
        )
        .await;
    });
}
