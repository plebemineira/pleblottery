use std::net::SocketAddr;
use std::sync::Arc;
use async_channel::{Receiver, Sender};
use error_handling::handle_result;
use codec_sv2::Frame;
use roles_logic_sv2::channel_logic::channel_factory::PoolChannelFactory;
use roles_logic_sv2::common_properties::CommonDownstreamData;
use roles_logic_sv2::Error;
use roles_logic_sv2::handlers::mining::{ParseDownstreamMiningMessages, SendTo};
use roles_logic_sv2::parsers::{Mining, PoolMessages};
use roles_logic_sv2::routing_logic::MiningRoutingLogic;
use roles_logic_sv2::template_distribution_sv2::SubmitSolution;
use roles_logic_sv2::utils::Mutex;
use tokio::task;
use tracing::{debug, error, warn};
use crate::error::{LotteryError, PoolResult};
use crate::lottery::{EitherFrame, Lottery, StdFrame};
use crate::lottery::setup_connection::SetupConnectionHandler;
use crate::status;

#[derive(Debug)]
pub struct DownstreamSv2 {
    // Either group or channel id
    pub id: u32,
    receiver: Receiver<EitherFrame>,
    sender: Sender<EitherFrame>,
    pub downstream_data: CommonDownstreamData,
    pub solution_sender: Sender<SubmitSolution<'static>>,
    pub channel_factory: Arc<Mutex<PoolChannelFactory>>,
}


impl DownstreamSv2 {
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        mut receiver: Receiver<EitherFrame>,
        mut sender: Sender<EitherFrame>,
        solution_sender: Sender<SubmitSolution<'static>>,
        lottery: Arc<Mutex<Lottery>>,
        channel_factory: Arc<Mutex<PoolChannelFactory>>,
        status_tx: status::Sender<'static>,
        address: SocketAddr,
    ) -> PoolResult<'static, Arc<Mutex<Self>>> {
        let setup_connection = Arc::new(Mutex::new(SetupConnectionHandler::new()));
        let downstream_data =
            SetupConnectionHandler::setup(setup_connection, &mut receiver, &mut sender, address)
                .await?;

        let id = match downstream_data.header_only {
            false => channel_factory.safe_lock(|c| c.new_group_id())?,
            true => channel_factory.safe_lock(|c| c.new_standard_id_for_hom())?,
        };

        let self_ = Arc::new(Mutex::new(DownstreamSv2 {
            id,
            receiver,
            sender,
            downstream_data,
            solution_sender,
            channel_factory,
        }));

        let cloned = self_.clone();

        task::spawn(async move {
            debug!("Starting up downstream receiver");
            let receiver_res = cloned
                .safe_lock(|d| d.receiver.clone())
                .map_err(|e| LotteryError::PoisonLock(e.to_string()));
            let receiver = match receiver_res {
                Ok(recv) => recv,
                Err(e) => {
                    if let Err(e) = status_tx
                        .send(status::Status {
                            state: status::State::Healthy(format!(
                                "Downstream connection dropped: {}",
                                e
                            )),
                        })
                        .await
                    {
                        error!("Encountered Error but status channel is down: {}", e);
                    }

                    return;
                }
            };
            loop {
                match receiver.recv().await {
                    Ok(received) => {
                        let received: Result<StdFrame, _> = received
                            .try_into()
                            .map_err(|e| LotteryError::Codec(codec_sv2::Error::FramingSv2Error(e)));
                        let std_frame = handle_result!(status_tx, received);
                        handle_result!(
                            status_tx,
                            DownstreamSv2::next(cloned.clone(), std_frame).await
                        );
                    }
                    _ => {
                        let res = lottery
                            .safe_lock(|p| p.downstreams_sv2.remove(&id))
                            .map_err(|e| LotteryError::PoisonLock(e.to_string()));
                        handle_result!(status_tx, res);
                        error!("Downstream {} disconnected", id);
                        break;
                    }
                }
            }
            warn!("Downstream connection dropped");
        });
        Ok(self_)
    }

    pub async fn next(self_mutex: Arc<Mutex<Self>>, mut incoming: StdFrame) -> PoolResult<'static, ()> {
        let message_type = incoming
            .get_header()
            .ok_or_else(|| LotteryError::Custom(String::from("No header set")))?
            .msg_type();
        let payload = incoming.payload();
        debug!(
            "Received downstream message type: {:?}, payload: {:?}",
            message_type, payload
        );
        let next_message_to_send = ParseDownstreamMiningMessages::handle_message_mining(
            self_mutex.clone(),
            message_type,
            payload,
            MiningRoutingLogic::None,
        );
        Self::match_send_to(self_mutex, next_message_to_send).await
    }

    #[async_recursion::async_recursion]
    pub async fn match_send_to(
        self_: Arc<Mutex<Self>>,
        send_to: Result<SendTo<()>, Error>,
    ) -> PoolResult<'static, ()> {
        match send_to {
            Ok(SendTo::Respond(message)) => {
                debug!("Sending to downstream: {:?}", message);
                // returning an error will send the error to the main thread,
                // and the main thread will drop the downstream from the lottery
                if let &Mining::OpenMiningChannelError(_) = &message {
                    Self::send(self_.clone(), message.clone()).await?;
                    let downstream_id = self_
                        .safe_lock(|d| d.id)
                        .map_err(|e| Error::PoisonLock(e.to_string()))?;
                    return Err(LotteryError::Sv2ProtocolError(
                        // (
                        // downstream_id,
                        message.clone(),
                        // )
                    ));
                } else {
                    Self::send(self_, message.clone()).await?;
                }
            }
            Ok(SendTo::Multiple(messages)) => {
                debug!("Sending multiple messages to downstream");
                for message in messages {
                    debug!("Sending downstream message: {:?}", message);
                    Self::match_send_to(self_.clone(), Ok(message)).await?;
                }
            }
            Ok(SendTo::None(_)) => {}
            Ok(m) => {
                error!("Unexpected SendTo: {:?}", m);
                panic!();
            }
            Err(Error::UnexpectedMessage(_message_type)) => todo!(),
            Err(e) => {
                error!("Error: {:?}", e);
                todo!()
            }
        }
        Ok(())
    }

    async fn send(
        self_mutex: Arc<Mutex<Self>>,
        message: roles_logic_sv2::parsers::Mining<'static>,
    ) -> PoolResult<()> {
        //let message = if let Mining::NewExtendedMiningJob(job) = message {
        //    Mining::NewExtendedMiningJob(extended_job_to_non_segwit(job, 32)?)
        //} else {
        //    message
        //};
        let sv2_frame: StdFrame = PoolMessages::Mining(message).try_into()?;
        let sender = self_mutex.safe_lock(|self_| self_.sender.clone())?;
        sender.send(sv2_frame.into()).await?;
        Ok(())
    }
}