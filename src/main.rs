#![allow(special_module_name)]

mod args;
mod template_receiver;
mod status;
mod error;
mod lottery;
mod downstream_sv1;
mod tproxy_config;
mod tproxy;
mod upstream_sv2;
mod tproxy_utils;

use async_channel::{bounded, unbounded};
use tracing::{error, info, warn, debug};
use tokio::select;
use tokio::{sync::broadcast, task};
use std::{
    net::{IpAddr, SocketAddr},
    str::FromStr,
    sync::Arc,
};
use futures::FutureExt;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let args = match args::Args::from_args() {
        Ok(cfg) => cfg,
        Err(help) => {
            error!("{}", help);
            return;
        }
    };

    let lottery_config: lottery::Configuration = match std::fs::read_to_string(&args.config_path) {
        Ok(c) => match toml::from_str(&c) {
            Ok(c) => c,
            Err(e) => {
                error!("Failed to parse config: {}", e);
                return;
            }
        },
        Err(e) => {
            error!("Failed to read config: {}", e);
            return;
        }
    };

    let proxy_config: tproxy_config::TProxyConfig = match std::fs::read_to_string(&args.config_path) {
        Ok(c) => match toml::from_str(&c) {
            Ok(c) => c,
            Err(e) => {
                error!("Failed to parse config: {}", e);
                return;
            }
        },
        Err(e) => {
            error!("Failed to read config: {}", e);
            return;
        }
    };

    let lottery_handle = task::spawn(async move {
        lottery(lottery_config).await;
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    let tproxy_handle = task::spawn(async move {
        tproxy(proxy_config).await;
    });

    futures::future::join_all(vec![lottery_handle, tproxy_handle]).await;
}

async fn lottery(config: lottery::Configuration) {
    let (status_tx, status_rx) = unbounded();
    let (s_new_t, r_new_t) = bounded(10);
    let (s_prev_hash, r_prev_hash) = bounded(10);
    let (s_solution, r_solution) = bounded(10);
    let (s_message_recv_signal, r_message_recv_signal) = bounded(10);
    let coinbase_output_result = lottery::get_coinbase_output(&config);
    let coinbase_output_len = match coinbase_output_result {
        Ok(coinbase_output) => coinbase_output.len() as u32,
        Err(err) => {
            error!("Failed to get coinbase output: {:?}", err);
            return;
        }
    };
    let tp_authority_public_key = config.tp_authority_public_key;

    let template_rx_res = template_receiver::TemplateRx::connect(
        config.tp_address.parse().unwrap(),
        s_new_t,
        s_prev_hash,
        r_solution,
        r_message_recv_signal,
        status::Sender::Upstream(status_tx.clone()),
        coinbase_output_len,
        tp_authority_public_key,
    )
        .await;

    if let Err(e) = template_rx_res {
        error!("Could not connect to Template Provider: {}", e);
        return;
    }

    let lottery = lottery::Lottery::start(
        config.clone(),
        r_new_t,
        r_prev_hash,
        s_solution,
        s_message_recv_signal,
        status::Sender::DownstreamListener(status_tx),
    );

    // Start the error handling loop
    // See `./status.rs` and `utils/error_handling` for information on how this operates
    loop {
        let task_status = select! {
            task_status = status_rx.recv() => task_status,
            interrupt_signal = tokio::signal::ctrl_c() => {
                match interrupt_signal {
                    Ok(()) => {
                        info!("Interrupt received");
                    },
                    Err(err) => {
                        error!("Unable to listen for interrupt signal: {}", err);
                        // we also shut down in case of error
                    },
                }
                break;
            }
        };
        let task_status: status::Status = task_status.unwrap();

        match task_status.state {
            // Should only be sent by the downstream listener
            status::State::DownstreamShutdown(err) => {
                error!(
                    "SHUTDOWN from Downstream: {}\nTry to restart the downstream listener",
                    err
                );
                break;
            }
            status::State::TemplateProviderShutdown(err) => {
                error!("SHUTDOWN from Upstream: {}\nTry to reconnecting or connecting to a new upstream", err);
                break;
            }
            status::State::Healthy(msg) => {
                info!("HEALTHY message: {}", msg);
            }
            status::State::DownstreamInstanceDropped(downstream_id) => {
                warn!("Dropping downstream instance {} from pool", downstream_id);
                if lottery
                    .safe_lock(|p| p.remove_downstream(downstream_id))
                    .is_err()
                {
                    break;
                }
            }
            status::State::BridgeShutdown(err) => {
                error!("SHUTDOWN from: {}", err);
                break;
            }
            status::State::UpstreamShutdown(err) => {
                error!("SHUTDOWN from: {}", err);
                break;
            }
        }
    }
}


async fn tproxy(proxy_config: tproxy_config::TProxyConfig) {
    let (tx_status, rx_status) = unbounded();

    // `tx_sv1_bridge` sender is used by `Downstream` to send a `DownstreamMessages` message to
    // `Bridge` via the `rx_sv1_downstream` receiver
    // (Sender<downstream_sv1::DownstreamMessages>, Receiver<downstream_sv1::DownstreamMessages>)
    let (tx_sv1_bridge, rx_sv1_downstream) = unbounded();

    // Sender/Receiver to send a SV2 `SubmitSharesExtended` from the `Bridge` to the `Upstream`
    // (Sender<SubmitSharesExtended<'static>>, Receiver<SubmitSharesExtended<'static>>)
    let (tx_sv2_submit_shares_ext, rx_sv2_submit_shares_ext) = bounded(10);

    // Sender/Receiver to send a SV2 `SetNewPrevHash` message from the `Upstream` to the `Bridge`
    // (Sender<SetNewPrevHash<'static>>, Receiver<SetNewPrevHash<'static>>)
    let (tx_sv2_set_new_prev_hash, rx_sv2_set_new_prev_hash) = bounded(10);

    // Sender/Receiver to send a SV2 `NewExtendedMiningJob` message from the `Upstream` to the
    // `Bridge`
    // (Sender<NewExtendedMiningJob<'static>>, Receiver<NewExtendedMiningJob<'static>>)
    let (tx_sv2_new_ext_mining_job, rx_sv2_new_ext_mining_job) = bounded(10);

    // Sender/Receiver to send a new extranonce from the `Upstream` to this `main` function to be
    // passed to the `Downstream` upon a Downstream role connection
    // (Sender<ExtendedExtranonce>, Receiver<ExtendedExtranonce>)
    let (tx_sv2_extranonce, rx_sv2_extranonce) = bounded(1);
    let target = Arc::new(roles_logic_sv2::utils::Mutex::new(vec![0; 32]));

    // Sender/Receiver to send SV1 `mining.notify` message from the `Bridge` to the `Downstream`
    let (tx_sv1_notify, _rx_sv1_notify): (
        broadcast::Sender<sv1_api::server_to_client::Notify>,
        broadcast::Receiver<sv1_api::server_to_client::Notify>,
    ) = broadcast::channel(10);

    // Format `Upstream` connection address
    let upstream_addr = SocketAddr::new(
        IpAddr::from_str(&proxy_config.upstream_address)
            .expect("Failed to parse upstream address!"),
        proxy_config.upstream_port,
    );

    let diff_config = Arc::new(roles_logic_sv2::utils::Mutex::new(proxy_config.upstream_difficulty_config.clone()));

    // Instantiate a new `Upstream` (SV2 Pool)
    let upstream = match upstream_sv2::Upstream::new(
        upstream_addr,
        proxy_config.upstream_authority_pubkey,
        rx_sv2_submit_shares_ext,
        tx_sv2_set_new_prev_hash,
        tx_sv2_new_ext_mining_job,
        proxy_config.min_extranonce2_size,
        tx_sv2_extranonce,
        status::Sender::Upstream(tx_status.clone()),
        target.clone(),
        diff_config.clone(),
    )
        .await
    {
        Ok(upstream) => upstream,
        Err(e) => {
            error!("Failed to create upstream: {}", e);
            return;
        }
    };

    // Spawn a task to do all of this init work so that the main thread
    // can listen for signals and failures on the status channel. This
    // allows for the tproxy to fail gracefully if any of these init tasks
    //fail
    task::spawn(async move {
        // Connect to the SV2 Upstream role
        match upstream_sv2::Upstream::connect(
            upstream.clone(),
            proxy_config.min_supported_version,
            proxy_config.max_supported_version,
        )
            .await
        {
            Ok(_) => info!("Connected to Upstream!"),
            Err(e) => {
                error!("Failed to connect to Upstream EXITING! : {}", e);
                return;
            }
        }

        // Start receiving messages from the SV2 Upstream role
        if let Err(e) = upstream_sv2::Upstream::parse_incoming(upstream.clone()) {
            error!("failed to create sv2 parser: {}", e);
            return;
        }

        debug!("Finished starting upstream listener");
        // Start task handler to receive submits from the SV1 Downstream role once it connects
        if let Err(e) = upstream_sv2::Upstream::handle_submit(upstream.clone()) {
            error!("Failed to create submit handler: {}", e);
            return;
        }

        // Receive the extranonce information from the Upstream role to send to the Downstream role
        // once it connects also used to initialize the bridge
        let (extended_extranonce, up_id) = rx_sv2_extranonce.recv().await.unwrap();
        loop {
            let target: [u8; 32] = target.safe_lock(|t| t.clone()).unwrap().try_into().unwrap();
            if target != [0; 32] {
                break;
            };
            async_std::task::sleep(std::time::Duration::from_millis(100)).await;
        }

        // Instantiate a new `Bridge` and begins handling incoming messages
        let b = tproxy::Bridge::new(
            rx_sv1_downstream,
            tx_sv2_submit_shares_ext,
            rx_sv2_set_new_prev_hash,
            rx_sv2_new_ext_mining_job,
            tx_sv1_notify.clone(),
            status::Sender::Bridge(tx_status.clone()),
            extended_extranonce,
            target,
            up_id,
        );
        tproxy::Bridge::start(b.clone());

        // Format `Downstream` connection address
        let downstream_addr = SocketAddr::new(
            IpAddr::from_str(&proxy_config.downstream_address).unwrap(),
            proxy_config.downstream_port,
        );

        // Accept connections from one or more SV1 Downstream roles (SV1 Mining Devices)
        downstream_sv1::Downstream::accept_connections(
            downstream_addr,
            tx_sv1_bridge,
            tx_sv1_notify,
            status::Sender::DownstreamListener(tx_status.clone()),
            b,
            proxy_config.downstream_difficulty_config,
            diff_config,
        );
    }); // End of init task

    debug!("Starting up signal listener");
    let mut interrupt_signal_future = Box::pin(tokio::signal::ctrl_c().fuse());
    debug!("Starting up status listener");

    // Check all tasks if is_finished() is true, if so exit
    loop {
        let task_status = select! {
            task_status = rx_status.recv().fuse() => task_status,
            interrupt_signal = interrupt_signal_future.as_mut() => {
                match interrupt_signal {
                    Ok(()) => {
                        info!("Interrupt received");
                    },
                    Err(err) => {
                        error!("Unable to listen for interrupt signal: {}", err);
                        // we also shut down in case of error
                    },
                }
                break;
            }
        };
        let task_status: status::Status = task_status.unwrap();

        match task_status.state {
            // Should only be sent by the downstream listener
            status::State::DownstreamShutdown(err) => {
                error!("SHUTDOWN from: {}", err);
                break;
            }
            status::State::BridgeShutdown(err) => {
                error!("SHUTDOWN from: {}", err);
                break;
            }
            status::State::UpstreamShutdown(err) => {
                error!("SHUTDOWN from: {}", err);
                break;
            }
            status::State::Healthy(msg) => {
                info!("HEALTHY message: {}", msg);
            }
            status::State::TemplateProviderShutdown(err) => {
                error!("SHUTDOWN from: {}", err);
                break;
            }
            status::State::DownstreamInstanceDropped(downstream_id) => {
                error!("SHUTDOWN from: {}", downstream_id);
                break;
            }
        }
    }
}