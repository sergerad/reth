//! Main `stage` command
//!
//! Stage debugging tool

use crate::{
    args::{
        get_secret_key,
        utils::{chain_help, chain_spec_value_parser, SUPPORTED_CHAINS},
        DatabaseArgs, DatadirArgs, NetworkArgs, StageEnum,
    },
    macros::block_executor,
    prometheus_exporter,
    version::SHORT_VERSION,
};
use clap::Parser;
use reth_beacon_consensus::EthBeaconConsensus;
use reth_cli_runner::CliContext;
use reth_config::{
    config::{EtlConfig, HashingConfig, SenderRecoveryConfig, TransactionLookupConfig},
    Config,
};
use reth_db::init_db;
use reth_downloaders::bodies::bodies::BodiesDownloaderBuilder;
use reth_exex::ExExManagerHandle;
use reth_primitives::ChainSpec;
use reth_provider::{
    providers::StaticFileProvider, ProviderFactory, StageCheckpointReader, StageCheckpointWriter,
    StaticFileProviderFactory,
};
use reth_stages::{
    stages::{
        AccountHashingStage, BodyStage, ExecutionStage, ExecutionStageThresholds,
        IndexAccountHistoryStage, IndexStorageHistoryStage, MerkleStage, SenderRecoveryStage,
        StorageHashingStage, TransactionLookupStage,
    },
    ExecInput, ExecOutput, Stage, StageExt, UnwindInput, UnwindOutput,
};
use std::{any::Any, net::SocketAddr, path::PathBuf, sync::Arc, time::Instant};
use tracing::*;

/// `reth stage` command
#[derive(Debug, Parser)]
pub struct Command {
    /// The path to the configuration file to use.
    #[arg(long, value_name = "FILE", verbatim_doc_comment)]
    config: Option<PathBuf>,

    /// The chain this node is running.
    ///
    /// Possible values are either a built-in chain or the path to a chain specification file.
    #[arg(
        long,
        value_name = "CHAIN_OR_PATH",
        long_help = chain_help(),
        default_value = SUPPORTED_CHAINS[0],
        value_parser = chain_spec_value_parser
    )]
    chain: Arc<ChainSpec>,

    /// Enable Prometheus metrics.
    ///
    /// The metrics will be served at the given interface and port.
    #[arg(long, value_name = "SOCKET")]
    metrics: Option<SocketAddr>,

    /// The name of the stage to run
    #[arg(value_enum)]
    stage: StageEnum,

    /// The height to start at
    #[arg(long)]
    from: u64,

    /// The end of the stage
    #[arg(long, short)]
    to: u64,

    /// Batch size for stage execution and unwind
    #[arg(long)]
    batch_size: Option<u64>,

    /// The maximum size in bytes of data held in memory before being flushed to disk as a file.
    #[arg(long)]
    etl_file_size: Option<usize>,

    /// Directory where to collect ETL files
    #[arg(long)]
    etl_dir: Option<PathBuf>,

    /// Normally, running the stage requires unwinding for stages that already
    /// have been run, in order to not rewrite to the same database slots.
    ///
    /// You can optionally skip the unwinding phase if you're syncing a block
    /// range that has not been synced before.
    #[arg(long, short)]
    skip_unwind: bool,

    /// Commits the changes in the database. WARNING: potentially destructive.
    ///
    /// Useful when you want to run diagnostics on the database.
    // TODO: We should consider allowing to run hooks at the end of the stage run,
    // e.g. query the DB size, or any table data.
    #[arg(long, short)]
    commit: bool,

    /// Save stage checkpoints
    #[arg(long)]
    checkpoints: bool,

    #[command(flatten)]
    datadir: DatadirArgs,

    #[command(flatten)]
    network: NetworkArgs,

    #[command(flatten)]
    db: DatabaseArgs,
}

