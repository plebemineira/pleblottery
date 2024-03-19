use roles_logic_sv2::parsers::Mining;

use super::error::LotteryError;

/// Each sending side of the status channel
/// should be wrapped with this enum to allow
/// the main thread to know which component sent the message
#[derive(Debug)]
pub enum Sender<'a> {
    Downstream(async_channel::Sender<Status<'a>>),
    DownstreamListener(async_channel::Sender<Status<'a>>),
    Upstream(async_channel::Sender<Status<'a>>),
    Bridge(async_channel::Sender<Status<'a>>),
    TemplateReceiver(async_channel::Sender<Status<'a>>),
}

impl Sender<'_> {
    /// used to clone the sending side of the status channel used by the TCP Listener
    /// into individual Sender's for each Downstream instance
    pub fn listener_to_connection(&self) -> Self {
        match self {
            // should only be used to clone the DownstreamListener(Sender) into Downstream(Sender)s
            Self::DownstreamListener(inner) => Self::Downstream(inner.clone()),
            _ => unreachable!(),
        }
    }

    pub async fn send(&self, status: Status<'static>) -> Result<(), async_channel::SendError<Status>> {
        match self {
            Self::Downstream(inner) => inner.send(status).await,
            Self::DownstreamListener(inner) => inner.send(status).await,
            Self::Bridge(inner) => inner.send(status).await,
            Self::Upstream(inner) => inner.send(status).await,
            Self::TemplateReceiver(inner) => inner.send(status).await,
        }
    }
}

impl Clone for Sender<'_> {
    fn clone(&self) -> Self {
        match self {
            Self::Downstream(inner) => Self::Downstream(inner.clone()),
            Self::DownstreamListener(inner) => Self::DownstreamListener(inner.clone()),
            Self::Bridge(inner) => Self::Bridge(inner.clone()),
            Self::Upstream(inner) => Self::Upstream(inner.clone()),
            Self::TemplateReceiver(inner) => Self::TemplateReceiver(inner.clone()),
        }
    }
}

#[derive(Debug)]
pub enum State<'a> {
    DownstreamShutdown(LotteryError<'a>),
    TemplateProviderShutdown(LotteryError<'a>),
    DownstreamInstanceDropped(u32),
    BridgeShutdown(LotteryError<'a>),
    UpstreamShutdown(LotteryError<'a>),
    Healthy(String),
}

/// message to be sent to the status loop on the main thread
#[derive(Debug)]
pub struct Status<'a> {
    pub state: State<'a>,
}

/// this function is used to discern which componnent experienced the event.
/// With this knowledge we can wrap the status message with information (`State` variants) so
/// the main status loop can decide what should happen
async fn send_status(
    sender: &Sender<'static>,
    e: LotteryError<'static>,
    outcome: error_handling::ErrorBranch,
) -> error_handling::ErrorBranch {
    match sender {
        Sender::Downstream(tx) => {
            tx.send(Status {
                state: State::Healthy(e.to_string()),
            })
                .await
                .unwrap_or(());
        }
        Sender::DownstreamListener(tx) => match e {
            LotteryError::RolesLogic(roles_logic_sv2::Error::NoDownstreamsConnected) => {
                tx.send(Status {
                    state: State::Healthy("No Downstreams Connected".to_string()),
                })
                    .await
                    .unwrap_or(());
            }
            _ => {
                tx.send(Status {
                    state: State::DownstreamShutdown(e),
                })
                    .await
                    .unwrap_or(());
            }
        },
        Sender::Upstream(tx) => {
            tx.send(Status {
                state: State::TemplateProviderShutdown(e),
            })
                .await
                .unwrap_or(());
        },
        Sender::Bridge(tx) => {
            tx.send(Status {
                state: State::BridgeShutdown(e),
            })
                .await
                .unwrap_or(());
        },
        Sender::TemplateReceiver(tx) => {
            tx.send(Status {
                state: State::UpstreamShutdown(e),
            })
                .await
                .unwrap_or(());
        }
    }
    outcome
}

// this is called by `error_handling::handle_result!`
// todo: as described in issue #777, we should replace every generic *(_) with specific errors and cover every possible combination
pub async fn handle_error(sender: &Sender<'static>, e: LotteryError<'static>) -> error_handling::ErrorBranch {
    tracing::debug!("Error: {:?}", &e);
    match e {
        LotteryError::Io(_) => send_status(sender, e, error_handling::ErrorBranch::Break).await,
        LotteryError::ChannelSend(_) => {
            //This should be a continue because if we fail to send to 1 downstream we should continue
            //processing the other downstreams in the loop we are in. Otherwise if a downstream fails
            //to send to then subsequent downstreams in the map won't get send called on them
            send_status(sender, e, error_handling::ErrorBranch::Continue).await
        }
        LotteryError::ChannelRecv(_) => {
            send_status(sender, e, error_handling::ErrorBranch::Break).await
        }
        LotteryError::BinarySv2(_) => send_status(sender, e, error_handling::ErrorBranch::Break).await,
        LotteryError::Codec(_) => send_status(sender, e, error_handling::ErrorBranch::Break).await,
        LotteryError::Noise(_) => send_status(sender, e, error_handling::ErrorBranch::Continue).await,
        LotteryError::RolesLogic(roles_logic_sv2::Error::NoDownstreamsConnected) => {
            send_status(sender, e, error_handling::ErrorBranch::Continue).await
        }
        LotteryError::RolesLogic(_) => {
            send_status(sender, e, error_handling::ErrorBranch::Break).await
        }
        LotteryError::Custom(_) => send_status(sender, e, error_handling::ErrorBranch::Break).await,
        LotteryError::Framing(_) => send_status(sender, e, error_handling::ErrorBranch::Break).await,
        LotteryError::PoisonLock(_) => {
            send_status(sender, e, error_handling::ErrorBranch::Break).await
        }
        LotteryError::ComponentShutdown(_) => {
            send_status(sender, e, error_handling::ErrorBranch::Break).await
        }
        LotteryError::VecToSlice32(_) => send_status(sender, e, error_handling::ErrorBranch::Break).await,
        // Errors on bad CLI argument input.
        LotteryError::BadCliArgs => send_status(sender, e, error_handling::ErrorBranch::Break).await,
        // Errors on bad `serde_json` serialize/deserialize.
        LotteryError::BadSerdeJson(_) => send_status(sender, e, error_handling::ErrorBranch::Break).await,
        // Errors on bad `toml` deserialize.
        LotteryError::BadTomlDeserialize(_) => {
            send_status(sender, e, error_handling::ErrorBranch::Break).await
        }
        // Errors from `binary_sv2` crate.
        LotteryError::BinarySv2(_) => send_status(sender, e, error_handling::ErrorBranch::Break).await,
        // Errors on bad noise handshake.
        LotteryError::CodecNoise(_) => send_status(sender, e, error_handling::ErrorBranch::Break).await,
        // Errors from `framing_sv2` crate.
        LotteryError::FramingSv2(_) => send_status(sender, e, error_handling::ErrorBranch::Break).await,
        //If the pool sends the tproxy an invalid extranonce
        LotteryError::InvalidExtranonce(_) => {
            send_status(sender, e, error_handling::ErrorBranch::Break).await
        }
        // Errors on bad `TcpStream` connection.
        LotteryError::Io(_) => send_status(sender, e, error_handling::ErrorBranch::Break).await,
        // Errors on bad `String` to `int` conversion.
        LotteryError::ParseInt(_) => send_status(sender, e, error_handling::ErrorBranch::Break).await,
        // Errors from `roles_logic_sv2` crate.
        LotteryError::RolesSv2Logic(_) => send_status(sender, e, error_handling::ErrorBranch::Break).await,
        LotteryError::UpstreamIncoming(_) => {
            send_status(sender, e, error_handling::ErrorBranch::Break).await
        }
        // SV1 protocol library error
        LotteryError::V1Protocol(_) => send_status(sender, e, error_handling::ErrorBranch::Break).await,
        LotteryError::SubprotocolMining(_) => {
            send_status(sender, e, error_handling::ErrorBranch::Break).await
        }
        // Channel Receiver Error
        LotteryError::ChannelErrorReceiver(_) => {
            send_status(sender, e, error_handling::ErrorBranch::Break).await
        }
        LotteryError::TokioChannelErrorRecv(_) => {
            send_status(sender, e, error_handling::ErrorBranch::Break).await
        }
        // Channel Sender Errors
        LotteryError::ChannelErrorSender(_) => {
            send_status(sender, e, error_handling::ErrorBranch::Break).await
        }
        LotteryError::Uint256Conversion(_) => {
            send_status(sender, e, error_handling::ErrorBranch::Break).await
        }
        LotteryError::SetDifficultyToMessage(_) => {
            send_status(sender, e, error_handling::ErrorBranch::Break).await
        }
        LotteryError::Infallible(_) => send_status(sender, e, error_handling::ErrorBranch::Break).await,
        LotteryError::Sv2ProtocolError(ref inner) => {
            match inner {
                // dont notify main thread just continue
                roles_logic_sv2::parsers::Mining::SubmitSharesError(_) => {
                    error_handling::ErrorBranch::Continue
                }
                _ => send_status(sender, e, error_handling::ErrorBranch::Break).await,
            }
        }
        LotteryError::TargetError(_) => {
            send_status(sender, e, error_handling::ErrorBranch::Continue).await
        }
        LotteryError::Sv1MessageTooLong => {
            send_status(sender, e, error_handling::ErrorBranch::Break).await
        }
    }
}
