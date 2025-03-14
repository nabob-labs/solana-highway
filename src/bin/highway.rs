use {
    anyhow::Context,
    clap::{Parser, Subcommand},
    futures::future::{self, Either, FutureExt},
    jsonrpsee::http_client::HttpClientBuilder,
    solana_sdk::{
        commitment_config::CommitmentConfig,
        pubkey::Pubkey,
        signature::{read_keypair, Keypair},
    },
    std::{
        convert::identity,
        fs,
        path::PathBuf,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    },
    tokio::{
        runtime::Builder,
        signal::unix::{signal, SignalKind},
        sync::{broadcast, oneshot},
        task::JoinHandle,
    },
    tracing::{info, warn},
    solana_highway::{
        blockhash_queue::BlockhashQueue,
        cluster_tpu_info::ClusterTpuInfo,
        config::{load_config, ConfigHighway, ConfigHighwayGatewayClient, ConfigMetricsUpstream},
        feature_flags::FeatureSet,
        grpc_geyser::GeyserSubscriber,
        grpc_highway::GrpcServer,
        grpc_metrics::GrpcClient as GrpcMetricsClient,
        metrics::highway as metrics,
        quic::{QuicClient, QuicClientMetric},
        quic_solana::ConnectionCache,
        rpc::{rpc_admin::RpcClient, rpc_solana_like::RpcServerImpl, RpcServer, RpcServerType},
        setup_tracing,
        stake::StakeInfo,
        task_group::TaskGroup,
        transactions::{GrpcRootedTxReceiver, SendTransactionsPool},
        util::{IdentityFlusherWaitGroup, PubkeySigner, ValueObserver, WaitShutdown},
    },
};

#[derive(Debug, Parser)]
#[clap(author, version, about)]
struct Args {
    /// Path to config
    #[clap(long)]
    pub config: PathBuf,

    /// Only check config and exit
    #[clap(long, default_value_t = false)]
    pub check: bool,

    #[command(subcommand)]
    pub command: Option<ArgsCommands>,
}

#[derive(Debug, Subcommand)]
enum ArgsCommands {
    /// Highway admin RPC interface
    Admin {
        #[command(subcommand)]
        cmd: ArgsCommandAdmin,
    },
}

#[derive(Debug, Subcommand)]
enum ArgsCommandAdmin {
    /// Print current identity
    GetIdentity,
    /// Set new identity from file
    SetIdentity {
        /// Path to file with Keypair
        #[clap(long)]
        identity: Option<PathBuf>,
    },
    /// Reset identity
    ResetIdentityKeypair,
}

fn main() -> anyhow::Result<()> {
    Builder::new_multi_thread()
        .thread_name_fn(|| {
            static ATOMIC_ID: AtomicUsize = AtomicUsize::new(0);
            let id = ATOMIC_ID.fetch_add(1, Ordering::Relaxed);
            format!("highway.tokio{id:02}")
        })
        .enable_all()
        .build()?
        .block_on(main2())
}

async fn main2() -> anyhow::Result<()> {
    let args = Args::parse();
    let config: ConfigHighway = load_config(&args.config).await?;
    if args.check {
        return Ok(());
    }

    setup_tracing(config.tracing.json)?;

    match args.command {
        Some(ArgsCommands::Admin { cmd }) => run_cmd_admin(config, cmd).await,
        None => run_highway(config).await,
    }
}

async fn run_cmd_admin(config: ConfigHighway, admin_cmd: ArgsCommandAdmin) -> anyhow::Result<()> {
    let addr = format!("http://{}", config.listen_admin.bind[0]);
    let client = HttpClientBuilder::default().build(addr)?;

    match admin_cmd {
        ArgsCommandAdmin::GetIdentity => {
            let identity = client.get_identity().await?;
            println!("{identity}");
        }
        ArgsCommandAdmin::SetIdentity { identity } => {
            let identity_prev = client.get_identity().await?;

            if let Some(identity) = identity {
                let identity = fs::canonicalize(&identity)
                    .with_context(|| format!("Unable to access path: {identity:?}"))?;
                client
                    .set_identity(identity.display().to_string(), false)
                    .await?;
            } else {
                let mut stdin = std::io::stdin();
                let identity = read_keypair(&mut stdin)
                    .map_err(|error| anyhow::anyhow!(error.to_string()))
                    .context("Unable to read JSON keypair from stdin")?;
                client
                    .set_identity_from_bytes(Vec::from(identity.to_bytes()), false)
                    .await?;
            }

            let identity = client.get_identity().await?;
            anyhow::ensure!(
                identity != identity_prev,
                format!("Failed to update identity: {identity} (new) != {identity_prev} (old)")
            );
            println!("Successfully update identity to {identity}");
        }
        ArgsCommandAdmin::ResetIdentityKeypair => {
            client.reset_identity().await?;
        }
    }

    Ok(())
}

