use super::{downstream_sv2, error::{LotteryError, PoolResult}, status};
use async_channel::{Receiver, Sender};
use binary_sv2::U256;
use codec_sv2::{Frame, HandshakeRole, Responder, StandardEitherFrame, StandardSv2Frame};
use error_handling::handle_result;
use key_utils::{Secp256k1PublicKey, Secp256k1SecretKey};
use network_helpers::noise_connection_tokio::Connection;
use nohash_hasher::BuildNoHashHasher;
use roles_logic_sv2::{
    channel_logic::channel_factory::PoolChannelFactory,
    common_properties::{CommonDownstreamData, IsDownstream, IsMiningDownstream},
    errors::Error,
    handlers::mining::{ParseDownstreamMiningMessages, SendTo},
    job_creator::JobsCreators,
    mining_sv2::{ExtendedExtranonce, SetNewPrevHash as SetNPH},
    parsers::{Mining, PoolMessages},
    routing_logic::MiningRoutingLogic,
    template_distribution_sv2::{NewTemplate, SetNewPrevHash, SubmitSolution},
    utils::{CoinbaseOutput as CoinbaseOutput_, Mutex},
};
use serde::Deserialize;
use std::{
    collections::HashMap,
    convert::{TryFrom, TryInto},
    net::SocketAddr,
    sync::Arc,
};
use stratum_common::bitcoin::{Script, TxOut};
use tokio::{net::TcpListener, task};
use tracing::{debug, error, info, warn};

use downstream_sv2::DownstreamSv2;

pub mod setup_connection;
use setup_connection::SetupConnectionHandler;

pub mod message_handler;
pub type Message = PoolMessages<'static>;
pub type StdFrame = StandardSv2Frame<Message>;
pub type EitherFrame = StandardEitherFrame<Message>;

