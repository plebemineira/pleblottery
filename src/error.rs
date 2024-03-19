use std::{
    convert::From,
    fmt::Debug,
    sync::{MutexGuard, PoisonError},
};

use roles_logic_sv2::parsers::Mining;

use roles_logic_sv2::{
    mining_sv2::{ExtendedExtranonce, NewExtendedMiningJob, SetCustomMiningJob},
};
use std::fmt;
use sv1_api::server_to_client::{Notify, SetDifficulty};

use stratum_common::bitcoin::util::uint::ParseLengthError;

#[derive(std::fmt::Debug)]
pub enum LotteryError<'a> {
    ChannelSend(Box<dyn std::marker::Send + Debug>),
    ChannelRecv(async_channel::RecvError),
    BinarySv2(binary_sv2::Error),
    Codec(codec_sv2::Error),
    Noise(noise_sv2::Error),
    RolesLogic(roles_logic_sv2::Error),
    Framing(codec_sv2::framing_sv2::Error),
    ComponentShutdown(String),
    Custom(String),
    // Sv2ProtocolError((u32, Mining<'static>)),
    VecToSlice32(Vec<u8>),
    /// Errors on bad CLI argument input.
    BadCliArgs,
    /// Errors on bad `serde_json` serialize/deserialize.
    BadSerdeJson(serde_json::Error),
    /// Errors on bad `toml` deserialize.
    BadTomlDeserialize(toml::de::Error),
    /// Errors on bad noise handshake.
    CodecNoise(codec_sv2::noise_sv2::Error),
    /// Errors from `framing_sv2` crate.
    FramingSv2(framing_sv2::Error),
    /// Errors on bad `TcpStream` connection.
    Io(std::io::Error),
    /// Errors due to invalid extranonce from upstream
    InvalidExtranonce(String),
    /// Errors on bad `String` to `int` conversion.
    ParseInt(std::num::ParseIntError),
    /// Errors from `roles_logic_sv2` crate.
    RolesSv2Logic(roles_logic_sv2::errors::Error),
    UpstreamIncoming(roles_logic_sv2::errors::Error),
    /// SV1 protocol library error
    V1Protocol(sv1_api::error::Error<'a>),
    #[allow(dead_code)]
    SubprotocolMining(String),
    // Locking Errors
    PoisonLock(String),
    // Channel Receiver Error
    ChannelErrorReceiver(async_channel::RecvError),
    TokioChannelErrorRecv(tokio::sync::broadcast::error::RecvError),
    // Channel Sender Errors
    ChannelErrorSender(ChannelSendError<'a>),
    Uint256Conversion(ParseLengthError),
    SetDifficultyToMessage(SetDifficulty),
    Infallible(std::convert::Infallible),
    // used to handle SV2 protocol error messages from pool
    // #[allow(clippy::enum_variant_names)]
    Sv2ProtocolError(Mining<'a>),
    #[allow(clippy::enum_variant_names)]
    TargetError(roles_logic_sv2::errors::Error),
    Sv1MessageTooLong,
}

impl std::fmt::Display for LotteryError<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use LotteryError::*;
        match self {
            Io(ref e) => write!(f, "I/O error: `{:?}", e),
            ChannelSend(ref e) => write!(f, "Channel send failed: `{:?}`", e),
            ChannelRecv(ref e) => write!(f, "Channel recv failed: `{:?}`", e),
            BinarySv2(ref e) => write!(f, "Binary SV2 error: `{:?}`", e),
            Codec(ref e) => write!(f, "Codec SV2 error: `{:?}", e),
            Framing(ref e) => write!(f, "Framing SV2 error: `{:?}`", e),
            Noise(ref e) => write!(f, "Noise SV2 error: `{:?}", e),
            RolesLogic(ref e) => write!(f, "Roles Logic SV2 error: `{:?}`", e),
            PoisonLock(ref e) => write!(f, "Poison lock: {:?}", e),
            ComponentShutdown(ref e) => write!(f, "Component shutdown: {:?}", e),
            Custom(ref e) => write!(f, "Custom SV2 error: `{:?}`", e),
            Sv2ProtocolError(ref e) => {
                write!(f, "Received Sv2 Protocol Error from upstream: `{:?}`", e)
            }
            BadCliArgs => write!(f, "Bad CLI arg input"),
            BadSerdeJson(ref e) => write!(f, "Bad serde json: `{:?}`", e),
            BadTomlDeserialize(ref e) => write!(f, "Bad `toml` deserialize: `{:?}`", e),
            crate::error::LotteryError::BinarySv2(ref e) => write!(f, "Binary SV2 error: `{:?}`", e),
            CodecNoise(ref e) => write!(f, "Noise error: `{:?}", e),
            FramingSv2(ref e) => write!(f, "Framing SV2 error: `{:?}`", e),
            InvalidExtranonce(ref e) => write!(f, "Invalid Extranonce error: `{:?}", e),
            crate::error::LotteryError::Io(ref e) => write!(f, "I/O error: `{:?}", e),
            ParseInt(ref e) => write!(f, "Bad convert from `String` to `int`: `{:?}`", e),
            RolesSv2Logic(ref e) => write!(f, "Roles SV2 Logic Error: `{:?}`", e),
            V1Protocol(ref e) => write!(f, "V1 Protocol Error: `{:?}`", e),
            SubprotocolMining(ref e) => write!(f, "Subprotocol Mining Error: `{:?}`", e),
            UpstreamIncoming(ref e) => write!(f, "Upstream parse incoming error: `{:?}`", e),
            PoisonLock(ref e) => write!(f, "Poison Lock error"),
            ChannelErrorReceiver(ref e) => write!(f, "Channel receive error: `{:?}`", e),
            TokioChannelErrorRecv(ref e) => write!(f, "Channel receive error: `{:?}`", e),
            ChannelErrorSender(ref e) => write!(f, "Channel send error: `{:?}`", e),
            Uint256Conversion(ref e) => write!(f, "U256 Conversion Error: `{:?}`", e),
            SetDifficultyToMessage(ref e) => {
                write!(f, "Error converting SetDifficulty to Message: `{:?}`", e)
            }
            VecToSlice32(ref e) => write!(f, "Standard Error: `{:?}`", e),
            Infallible(ref e) => write!(f, "Infallible Error:`{:?}`", e),
            crate::error::LotteryError::Sv2ProtocolError(ref e) => {
                write!(f, "Received Sv2 Protocol Error from upstream: `{:?}`", e)
            }
            TargetError(ref e) => {
                write!(f, "Impossible to get target from hashrate: `{:?}`", e)
            }
            Sv1MessageTooLong => {
                write!(f, "Received an sv1 message that is longer than max len")
            }
        }
    }
}