async fn spawn_highway_gw_listener(
    highway_gw_config: ConfigHighwayGatewayClient,
    mut identity_observer: ValueObserver<PubkeySigner>,
    tx_sender: RpcServerImpl,
    expected_identity: Option<Pubkey>,
    features: FeatureSet,
    mut stop_rx: oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    loop {
        let highway_gw_config2 = highway_gw_config.clone();
        let tx_sender2 = tx_sender.clone();
        let features = features.clone();
        let mut identity_observer2 = identity_observer.clone();
        let (stop_tx2, stop_rx2) = tokio::sync::oneshot::channel();
        let fut = identity_observer.until_value_change(move |current_identity| {
            if let Some(expected_identity) = expected_identity {
                if current_identity.pubkey() != expected_identity {
                    let actual_pubkey = current_identity.pubkey();
                    warn!("expected identity: {expected_identity}, actual identity: {actual_pubkey}");
                    warn!("will not connect to highway-gateway with identity: {actual_pubkey}, waiting for correct identity to be set...");
                    future::pending().boxed()
                } else {
                    GrpcServer::run_with(
                        Arc::new(current_identity),
                        highway_gw_config2.clone(),
                        tx_sender2.clone(),
                        features,
                        stop_rx2,
                    ).boxed()
                }
            } else {
                GrpcServer::run_with(
                    Arc::new(current_identity),
                    highway_gw_config2.clone(),
                    tx_sender2.clone(),
                    features,
                    stop_rx2,
                ).boxed()
            }
        });
        tokio::select! {
            result = fut => {
                match result {
                    Either::Left(_) => {}
                    Either::Right(_) => {
                        warn!("highway-gateway listener stopped");
                    }
                }
            },
            _ = &mut stop_rx => {
                drop(stop_tx2);
                return Ok(());
            },
            current_identity = identity_observer2.observe() => {
                if let Some(expected_identity) = expected_identity {
                    if current_identity.pubkey() != expected_identity {
                        drop(stop_tx2);
                    }
                }
            }
        }
    }
}

fn spawn_lewis_metric_subscriber(
    config: Option<ConfigMetricsUpstream>,
    mut rx: broadcast::Receiver<QuicClientMetric>,
) -> JoinHandle<()> {
    let grpc_metrics = GrpcMetricsClient::new(config);
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(metric) => match metric {
                    QuicClientMetric::SendAttempts {
                        sig,
                        leader,
                        leader_tpu_addr,
                        slots,
                        error,
                    } => {
                        grpc_metrics.emit_send_attempt(
                            &sig,
                            &leader,
                            slots.as_slice(),
                            leader_tpu_addr,
                            error,
                        );
                    }
                },
                Err(broadcast::error::RecvError::Closed) => {
                    break;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    warn!("lewis metrics subscriber lagged behind");
                }
            }
        }
    })
}

