use std::{
    fs::File,
    io::Read,
    str::FromStr,
    sync::{Arc, RwLock, atomic::{AtomicBool, AtomicU64, Ordering}},
    thread::{Builder, JoinHandle},
    time::Duration, task::Context,
};

// use solana_client::rpc_client::RpcClient;
use crate::{
    account_write_filter::{self, AccountWriteRoute},
    grpc_plugin_source::{self, FilterConfig, SourceConfig},
    mango::GroupConfig,
    mango_v3_perp_crank_sink::MangoV3PerpCrankSink,
    metrics, blockhash_poller, transaction_sender, states::TransactionSendRecord, rotating_queue::RotatingQueue,
};
use futures::{task::noop_waker};
use crossbeam_channel::{Sender, unbounded};
use log::info;
use chrono::{DateTime, Utc};
use solana_client::{nonblocking::rpc_client::RpcClient, tpu_client::TpuClient};
use solana_quic_client::{QuicPool, QuicConnectionManager, QuicConfig};
use solana_sdk::{instruction::Instruction, pubkey::Pubkey, signature::Keypair, hash::Hash, transaction::Transaction, signer::Signer};

pub fn start(
    tx_record_sx: Sender<TransactionSendRecord>,
    exit_signal: Arc<AtomicBool>,
    blockhash: Arc<RwLock<Hash>>,
    current_slot: Arc<AtomicU64>,
    tpu_client: Arc<TpuClient<QuicPool, QuicConnectionManager, QuicConfig>>,
    group: &GroupConfig,
    identity: &Keypair,
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
        account_ids: group.perp_markets.iter().map(|m| m.events_key.clone()).collect(),
    };


    let (instruction_sender, instruction_receiver) = unbounded::<Vec<Instruction>>();
    let identity = Keypair::from_bytes(identity.to_bytes().as_slice()).unwrap();
    Builder::new().name("crank-tx-sender".into()).spawn(move || {
        info!("crank-tx-sender signing with keypair pk={:?}", identity.pubkey());
        loop {
            if exit_signal.load(Ordering::Acquire) {
                break;
            }

            if let Ok(ixs) = instruction_receiver.recv() {
                // TODO add priority fee

                let tx = Transaction::new_signed_with_payer(
                    &ixs,
                    Some(&identity.pubkey()),
                    &[&identity],
                    *blockhash.read().unwrap(),
                );
                // TODO: find perp market pk and resolve import issue between solana program versions
                // tx_record_sx.send(TransactionSendRecord {
                //     signature:  tx.signatures[0],
                //     sent_at: Utc::now(),
                //     sent_slot: current_slot.load(Ordering::Acquire),
                //     market_maker: identity.pubkey(),
                //     market: c.perp_market_pk,
                // });
                let ok = tpu_client.send_transaction(&tx);
                info!("crank-tx-sender tx={:?} ok={ok}", tx.signatures[0]);
                
            }
        }
    }).unwrap();

    tokio::spawn(async move {
            let config: SourceConfig = {
                let mut file = File::open("source.toml").expect("source.toml file in cwd");
                let mut contents = String::new();
                file.read_to_string(&mut contents)
                    .expect("source.toml to contain data");
                toml::from_str(&contents).unwrap()
            };

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

            grpc_plugin_source::process_events(
                &config,
                &filter_config,
                account_write_queue_sender,
                slot_queue_sender,
                metrics_tx.clone(),
            ).await;


            // TODO also implement websocket handler
            //   websocket_source::process_events(
            //     &config.source,
            //     account_write_queue_sender,
            //     slot_queue_sender,
            // ).await;
        });

}