pub type PoolResult<'a, T> = Result<T, LotteryError<'a>>;

impl<'a> From<std::io::Error> for LotteryError<'a> {
    fn from(e: std::io::Error) -> LotteryError<'a> {
        LotteryError::Io(e)
    }
}

impl<'a> From<async_channel::RecvError> for LotteryError<'a> {
    fn from(e: async_channel::RecvError) -> LotteryError<'a> {
        LotteryError::ChannelRecv(e)
    }
}

impl<'a> From<tokio::sync::broadcast::error::RecvError> for LotteryError<'a> {
    fn from(e: tokio::sync::broadcast::error::RecvError) -> Self {
        LotteryError::TokioChannelErrorRecv(e)
    }
}

impl<'a> From<tokio::sync::broadcast::error::SendError<Notify<'a>>> for LotteryError<'a> {
    fn from(e: tokio::sync::broadcast::error::SendError<Notify<'a>>) -> Self {
        LotteryError::ChannelErrorSender(ChannelSendError::Notify(e))
    }
}

impl<'a> From<binary_sv2::Error> for LotteryError<'a> {
    fn from(e: binary_sv2::Error) -> LotteryError<'a> {
        LotteryError::BinarySv2(e)
    }
}

impl<'a> From<codec_sv2::Error> for LotteryError<'a> {
    fn from(e: codec_sv2::Error) -> LotteryError<'a> {
        LotteryError::Codec(e)
    }
}