impl Command {
    /// Execute `stage` command
    pub async fn execute(self, ctx: CliContext) -> eyre::Result<()> {
        // Raise the fd limit of the process.
        // Does not do anything on windows.
        let _ = fdlimit::raise_fd_limit();

        // add network name to data dir
        let data_dir = self.datadir.resolve_datadir(self.chain.chain);
        let config_path = self.config.clone().unwrap_or_else(|| data_dir.config());

        let config: Config = confy::load_path(config_path).unwrap_or_default();
        info!(target: "reth::cli", "reth {} starting stage {:?}", SHORT_VERSION, self.stage);

        // use the overridden db path if specified
        let db_path = data_dir.db();

        info!(target: "reth::cli", path = ?db_path, "Opening database");
        let db = Arc::new(init_db(db_path, self.db.database_args())?);
        info!(target: "reth::cli", "Database opened");

        let provider_factory = ProviderFactory::new(
            Arc::clone(&db),
            self.chain.clone(),
            StaticFileProvider::read_write(data_dir.static_files())?,
        );
        let mut provider_rw = provider_factory.provider_rw()?;

        if let Some(listen_addr) = self.metrics {
            info!(target: "reth::cli", "Starting metrics endpoint at {}", listen_addr);
            prometheus_exporter::serve(
                listen_addr,
                prometheus_exporter::install_recorder()?,
                Arc::clone(&db),
                provider_factory.static_file_provider(),
                metrics_process::Collector::default(),
                ctx.task_executor,
            )
            .await?;
        }

        let batch_size = self.batch_size.unwrap_or(self.to.saturating_sub(self.from) + 1);

        let etl_config = EtlConfig::new(
            Some(self.etl_dir.unwrap_or_else(|| EtlConfig::from_datadir(data_dir.data_dir()))),
            self.etl_file_size.unwrap_or(EtlConfig::default_file_size()),
        );
        let prune_modes = config.prune.clone().map(|prune| prune.segments).unwrap_or_default();

        let (mut exec_stage, mut unwind_stage): (Box<dyn Stage<_>>, Option<Box<dyn Stage<_>>>) =
            match self.stage {
                StageEnum::Bodies => {
                    let consensus = Arc::new(EthBeaconConsensus::new(self.chain.clone()));

                    let mut config = config;
                    config.peers.trusted_nodes_only = self.network.trusted_only;
                    if !self.network.trusted_peers.is_empty() {
                        for peer in &self.network.trusted_peers {
                            let peer = peer.resolve().await?;
                            config.peers.trusted_nodes.insert(peer);
                        }
                    }

                    let network_secret_path = self
                        .network
                        .p2p_secret_key
                        .clone()
                        .unwrap_or_else(|| data_dir.p2p_secret());
                    let p2p_secret_key = get_secret_key(&network_secret_path)?;

                    let default_peers_path = data_dir.known_peers();

                    let network = self
                        .network
                        .network_config(
                            &config,
                            self.chain.clone(),
                            p2p_secret_key,
                            default_peers_path,
                        )
                        .build(provider_factory.clone())
                        .start_network()
                        .await?;
                    let fetch_client = Arc::new(network.fetch_client().await?);

                    let stage = BodyStage::new(
                        BodiesDownloaderBuilder::default()
                            .with_stream_batch_size(batch_size as usize)
                            .with_request_limit(config.stages.bodies.downloader_request_limit)
                            .with_max_buffered_blocks_size_bytes(
                                config.stages.bodies.downloader_max_buffered_blocks_size_bytes,
                            )
                            .with_concurrent_requests_range(
                                config.stages.bodies.downloader_min_concurrent_requests..=
                                    config.stages.bodies.downloader_max_concurrent_requests,
                            )
                            .build(fetch_client, consensus.clone(), provider_factory.clone()),
                    );
                    (Box::new(stage), None)
                }
                StageEnum::Senders => (
                    Box::new(SenderRecoveryStage::new(SenderRecoveryConfig {
                        commit_threshold: batch_size,
                    })),
                    None,
                ),
                StageEnum::Execution => {
                    let executor = block_executor!(self.chain.clone());
                    (
                        Box::new(ExecutionStage::new(
                            executor,
                            ExecutionStageThresholds {
                                max_blocks: Some(batch_size),
                                max_changes: None,
                                max_cumulative_gas: None,
                                max_duration: None,
                            },
                            config.stages.merkle.clean_threshold,
                            prune_modes,
                            ExExManagerHandle::empty(),
                        )),
                        None,
                    )
                }
                StageEnum::TxLookup => (
                    Box::new(TransactionLookupStage::new(
                        TransactionLookupConfig { chunk_size: batch_size },
                        etl_config,
                        prune_modes.transaction_lookup,
                    )),
                    None,
                ),
                StageEnum::AccountHashing => (
                    Box::new(AccountHashingStage::new(
                        HashingConfig { clean_threshold: 1, commit_threshold: batch_size },
                        etl_config,
                    )),
                    None,
                ),
                StageEnum::StorageHashing => (
                    Box::new(StorageHashingStage::new(
                        HashingConfig { clean_threshold: 1, commit_threshold: batch_size },
                        etl_config,
                    )),
                    None,
                ),
                StageEnum::Merkle => (
                    Box::new(MerkleStage::new_execution(config.stages.merkle.clean_threshold)),
                    Some(Box::new(MerkleStage::default_unwind())),
                ),
                StageEnum::AccountHistory => (
                    Box::new(IndexAccountHistoryStage::new(
                        config.stages.index_account_history,
                        etl_config,
                        prune_modes.account_history,
                    )),
                    None,
                ),
                StageEnum::StorageHistory => (
                    Box::new(IndexStorageHistoryStage::new(
                        config.stages.index_storage_history,
                        etl_config,
                        prune_modes.storage_history,
                    )),
                    None,
                ),
                _ => return Ok(()),
            };
        if let Some(unwind_stage) = &unwind_stage {
            assert_eq!((*exec_stage).type_id(), (**unwind_stage).type_id());
        }

        let checkpoint = provider_rw.get_stage_checkpoint(exec_stage.id())?.unwrap_or_default();

        let unwind_stage = unwind_stage.as_mut().unwrap_or(&mut exec_stage);

        let mut unwind = UnwindInput {
            checkpoint: checkpoint.with_block_number(self.to),
            unwind_to: self.from,
            bad_block: None,
        };

        if !self.skip_unwind {
            while unwind.checkpoint.block_number > self.from {
                let UnwindOutput { checkpoint } = unwind_stage.unwind(&provider_rw, unwind)?;
                unwind.checkpoint = checkpoint;

                if self.checkpoints {
                    provider_rw.save_stage_checkpoint(unwind_stage.id(), checkpoint)?;
                }

                if self.commit {
                    provider_rw.commit()?;
                    provider_rw = provider_factory.provider_rw()?;
                }
            }
        }

        let mut input = ExecInput {
            target: Some(self.to),
            checkpoint: Some(checkpoint.with_block_number(self.from)),
        };

        let start = Instant::now();
        info!(target: "reth::cli", stage = %self.stage, "Executing stage");
        loop {
            exec_stage.execute_ready(input).await?;
            let ExecOutput { checkpoint, done } = exec_stage.execute(&provider_rw, input)?;

            input.checkpoint = Some(checkpoint);

            if self.checkpoints {
                provider_rw.save_stage_checkpoint(exec_stage.id(), checkpoint)?;
            }
            if self.commit {
                provider_rw.commit()?;
                provider_rw = provider_factory.provider_rw()?;
            }

            if done {
                break
            }
        }
        info!(target: "reth::cli", stage = %self.stage, time = ?start.elapsed(), "Finished stage");

        Ok(())
    }
}