async fn run_highway(config: ConfigHighway) -> anyhow::Result<()> {
    metrics::init();
    if let Some(identity) = config.identity.expected {
        metrics::quic_set_identity_expected(identity);
    }
    // let flush_identity = Arc::new(flush_identity);
    let (shutdown_geyser_tx, shutdown_geyser_rx) = oneshot::channel();
    let (geyser, mut geyser_handle) = GeyserSubscriber::new(
        shutdown_geyser_rx,
        config.upstream.primary_grpc.clone(),
        config
            .upstream
            .secondary_grpc
            .unwrap_or(config.upstream.primary_grpc),
    );
    let blockhash_queue = BlockhashQueue::new(&geyser);
    let cluster_tpu_info = ClusterTpuInfo::new(
        config.upstream.rpc.clone(),
        &geyser,
        config.upstream.cluster_nodes_update_interval,
        config.blocklist,
    )
    .await;

    let rooted_tx_geyser_rx = geyser
        .subscribe_transactions()
        .await
        .expect("failed to subscribe geyser transactions");
    let (rooted_transactions_rx, rooted_tx_loop_fut) =
        GrpcRootedTxReceiver::new(rooted_tx_geyser_rx);

    let identity_flusher_wg = IdentityFlusherWaitGroup::default();

    let initial_identity = config.identity.keypair.unwrap_or(Keypair::new());
    let (quic_session, quic_identity_man) = ConnectionCache::new(
        config.quic.clone(),
        initial_identity,
        identity_flusher_wg.clone(),
    );

    let quic_tx_sender = QuicClient::new(
        Arc::new(cluster_tpu_info.clone()),
        config.quic.clone(),
        Arc::new(quic_session),
    );

    let quic_tx_metrics_listener = quic_tx_sender.subscribe_metrics();
    let lewis = spawn_lewis_metric_subscriber(config.metrics_upstream, quic_tx_metrics_listener);

    let (send_transactions, send_tx_pool_fut) = SendTransactionsPool::spawn(
        config.send_transaction_service,
        Arc::new(blockhash_queue.clone()),
        Box::new(rooted_transactions_rx),
        Arc::new(quic_tx_sender.clone()),
    )
    .await;

    // Add all flusher here
    identity_flusher_wg
        .add_flusher(Box::new(send_transactions.clone()))
        .await;

    let rpc = solana_client::nonblocking::rpc_client::RpcClient::new_with_commitment(
        config.upstream.rpc.clone(),
        CommitmentConfig::finalized(),
    );
    let stake = StakeInfo::new(
        rpc,
        config.upstream.stake_update_interval,
        quic_identity_man.observe_identity_change(),
    );

    let quic_identity_observer = quic_identity_man.observe_signer_change();
    // Run RPC admin
    let rpc_admin = RpcServer::new(
        config.listen_admin.bind[0],
        RpcServerType::Admin {
            quic_identity_man,
            allowed_identity: config.identity.expected,
        },
    )
    .await?;

    // Run RPC solana-like
    let rpc_solana_like = RpcServer::new(
        config.listen_solana_like.bind[0],
        RpcServerType::SolanaLike {
            stp: send_transactions.clone(),
            rpc: config.upstream.rpc.clone(),
            proxy_sanitize_check: config.listen_solana_like.proxy_sanitize_check,
            proxy_preflight_check: config.listen_solana_like.proxy_preflight_check,
        },
    )
    .await?;

    // Run gRPC to highway-gateway
    let (stop_highway_gw_listener_tx, stop_highway_gw_listener_rx) = oneshot::channel();
    let highway_gw_listener = if let Some(config_highway_gateway) = config.highway_gateway {
        if config_highway_gateway.endpoints.is_empty() {
            warn!("no endpoints for highway-gateway with existed config");
            None
        } else {
            let highway_gw_config = config_highway_gateway.clone();
            let quic_identity_observer = quic_identity_observer;
            let expected_identity = config.identity.expected;

            let tx_sender = RpcServer::create_solana_like_rpc_server_impl(
                send_transactions.clone(),
                config.upstream.rpc.clone(),
                config.listen_solana_like.proxy_sanitize_check,
                config.listen_solana_like.proxy_preflight_check,
            )
            .await
            .expect("rpc server impl");

            info!("starting highway-gateway listener");
            let h = tokio::spawn(async move {
                spawn_highway_gw_listener(
                    highway_gw_config,
                    quic_identity_observer,
                    tx_sender,
                    expected_identity,
                    config.features,
                    stop_highway_gw_listener_rx,
                )
                .await
            })
            .map(|result| result.map_err(anyhow::Error::new).and_then(identity));

            Some(h.boxed())
        }
    } else {
        warn!("Skipping highway-gateway listener, no config provided");
        None
    };

    let mut sigint = signal(SignalKind::interrupt())?;

    let mut tg = TaskGroup::default();

    tg.spawn_cancelable("lewis", async move {
        lewis.await.expect("lewis");
    });

    tg.spawn_with_shutdown("geyser", |mut stop| async move {
        tokio::select! {
            result = &mut geyser_handle => {
                result.expect("geyser handle").expect("geyser result");
            },
            _ = &mut stop => {
                let _ = shutdown_geyser_tx.send(());
                geyser_handle.await.expect("geyser handle").expect("geyser result");
            },
        }
    });

    tg.spawn_with_shutdown("blockhash_queue", |mut stop| async move {
        tokio::select! {
            result = blockhash_queue.clone().wait_shutdown() => {
                result.expect("blockhash_queue");
            },
            _ = &mut stop => {
                blockhash_queue.shutdown();
                blockhash_queue.wait_shutdown().await.expect("blockhash_queue shutdown");
            },
        }
    });

    tg.spawn_with_shutdown("cluster_tpu_info", |mut stop| async move {
        tokio::select! {
            result = cluster_tpu_info.clone().wait_shutdown() => {
                result.expect("cluster_tpu_info");
            },
            _ = &mut stop => {
                cluster_tpu_info.shutdown();
                cluster_tpu_info.wait_shutdown().await.expect("cluster_tpu_info shutdown");
            },
        }
    });

    tg.spawn_cancelable("rooted_transactions", async move {
        rooted_tx_loop_fut.await;
    });

    tg.spawn_cancelable("send_transactions_pool", send_tx_pool_fut);

    tg.spawn_with_shutdown("stake", |mut stop| async move {
        tokio::select! {
            result = stake.clone().wait_shutdown() => {
                result.expect("stake");
            },
            _ = &mut stop => {
                stake.shutdown();
                stake.wait_shutdown().await.expect("stake shutdown");
            },
        }
    });

    if let Some(mut highway_gw_listener) = highway_gw_listener {
        tg.spawn_with_shutdown("highway_gw_listener", |mut stop| async move {
            tokio::select! {
                result = &mut highway_gw_listener => {
                    result.expect("highway_gw_listener");
                },
                _ = &mut stop => {
                    let _ = stop_highway_gw_listener_tx.send(());
                    highway_gw_listener.await.expect("highway_gw_listener");
                },
            }
        });
    }

    tg.spawn_cancelable("SIGINT", async move {
        sigint.recv().await;
        info!("SIGINT received...");
    });

    let (first, result, rest) = tg.wait_one().await.expect("task group empty");
    rpc_admin.shutdown();
    rpc_solana_like.shutdown();

    warn!("first task group finished {first} with  {result:?}");

    for (name, result) in rest {
        if let Err(e) = result {
            tracing::error!("task: {name} shutdown with: {e:?}");
        }
    }

    Ok(())
}