pub fn get_coinbase_output(config: &Configuration) -> Result<Vec<TxOut>, Error> {
    let mut result = Vec::new();
    for coinbase_output_lottery in &config.coinbase_outputs {
        let coinbase_output: CoinbaseOutput_ = coinbase_output_lottery.try_into()?;
        let output_script: Script = coinbase_output.try_into()?;
        result.push(TxOut {
            value: 0,
            script_pubkey: output_script,
        });
    }
    match result.is_empty() {
        true => Err(Error::EmptyCoinbaseOutputs),
        _ => Ok(result),
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct CoinbaseOutput {
    output_script_type: String,
    output_script_value: String,
}

impl TryFrom<&CoinbaseOutput> for CoinbaseOutput_ {
    type Error = Error;

    fn try_from(lottery_output: &CoinbaseOutput) -> Result<Self, Self::Error> {
        match lottery_output.output_script_type.as_str() {
            "TEST" | "P2PK" | "P2PKH" | "P2WPKH" | "P2SH" | "P2WSH" | "P2TR" => {
                Ok(CoinbaseOutput_ {
                    output_script_type: lottery_output.clone().output_script_type,
                    output_script_value: lottery_output.clone().output_script_value,
                })
            }
            _ => Err(Error::UnknownOutputScriptType),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct Configuration {
    pub listen_address: String,
    pub tp_address: String,
    pub tp_authority_public_key: Option<Secp256k1PublicKey>,
    pub authority_public_key: Secp256k1PublicKey,
    pub authority_secret_key: Secp256k1SecretKey,
    pub cert_validity_sec: u64,
    pub coinbase_outputs: Vec<CoinbaseOutput>,
    pub lottery_signature: String,
    #[cfg(feature = "test_only_allow_unencrypted")]
    pub test_only_listen_adress_plain: String,
}

/// Accept downstream connection
pub struct Lottery {
    pub downstreams_sv2: HashMap<u32, Arc<Mutex<DownstreamSv2>>, BuildNoHashHasher<u32>>,
    solution_sender: Sender<SubmitSolution<'static>>,
    new_template_processed: bool,
    channel_factory: Arc<Mutex<PoolChannelFactory>>,
    last_prev_hash_template_id: u64,
    status_tx: status::Sender<'static>,
}

// Verifies token for a custom job which is the signed tx_hash_list_hash by Job Declarator Server
//TODO: implement the use of this fuction in main.rs
#[allow(dead_code)]
pub fn verify_token(
    tx_hash_list_hash: U256,
    signature: secp256k1::schnorr::Signature,
    pub_key: key_utils::Secp256k1PublicKey,
) -> Result<(), secp256k1::Error> {
    let secp = secp256k1::Secp256k1::verification_only();
    // Create PublicKey instance
    let x_only_public_key = pub_key.0;

    let message: Vec<u8> = tx_hash_list_hash.to_vec();

    // Verify signature
    let is_verified = secp.verify_schnorr(
        &signature,
        &secp256k1::Message::from_digest_slice(&message)?,
        &x_only_public_key,
    );

    // debug
    debug!("Message: {}", std::str::from_utf8(&message).unwrap());
    debug!("Verified signature {:?}", is_verified);
    is_verified
}

impl IsDownstream for DownstreamSv2 {
    fn get_downstream_mining_data(&self) -> CommonDownstreamData {
        self.downstream_data
    }
}

impl IsMiningDownstream for DownstreamSv2 {}

impl Lottery {
    #[cfg(feature = "test_only_allow_unencrypted")]
    async fn accept_incoming_plain_connection(
        self_: Arc<Mutex<Lottery>>,
        config: Configuration,
    ) -> PoolResult<()> {
        let listner = TcpListener::bind(&config.test_only_listen_adress_plain)
            .await
            .unwrap();
        let status_tx = self_.safe_lock(|s| s.status_tx.clone())?;

        info!(
            "Listening for unencrypted connection on: {}",
            config.test_only_listen_adress_plain
        );
        while let Ok((stream, _)) = listner.accept().await {
            let address = stream.peer_addr().unwrap();
            debug!("New connection from {}", address);

            let (receiver, sender): (Receiver<EitherFrame>, Sender<EitherFrame>) =
                network_helpers::plain_connection_tokio::PlainConnection::new(stream).await;

            handle_result!(
                status_tx,
                Self::accept_incoming_connection_(self_.clone(), receiver, sender, address).await
            );
        }
        Ok(())
    }

    async fn accept_incoming_connection(
        self_: Arc<Mutex<Lottery>>,
        config: Configuration,
    ) -> PoolResult<'static, ()> {
        let status_tx = self_.safe_lock(|s| s.status_tx.clone())?;
        let listener = TcpListener::bind(&config.listen_address).await?;
        info!(
            "Listening for encrypted connection on: {}",
            config.listen_address
        );
        while let Ok((stream, _)) = listener.accept().await {
            let address = stream.peer_addr().unwrap();
            debug!(
                "New connection from {:?}",
                stream.peer_addr().map_err(LotteryError::Io)
            );

            let responder = Responder::from_authority_kp(
                &config.authority_public_key.into_bytes(),
                &config.authority_secret_key.into_bytes(),
                std::time::Duration::from_secs(config.cert_validity_sec),
            );
            match responder {
                Ok(resp) => {
                    if let Ok((receiver, sender, _, _)) =
                        Connection::new(stream, HandshakeRole::Responder(resp)).await
                    {
                        handle_result!(
                            status_tx,
                            Self::accept_incoming_connection_(
                                self_.clone(),
                                receiver,
                                sender,
                                address
                            )
                            .await
                        );
                    }
                }
                Err(_e) => {
                    todo!()
                }
            }
        }
        Ok(())
    }

    async fn accept_incoming_connection_(
        self_: Arc<Mutex<Lottery>>,
        receiver: Receiver<EitherFrame>,
        sender: Sender<EitherFrame>,
        address: SocketAddr,
    ) -> PoolResult<'static, ()> {
        let solution_sender = self_.safe_lock(|p| p.solution_sender.clone())?;
        let status_tx = self_.safe_lock(|s| s.status_tx.clone())?;
        let channel_factory = self_.safe_lock(|s| s.channel_factory.clone())?;

        let downstream = DownstreamSv2::new(
            receiver,
            sender,
            solution_sender,
            self_.clone(),
            channel_factory,
            // convert Listener variant to Downstream variant
            status_tx.listener_to_connection(),
            address,
        )
            .await?;

        let (_, channel_id) = downstream.safe_lock(|d| (d.downstream_data.header_only, d.id))?;

        self_.safe_lock(|p| {
            p.downstreams_sv2.insert(channel_id, downstream);
        })?;
        Ok(())
    }

    async fn on_new_prev_hash(
        self_: Arc<Mutex<Self>>,
        rx: Receiver<SetNewPrevHash<'static>>,
        sender_message_received_signal: Sender<()>,
    ) -> PoolResult<()> {
        let status_tx = self_
            .safe_lock(|s| s.status_tx.clone())
            .map_err(|e| LotteryError::PoisonLock(e.to_string()))?;
        while let Ok(new_prev_hash) = rx.recv().await {
            debug!("New prev hash received: {:?}", new_prev_hash);
            let res = self_
                .safe_lock(|s| {
                    s.last_prev_hash_template_id = new_prev_hash.template_id;
                })
                .map_err(|e| LotteryError::PoisonLock(e.to_string()));
            handle_result!(status_tx, res);

            let job_id_res = self_
                .safe_lock(|s| {
                    s.channel_factory
                        .safe_lock(|f| f.on_new_prev_hash_from_tp(&new_prev_hash))
                        .map_err(|e| LotteryError::PoisonLock(e.to_string()))
                })
                .map_err(|e| LotteryError::PoisonLock(e.to_string()));
            let job_id = handle_result!(status_tx, handle_result!(status_tx, job_id_res));

            match job_id {
                Ok(job_id) => {
                    let downstreams = self_
                        .safe_lock(|s| s.downstreams_sv2.clone())
                        .map_err(|e| LotteryError::PoisonLock(e.to_string()));
                    let downstreams = handle_result!(status_tx, downstreams);

                    for (channel_id, downtream) in downstreams {
                        let message = Mining::SetNewPrevHash(SetNPH {
                            channel_id,
                            job_id,
                            prev_hash: new_prev_hash.prev_hash.clone(),
                            min_ntime: new_prev_hash.header_timestamp,
                            nbits: new_prev_hash.n_bits,
                        });
                        let res = DownstreamSv2::match_send_to(
                            downtream.clone(),
                            Ok(SendTo::Respond(message)),
                        )
                            .await;
                        handle_result!(status_tx, res);
                    }
                    handle_result!(status_tx, sender_message_received_signal.send(()).await);
                }
                Err(_) => todo!(),
            }
        }
        Ok(())
    }

    async fn on_new_template(
        self_: Arc<Mutex<Self>>,
        rx: Receiver<NewTemplate<'static>>,
        sender_message_received_signal: Sender<()>,
    ) -> PoolResult<()> {
        let status_tx = self_.safe_lock(|s| s.status_tx.clone())?;
        let channel_factory = self_.safe_lock(|s| s.channel_factory.clone())?;
        while let Ok(mut new_template) = rx.recv().await {
            debug!(
                "New template received, creating a new mining job(s): {:?}",
                new_template
            );

            let messages = channel_factory
                .safe_lock(|cf| cf.on_new_template(&mut new_template))
                .map_err(|e| LotteryError::PoisonLock(e.to_string()));
            let messages = handle_result!(status_tx, messages);
            let mut messages = handle_result!(status_tx, messages);

            let downstreams = self_
                .safe_lock(|s| s.downstreams_sv2.clone())
                .map_err(|e| LotteryError::PoisonLock(e.to_string()));
            let downstreams = handle_result!(status_tx, downstreams);

            for (channel_id, downtream) in downstreams {
                if let Some(to_send) = messages.remove(&channel_id) {
                    if let Err(e) =
                        DownstreamSv2::match_send_to(downtream.clone(), Ok(SendTo::Respond(to_send)))
                            .await
                    {
                        error!("Unknown template provider message: {:?}", e);
                    }
                }
            }
            let res = self_
                .safe_lock(|s| s.new_template_processed = true)
                .map_err(|e| LotteryError::PoisonLock(e.to_string()));
            handle_result!(status_tx, res);

            handle_result!(status_tx, sender_message_received_signal.send(()).await);
        }
        Ok(())
    }

    pub fn start(
        config: Configuration,
        new_template_rx: Receiver<NewTemplate<'static>>,
        new_prev_hash_rx: Receiver<SetNewPrevHash<'static>>,
        solution_sender: Sender<SubmitSolution<'static>>,
        sender_message_received_signal: Sender<()>,
        status_tx: status::Sender<'static>,
    ) -> Arc<Mutex<Self>> {
        let extranonce_len = 32;
        let range_0 = std::ops::Range { start: 0, end: 0 };
        let range_1 = std::ops::Range { start: 0, end: 16 };
        let range_2 = std::ops::Range {
            start: 16,
            end: extranonce_len,
        };
        let ids = Arc::new(Mutex::new(roles_logic_sv2::utils::GroupId::new()));
        let lottery_coinbase_outputs = get_coinbase_output(&config);
        info!("PUB KEY: {:?}", lottery_coinbase_outputs);
        let extranonces = ExtendedExtranonce::new(range_0, range_1, range_2);
        let creator = JobsCreators::new(extranonce_len as u8);
        let share_per_min = 1.0;
        let kind = roles_logic_sv2::channel_logic::channel_factory::ExtendedChannelKind::Pool;
        let channel_factory = Arc::new(Mutex::new(PoolChannelFactory::new(
            ids,
            extranonces,
            creator,
            share_per_min,
            kind,
            lottery_coinbase_outputs.expect("Invalid coinbase output in config"),
            config.lottery_signature.clone(),
        )));
        let lottery = Arc::new(Mutex::new(Lottery {
            downstreams_sv2: HashMap::with_hasher(BuildNoHashHasher::default()),
            solution_sender,
            new_template_processed: false,
            channel_factory,
            last_prev_hash_template_id: 0,
            status_tx: status_tx.clone(),
        }));

        let cloned = lottery.clone();
        let cloned2 = lottery.clone();
        let cloned3 = lottery.clone();

        info!("Starting up lottery listener");
        let status_tx_clone = status_tx.clone();
        task::spawn(async move {
            if let Err(e) = Self::accept_incoming_connection(cloned, config).await {
                error!("{}", e);
            }
            if status_tx_clone
                .send(status::Status {
                    state: status::State::DownstreamShutdown(LotteryError::ComponentShutdown(
                        "Downstream no longer accepting incoming connections".to_string(),
                    )),
                })
                .await
                .is_err()
            {
                error!("Downstream shutdown and Status Channel dropped");
            }
        });

        let cloned = sender_message_received_signal.clone();
        let status_tx_clone = status_tx.clone();
        task::spawn(async move {
            if let Err(e) = Self::on_new_prev_hash(cloned2, new_prev_hash_rx, cloned).await {
                error!("{}", e);
            }
            // on_new_prev_hash shutdown
            if status_tx_clone
                .send(status::Status {
                    state: status::State::DownstreamShutdown(LotteryError::ComponentShutdown(
                        "Downstream no longer accepting new prevhash".to_string(),
                    )),
                })
                .await
                .is_err()
            {
                error!("Downstream shutdown and Status Channel dropped");
            }
        });

        let status_tx_clone = status_tx;
        task::spawn(async move {
            if let Err(e) =
                Self::on_new_template(lottery, new_template_rx, sender_message_received_signal).await
            {
                error!("{}", e);
            }
            // on_new_template shutdown
            if status_tx_clone
                .send(status::Status {
                    state: status::State::DownstreamShutdown(LotteryError::ComponentShutdown(
                        "Downstream no longer accepting templates".to_string(),
                    )),
                })
                .await
                .is_err()
            {
                error!("Downstream shutdown and Status Channel dropped");
            }
        });
        cloned3
    }

    /// This removes the downstream from the list of downstreams
    /// due to a race condition it's possible for downstreams to have been cloned right before
    /// this remove happens which will cause the cloning task to still attempt to communicate with the
    /// downstream. This is going to be rare and will won't cause any issues as the attempt to communicate
    /// will fail but continue with the next downstream.
    pub fn remove_downstream(&mut self, downstream_id: u32) {
        self.downstreams_sv2.remove(&downstream_id);
    }
}