impl<'a> From<noise_sv2::Error> for LotteryError<'a> {
    fn from(e: noise_sv2::Error) -> LotteryError<'a> {
        LotteryError::Noise(e)
    }
}

impl<'a> From<roles_logic_sv2::Error> for LotteryError<'a> {
    fn from(e: roles_logic_sv2::Error) -> LotteryError<'a> {
        LotteryError::RolesLogic(e)
    }
}

impl<'a> From<sv1_api::error::Error<'a>> for LotteryError<'a> {
    fn from(e: sv1_api::error::Error<'a>) -> Self {
        LotteryError::V1Protocol(e)
    }
}

impl<'a, T: 'static + std::marker::Send + Debug> From<async_channel::SendError<T>> for LotteryError<'a> {
    fn from(e: async_channel::SendError<T>) -> LotteryError<'a> {
        LotteryError::ChannelSend(Box::new(e))
    }
}

impl<'a> From<String> for LotteryError<'a> {
    fn from(e: String) -> LotteryError<'a> {
        LotteryError::Custom(e)
    }
}

impl<'a> From<Vec<u8>> for LotteryError<'a> {
    fn from(e: Vec<u8>) -> Self {
        LotteryError::VecToSlice32(e)
    }
}
impl<'a> From<codec_sv2::framing_sv2::Error> for LotteryError<'a> {
    fn from(e: codec_sv2::framing_sv2::Error) -> LotteryError<'a> {
        LotteryError::Framing(e)
    }
}

impl<'a, T> From<PoisonError<MutexGuard<'_, T>>> for LotteryError<'a> {
    fn from(e: PoisonError<MutexGuard<T>>) -> LotteryError<'a> {
        LotteryError::PoisonLock(e.to_string())
    }
}

impl<'a> From<Mining<'a>> for LotteryError<'a> {
    fn from(e: Mining<'a>) -> Self {
        LotteryError::Sv2ProtocolError(e)
    }
}

impl<'a> From<serde_json::Error> for LotteryError<'a> {
    fn from(e: serde_json::Error) -> Self {
        LotteryError::BadSerdeJson(e)
    }
}

impl<'a> From<ParseLengthError> for LotteryError<'a> {
    fn from(e: ParseLengthError) -> Self {
        LotteryError::Uint256Conversion(e)
    }
}

impl<'a> From<std::num::ParseIntError> for LotteryError<'a> {
    fn from(e: std::num::ParseIntError) -> Self {
        LotteryError::ParseInt(e)
    }
}
pub type ProxyResult<'a, T> = core::result::Result<T, LotteryError<'a>>;

#[derive(Debug)]
pub enum ChannelSendError<'a> {
    SubmitSharesExtended(
        async_channel::SendError<roles_logic_sv2::mining_sv2::SubmitSharesExtended<'a>>,
    ),
    SetNewPrevHash(async_channel::SendError<roles_logic_sv2::mining_sv2::SetNewPrevHash<'a>>),
    NewExtendedMiningJob(async_channel::SendError<NewExtendedMiningJob<'a>>),
    Notify(tokio::sync::broadcast::error::SendError<Notify<'a>>),
    V1Message(async_channel::SendError<sv1_api::Message>),
    General(String),
    Extranonce(async_channel::SendError<(ExtendedExtranonce, u32)>),
    SetCustomMiningJob(
        async_channel::SendError<roles_logic_sv2::mining_sv2::SetCustomMiningJob<'a>>,
    ),
    NewTemplate(
        async_channel::SendError<(
            roles_logic_sv2::template_distribution_sv2::SetNewPrevHash<'a>,
            Vec<u8>,
        )>,
    ),
}