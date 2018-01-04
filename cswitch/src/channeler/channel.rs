use std::{io, mem, time};
use std::cell::RefCell;
use std::net::SocketAddr;
use std::collections::HashMap;

use futures::prelude::*;
use futures::sync::mpsc;
use futures::stream::{SplitSink, SplitStream};

use futures_mutex::FutMutex;

use tokio_core::net::TcpStream;
use tokio_core::reactor::{Handle, Timeout};

use tokio_io::AsyncRead;
use tokio_io::codec::Framed;

use bytes::Bytes;
use ring::rand::SystemRandom;

use crypto::uid::gen_uid;
use crypto::identity::PublicKey;
use crypto::rand_values::RandValue;
use crypto::symmetric_enc::{
    SymmetricKey,
    Encryptor,
    Decryptor,
    EncNonceCounter,
    SymmetricEncError,
};
use crypto::dh::{DhPrivateKey, DhPublicKey, Salt};
use inner_messages::{
    ChannelerToNetworker,
    ChannelOpened,
    ChannelMessageReceived,
};
use security_module::security_module_client::{
    SecurityModuleClient,
    SecurityModuleClientError,
};

use schema::SchemaError;
// use schema::channeler_capnp::MessageType;
use schema::channeler::{
    serialize_message,
    deserialize_message,
    serialize_enc_message,
    deserialize_enc_message,
    serialize_exchange_message,
    deserialize_exchange_message,
    serialize_init_channel_message,
    deserialize_init_channel_message,
};

use super::{ToChannel, ChannelerNeighbor, KEEP_ALIVE_TICKS};
use super::codec::{PrefixFrameCodec, PrefixFrameCodecError};

#[derive(Debug)]
pub enum ChannelError {
    Io(io::Error),
    Timeout,
    FutMutex,
    Schema(SchemaError),
    Capnp(::capnp::Error),
    Codec(PrefixFrameCodecError),
    SecurityModule(SecurityModuleClientError),
    EncryptError(SymmetricEncError),
    SendToNetworkerFailed,
    SendToRemoteFailed,
    RecvFromInnerFailed,
    KeepAliveTimeout,
    Closed(&'static str),
}

impl From<io::Error> for ChannelError {
    #[inline]
    fn from(e: io::Error) -> ChannelError {
        ChannelError::Io(e)
    }
}

impl From<::capnp::Error> for ChannelError {
    #[inline]
    fn from(e: ::capnp::Error) -> ChannelError {
        ChannelError::Capnp(e)
    }
}

impl From<PrefixFrameCodecError> for ChannelError {
    #[inline]
    fn from(e: PrefixFrameCodecError) -> ChannelError {
        ChannelError::Codec(e)
    }
}

impl From<SecurityModuleClientError> for ChannelError {
    #[inline]
    fn from(e: SecurityModuleClientError) -> ChannelError {
        ChannelError::SecurityModule(e)
    }
}

impl From<SchemaError> for ChannelError {
    #[inline]
    fn from(e: SchemaError) -> ChannelError {
        ChannelError::Schema(e)
    }
}

impl From<SymmetricEncError> for ChannelError {
    #[inline]
    fn from(e: SymmetricEncError) -> ChannelError {
        ChannelError::EncryptError(e)
    }
}

/// The channel used to communicate to neighbors.
#[must_use = "futures do nothing unless polled"]
pub struct Channel {
    remote_public_key: PublicKey,

    // The inner sender and receiver used to communicate with internal services
    inner_sender:   mpsc::Sender<ChannelerToNetworker>,
    inner_receiver: mpsc::Receiver<ToChannel>,
    inner_buffered: Option<ChannelerToNetworker>,

    // The outer sender and receiver used to communicate with neighbors
    outer_sender:   SplitSink<Framed<TcpStream, PrefixFrameCodec>>,
    outer_receiver: SplitStream<Framed<TcpStream, PrefixFrameCodec>>,
    outer_buffered: Option<Bytes>,

    send_counter: u64,
    recv_counter: u64,
    encryptor: Encryptor,
    decryptor: Decryptor,

    remaining_tick_to_send_ka: usize,
    remaining_tick_to_recv_ka: usize,
}

impl Channel {
    /// Create a new channel connected to the specified neighbor.
    pub fn connect(
        addr:                &SocketAddr,
        handle:              &Handle,
        neighbor_public_key: &PublicKey,
        neighbors:           &FutMutex<HashMap<PublicKey, ChannelerNeighbor>>,
        networker_sender:    &mpsc::Sender<ChannelerToNetworker>,
        sm_client:           &SecurityModuleClient
    ) -> ChannelNew {
        let prepare_tcp_fut = TcpStream::connect(addr, handle).map_err(|e| e.into());

        ChannelNew {
            state: RefCell::new(ChannelNewState::PrepareTcp(Box::new(prepare_tcp_fut))),
            role:             Role::Initiator,
            timeout:          Timeout::new(time::Duration::from_secs(5), handle).unwrap(),
            rng:              SystemRandom::new(),
            sm_client:        sm_client.clone(),
            neighbors:        neighbors.clone(),
            networker_sender: networker_sender.clone(),

            neighbor_public_key: Some(neighbor_public_key.clone()),
            sent_rand_value:     None,
            recv_rand_value:     None,
            dh_private_key:      None,
            dh_public_key:       None,
            dh_key_salt:         None,
            sender:              None,
            receiver:            None,
        }
    }

    /// Create a new channel from a incoming socket.
    pub fn from_socket(
        socket:           TcpStream,
        handle:           &Handle,
        neighbors:        &FutMutex<HashMap<PublicKey, ChannelerNeighbor>>,
        networker_sender: &mpsc::Sender<ChannelerToNetworker>,
        sm_client:        &SecurityModuleClient
    ) -> ChannelNew {
        let (tx, rx) = socket.framed(PrefixFrameCodec::new()).split();

        let rng             = SystemRandom::new();
        let rand_value      = RandValue::new(&rng);
        let sent_rand_value = rand_value.clone();

        let prepare_init_fut = sm_client.request_public_key()
            .map_err(|e| e.into())
            .and_then(move |public_key| {
                serialize_init_channel_message(rand_value, public_key)
                    .map_err(|e| e.into())
            });

        ChannelNew {
            state: RefCell::new(ChannelNewState::PrepareInit(Box::new(prepare_init_fut))),
            role:             Role::Responder,
            timeout:          Timeout::new(time::Duration::from_secs(5), handle).unwrap(),
            rng,
            sm_client:        sm_client.clone(),
            neighbors:        neighbors.clone(),
            networker_sender: networker_sender.clone(),

            neighbor_public_key: None,
            sent_rand_value:     Some(sent_rand_value),
            recv_rand_value:     None,
            dh_private_key:      None,
            dh_public_key:       None,
            dh_key_salt:         None,
            sender:              Some(RefCell::new(tx)),
            receiver:            Some(RefCell::new(rx)),
        }
    }

    // Pack and encrypt a message to be sent to remote.
    //
    // **If the `content` is None, a packed KA message will be returned.**
    fn pack_msg(&mut self, content: Option<Bytes>) -> Result<Bytes, ChannelError> {
        // TODO:
        // 1. Change the return type of Decryptor::encrypt
        // 2. Use explicit message type (required: refactor schema::channeler)
        let plain_msg = serialize_enc_message(self.send_counter, content)?;
        let encrypted = self.encryptor.encrypt(&plain_msg)?;

        serialize_message(Bytes::from(encrypted)).map_err(|e| e.into())
    }

    // Decrypt and unpack a message received from remote.
    //
    // **If the message is a `KeepAlive`, the content will be ignored.**
    fn unpack_msg(&mut self, raw: Bytes) -> Result<Option<Bytes>, ChannelError> {
        // TODO:
        // 1. Change the return type of Decryptor::decrypt
        // 2. Use explicit message type (required: refactor schema::channeler)

        let plain_msg = Bytes::from(self.decryptor.decrypt(&deserialize_message(raw)?)?);
        let (counter, _ty, content) = deserialize_enc_message(plain_msg)?;

        if counter != self.recv_counter {
            Err(ChannelError::Closed("unexpected counter"))
        } else {
            update_counter(&mut self.recv_counter);
            Ok(content)
        }
    }

    /// Try to start the process of sending a message to the networker.
    ///
    /// # Return Value
    ///
    /// - Returns `Async::Ready(())` if the `inner_sender` was able to start
    /// sending `msg` to `Networker`
    ///
    /// - Returns `ASync::NotReady` if the `inner_sender` was unable to
    /// begin sending `msg` to networker
    ///
    /// # Error
    ///
    /// This method returns an error `Err(ChannelError::SendToNetworkerFailed)`
    /// when the `inner_sender` encounters an error in starting sending
    ///
    /// # Panic
    ///
    /// This method may panic in a few situations:
    ///
    /// - The `inner_buffered` not empty
    ///
    /// Refer `futures`'s documentation for more information.
    fn try_start_send_inner(&mut self, msg: ChannelerToNetworker) -> Poll<(), ChannelError> {
        debug_assert!(self.inner_buffered.is_none());

        // Convert Err(()) to ChannelError
        let start_send = self.inner_sender.start_send(msg)
            .map_err(|_| ChannelError::SendToNetworkerFailed);

        if let AsyncSink::NotReady(msg) = start_send? {
            self.inner_buffered = Some(msg);
            Ok(Async::NotReady)
        } else {
            Ok(Async::Ready(()))
        }
    }

    /// Try to start the process of sending a message to the `Remote`.
    ///
    /// # Return Value
    ///
    /// - Returns `Async::Ready(())` if the `outer_sender` was able to start
    /// sending `msg` to `Remote`
    ///
    /// - Returns `ASync::NotReady` if the `outer_sender` was unable to
    /// begin sending `msg` to `Remote`
    ///
    /// # Error
    ///
    /// This method returns an error `Err(ChannelError::Io(...))`
    /// when the `outer_sender` encounters an error in starting sending
    ///
    /// # Panic
    ///
    /// This method may panic in a few situations:
    ///
    /// - The `outer_buffered` not empty
    ///
    /// Refer `futures`'s documentation for more information.
    fn try_start_send_outer(&mut self, msg: Bytes) -> Poll<(), ChannelError> {
        debug_assert!(self.outer_buffered.is_none());

        let start_send = self.outer_sender.start_send(msg);

        if let AsyncSink::NotReady(msg) = start_send? {
            self.outer_buffered = Some(msg);
            Ok(Async::NotReady)
        } else {
            update_counter(&mut self.send_counter);
            Ok(Async::Ready(()))
        }
    }

    /// Attempt to pull out the next message **needed to be sent to remote**.
    fn try_poll_inner(&mut self) -> Poll<Option<Bytes>, ChannelError> {
        loop {
            let poll_result = self.inner_receiver.poll().map_err(|_| {
                ChannelError::RecvFromInnerFailed
            });

            if let Some(msg) = try_ready!(poll_result) {
                match msg {
                    ToChannel::TimeTick => {
                        self.remaining_tick_to_send_ka -= 1;
                        self.remaining_tick_to_recv_ka -= 1;

                        if self.remaining_tick_to_recv_ka == 0 {
                            return Err(ChannelError::KeepAliveTimeout);
                        } else {
                            if self.remaining_tick_to_send_ka == 0 {
                                self.remaining_tick_to_send_ka = KEEP_ALIVE_TICKS;

                                let msg = self.pack_msg(None)?;
                                return Ok(Async::Ready(Some(msg)));
                            }
                        }
                    }
                    ToChannel::SendMessage(raw) => {
                        let msg = self.pack_msg(Some(Bytes::from(raw)))?;
                        return Ok(Async::Ready(Some(msg)));
                    }
                }
            } else {
                // The internal channel has finish
                return Ok(Async::Ready(None));
            }
        }
    }

    /// Attempt to pull out the next message **needed to be sent to networker**.
    fn try_poll_outer(&mut self) -> Poll<Option<ChannelerToNetworker>, ChannelError> {
        loop {
            if let Some(raw) = try_ready!(self.outer_receiver.poll()) {
                match self.unpack_msg(raw)? {
                    Some(content) => {
                        let msg = ChannelerToNetworker::ChannelMessageReceived(
                            ChannelMessageReceived {
                                remote_public_key: self.remote_public_key.clone(),
                                message_content: content.to_vec(),
                            }
                        );

                        return Ok(Async::Ready(Some(msg)));
                    }
                    None => {
                        self.remaining_tick_to_recv_ka = 2 * KEEP_ALIVE_TICKS;
                    }
                }
            } else {
                // The tcp connection has finished
                return Ok(Async::Ready(None));
            }
        }
    }
}

impl Future for Channel {
    type Item  = ();
    type Error = ChannelError;

    fn poll(&mut self) -> Poll<(), Self::Error> {
        loop {
            if let Some(msg) = self.outer_buffered.take() {
                try_ready!(self.try_start_send_outer(msg));
            }
            match self.try_poll_inner()? {
                Async::Ready(None) => {
                    debug!("inner channel closed, closing");
                    return Ok(Async::Ready(()));
                }
                Async::Ready(Some(msg)) => {
                    try_ready!(self.try_start_send_outer(msg));
                }
                Async::NotReady => {
                    try_ready!(self.outer_sender.poll_complete());

                    if let Some(msg) = self.inner_buffered.take() {
                        try_ready!(self.try_start_send_inner(msg));
                    }
                    match self.try_poll_outer()? {
                        Async::Ready(None) => {
                            debug!("tcp connection closed, closing");
                            return Ok(Async::Ready(()));
                        }
                        Async::Ready(Some(msg)) => {
                            try_ready!(self.try_start_send_inner(msg));
                        }
                        Async::NotReady => {
                            try_ready!(
                                self.inner_sender.poll_complete()
                                    .map_err(|_| ChannelError::SendToNetworkerFailed)
                            );
                            return Ok(Async::NotReady);
                        }
                    }
                }
            }
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
enum Role {
    Initiator,
    Responder,
}

enum ChannelNewState {
    // Prepare a TCP connection used in Channel, at this stage, we should finish:
    //
    // 1. Establish a TCP connection to the remote
    // 2. Increase the `num_pending_out_conn` for the given neighbor
    PrepareTcp(Box<Future<Item=TcpStream, Error=ChannelError>>),

    // Prepare a serialized InitChannel message, at this stage, we should finish:
    //
    // 1. Request the public key from SecurityModuleClient
    // 2. Create and serialize the InitChannel message to send
    PrepareInit(Box<Future<Item=Bytes, Error=ChannelError>>),

    // Trying to send serialized InitChannel message
    SendInit(Option<Bytes>),

    // Waiting the InitChannel message from neighbor
    WaitInit,

    VerifyNeighbor {
        public_key: PublicKey,
        recv_rand_value: RandValue,
        verify_neighbor_fut: Box<Future<Item=(), Error=ChannelError>>,
    },

    // Prepare a serialized Exchange message, at this stage, we should finish:
    //
    // 1. Request the signature from SecurityModuleClient
    // 2. Create and serialize the Exchange message to send
    PrepareExchange(Box<Future<Item=Bytes, Error=ChannelError>>),

    // Trying to send serialized Exchange message
    SendExchange(Option<Bytes>),

    // Waiting the Exchange message from neighbor
    WaitExchange,

    // The final stage to build a channel, as this stage, we should finish:
    //
    // 1. Decrease the `num_pending_out_conn` for the given neighbor
    // 2. Send a `ChannelOpened` notification to Networker if needed
    // 3. Add a new mpsc::Sender<ToChannel> to the `channel_senders`
    FinalStage(Box<Future<Item=(SymmetricKey, SymmetricKey, mpsc::Receiver<ToChannel>), Error=ChannelError>>),

    // An error occurred in creating the channel, we need to do some cleanup:
    //
    // 1. Decrease the `num_pending_out_conn` for the given neighbor
    //
    // We should return the actual error after cleanup.
    Error(Box<Future<Item=ChannelError, Error=Option<PublicKey>>>),

    Empty,
}

#[must_use = "futures do nothing unless polled"]
pub struct ChannelNew {
    role: Role,
    state: RefCell<ChannelNewState>,
    timeout: Timeout,

    // Utils used in performing exchange
    rng:       SystemRandom,
    sm_client: SecurityModuleClient,

    networker_sender: mpsc::Sender<ChannelerToNetworker>,
    neighbors: FutMutex<HashMap<PublicKey, ChannelerNeighbor>>,

    // The public key of neighbor
    neighbor_public_key: Option<PublicKey>,

    sent_rand_value: Option<RandValue>,
    recv_rand_value: Option<RandValue>,

    // The parts used to perform DH exchange
    dh_key_salt:    Option<Salt>,
    dh_public_key:  Option<DhPublicKey>,
    dh_private_key: Option<DhPrivateKey>,

    sender:   Option<RefCell<SplitSink<Framed<TcpStream, PrefixFrameCodec>>>>,
    receiver: Option<RefCell<SplitStream<Framed<TcpStream, PrefixFrameCodec>>>>,
}

impl ChannelNew {
    #[inline]
    fn on_error<E: Into<ChannelError>>(&self, e: E) {
        let role = self.role.clone();
        let neighbor_public_key = self.neighbor_public_key.clone();
        let neighbor_public_key_copy = neighbor_public_key.clone();

        let original_error = e.into();

        let cleanup_fut = self.neighbors.clone().lock()
            .map_err(|_: ()| neighbor_public_key)
            .and_then(move |mut neighbors| {
                if let Some(public_key) = neighbor_public_key_copy {
                    match neighbors.get_mut(&public_key) {
                        None => {
                            error!("nonexistent neighbor");
                        }
                        Some(neighbor) => {
                            if role == Role::Initiator {
                                neighbor.num_pending_out_conn -= 1;
                            }
                        }
                    }
                }
                Ok(original_error)
            });

        self.state.replace(ChannelNewState::Error(Box::new(cleanup_fut)));
    }
}

impl Future for ChannelNew {
    type Item  = Channel;
    type Error = ChannelError;

    fn poll(&mut self) -> Poll<Channel, ChannelError> {
        trace!("ChannelNew::poll - {:?}", ::std::time::Instant::now());

        match self.timeout.poll() {
            Err(e) => self.on_error(e),
            Ok(Async::Ready(_)) => {
                error!("ChannelNew::timeout fired");
                return Err(ChannelError::Timeout);
            }
            Ok(Async::NotReady) => (),
        }

        loop {
            match self.state.replace(ChannelNewState::Empty) {
                ChannelNewState::Empty => unreachable!("can't poll twice"),
                ChannelNewState::Error(mut cleanup_fut) => {
                    match cleanup_fut.poll() {
                        Err(_public_key) => {
                            error!("an error occurred in decreasing the num_pending_out_conn");
                            // TODO: Should we retry here?
                            return Err(
                                ChannelError::Closed("failed to decrease the num_pending_out_conn")
                            );
                        }
                        Ok(Async::NotReady) => {
                            self.state.replace(ChannelNewState::Error(cleanup_fut));
                            return Ok(Async::NotReady);
                        }
                        Ok(Async::Ready(e)) => {
                            return Err(e);
                        }
                    }
                }
                ChannelNewState::PrepareTcp(mut prepare_tcp_fut) => {
                    match prepare_tcp_fut.poll() {
                        Err(e) => self.on_error(e),
                        Ok(Async::Ready(tcp_stream)) => {
                            trace!("ChannelNewState::PrepareTcp\t\t[Ready]");

                            let (tx, rx) = tcp_stream.framed(PrefixFrameCodec::new()).split();
                            self.sender   = Some(RefCell::new(tx));
                            self.receiver = Some(RefCell::new(rx));

                            let rand_value = RandValue::new(&self.rng);
                            self.sent_rand_value = Some(rand_value.clone());

                            let prepare_init_fut = self.sm_client.request_public_key()
                                .map_err(|e| e.into())
                                .and_then(move |public_key| {
                                    serialize_init_channel_message(rand_value, public_key)
                                        .map_err(|e| e.into())
                                });

                            self.state.replace(
                                ChannelNewState::PrepareInit(Box::new(prepare_init_fut))
                            );
                        }
                        Ok(Async::NotReady) => {
                            trace!("ChannelNewState::PrepareTcp\t\t[NotReady]");

                            self.state.replace(ChannelNewState::PrepareTcp(prepare_tcp_fut));
                            return Ok(Async::NotReady);
                        }
                    }
                }
                ChannelNewState::PrepareInit(mut prepare_init_fut) => {
                    match prepare_init_fut.poll() {
                        Err(e) => self.on_error(e),
                        Ok(Async::Ready(serialized_msg)) => {
                            trace!("ChannelNewState::PrepareInit\t\t[Ready]");

                            self.state.replace(ChannelNewState::SendInit(Some(serialized_msg)));
                        }
                        Ok(Async::NotReady) => {
                            trace!("ChannelNewState::PrepareInit\t\t[NotReady]");

                            self.state.replace(ChannelNewState::PrepareInit(prepare_init_fut));
                            return Ok(Async::NotReady);
                        }
                    }
                }
                ChannelNewState::SendInit(init_channel_msg) => {
                    let mut sender =
                        self.sender.as_ref().expect("sender is None").borrow_mut();

                    if let Some(msg) = init_channel_msg {
                        match sender.start_send(msg) {
                            Err(e) => self.on_error(e),
                            Ok(AsyncSink::Ready) => {
                                self.state.replace(ChannelNewState::SendInit(None));
                            }
                            Ok(AsyncSink::NotReady(msg)) => {
                                self.state.replace(ChannelNewState::SendInit(Some(msg)));
                                return Ok(Async::NotReady);
                            }
                        }
                    } else {
                        match sender.poll_complete() {
                            Err(e) => self.on_error(e),
                            Ok(Async::NotReady) => {
                                trace!("ChannelNewState::SendingInit\t\t[NotReady]");

                                self.state.replace(ChannelNewState::SendInit(None));
                                return Ok(Async::NotReady);
                            }
                            Ok(Async::Ready(_)) => {
                                trace!("ChannelNewState::SendingInit\t\t[Ready]");

                                self.state.replace(ChannelNewState::WaitInit);
                            }
                        }
                    }
                }
                ChannelNewState::WaitInit => {
                    let mut receiver =
                        self.receiver.as_ref().expect("receiver is None").borrow_mut();

                    match receiver.poll() {
                        Err(e) => self.on_error(e),
                        Ok(Async::Ready(None)) => {
                            error!("connection lost");
                            self.on_error(ChannelError::Closed("connection lost"));
                            return Err(ChannelError::Closed("connection lost"));
                        }
                        Ok(Async::Ready(Some(buffer))) => {
                            trace!("ChannelNewState::WaitInit\t\t[Ready]");

                            let (public_key, recv_rand_value) =
                                match deserialize_init_channel_message(buffer) {
                                    Err(e) => {
                                        self.on_error(e);
                                        continue;
                                    }
                                    Ok(res) => res,
                                };

                            let public_key_to_verify = public_key.clone();
                            let expected_public_key = self.neighbor_public_key.clone();

                            let verify_neighbor_fut = self.neighbors.clone().lock()
                                .map_err(|_: ()| ChannelError::FutMutex)
                                .and_then(move |neighbors| {
                                    if let Some(key) = expected_public_key {
                                        if key.as_ref() != public_key_to_verify.as_ref() {
                                            return Err(ChannelError::Closed("neighbor public key not match"));
                                        } else {
                                            return Ok(());
                                        }
                                    } else {
                                        match neighbors.get(&public_key_to_verify) {
                                            None => {
                                                return Err(ChannelError::Closed("unknown neighbor"));
                                            }
                                            Some(neighbor) => {
                                                if neighbor.info.neighbor_address.socket_addr.is_some() {
                                                    return Err(ChannelError::Closed("not allowed"));
                                                } else {
                                                    return Ok(());
                                                }
                                            }
                                        }
                                    }
                                });

                            self.state.replace(
                                ChannelNewState::VerifyNeighbor {
                                    public_key,
                                    recv_rand_value,
                                    verify_neighbor_fut: Box::new(verify_neighbor_fut),
                                }
                            );
                        }
                        Ok(Async::NotReady) => {
                            trace!("ChannelNewState::WaitInit\t\t[Not Ready]");

                            self.state.replace(ChannelNewState::WaitInit);
                            return Ok(Async::NotReady);
                        }
                    }
                }
                ChannelNewState::VerifyNeighbor { public_key, recv_rand_value, mut verify_neighbor_fut } => {
                    match verify_neighbor_fut.poll() {
                        Err(e) => self.on_error(e),
                        Ok(Async::NotReady) => {
                            trace!("ChannelNewState::VerifyNeighbor\t[Not Ready]");

                            self.state.replace(
                                ChannelNewState::VerifyNeighbor {
                                    public_key,
                                    recv_rand_value,
                                    verify_neighbor_fut: Box::new(verify_neighbor_fut),
                                }
                            );
                            return Ok(Async::NotReady);
                        }
                        Ok(Async::Ready(_)) => {
                            trace!("ChannelNewState::VerifyNeighbor\t[Ready]");
                            self.neighbor_public_key = Some(public_key);

                            // Generate ephemeral DH private key
                            let dh_key_salt    = Salt::new(&self.rng);
                            let dh_private_key = DhPrivateKey::new(&self.rng);
                            let dh_public_key  = dh_private_key.compute_public_key();

                            // message = (channelRandValue + commPublicKey + keySalt)
                            let mut message = Vec::with_capacity(1024);
                            message.extend_from_slice(recv_rand_value.as_bytes());
                            message.extend_from_slice(dh_public_key.as_bytes());
                            message.extend_from_slice(dh_key_salt.as_bytes());

                            // Keep these values
                            self.dh_key_salt     = Some(dh_key_salt.clone());
                            self.dh_public_key   = Some(dh_public_key.clone());
                            self.dh_private_key  = Some(dh_private_key);
                            self.recv_rand_value = Some(recv_rand_value);

                            let prepare_exchange_fut = self.sm_client.request_sign(message)
                                .map_err(|e| e.into())
                                .and_then(move |signature| {
                                    serialize_exchange_message(dh_public_key, dh_key_salt, signature)
                                        .map_err(|e| e.into())
                                });

                            self.state.replace(
                                ChannelNewState::PrepareExchange(Box::new(prepare_exchange_fut))
                            );
                        }
                    }
                }
                ChannelNewState::PrepareExchange(mut prepare_exchange_fut) => {
                    match prepare_exchange_fut.poll() {
                        Err(e) => self.on_error(e),
                        Ok(Async::Ready(serialized_msg)) => {
                            trace!("ChannelNewState::PrepareExchange\t[Ready]");

                            self.state.replace(ChannelNewState::SendExchange(Some(serialized_msg)));
                        }
                        Ok(Async::NotReady) => {
                            trace!("ChannelNewState::PrepareExchange\t[Not Ready]");

                            self.state.replace(
                                ChannelNewState::PrepareExchange(prepare_exchange_fut)
                            );
                            return Ok(Async::NotReady);
                        }
                    }
                }
                ChannelNewState::SendExchange(exchange_msg) => {
                    let mut sender =
                        self.sender.as_ref().expect("sender is None").borrow_mut();

                    if let Some(msg) = exchange_msg {
                        match sender.start_send(msg) {
                            Err(e) => self.on_error(e),
                            Ok(AsyncSink::Ready) => {
                                self.state.replace(ChannelNewState::SendExchange(None));
                            }
                            Ok(AsyncSink::NotReady(msg)) => {
                                self.state.replace(ChannelNewState::SendExchange(Some(msg)));
                                return Ok(Async::NotReady);
                            }
                        }
                    } else {
                        match sender.poll_complete() {
                            Err(e) => self.on_error(e),
                            Ok(Async::NotReady) => {
                                trace!("ChannelNewState::SendExchange\t[NotReady]");

                                self.state.replace(ChannelNewState::SendExchange(None));
                                return Ok(Async::NotReady);
                            }
                            Ok(Async::Ready(_)) => {
                                trace!("ChannelNewState::SendExchange\t[Ready]");

                                self.state.replace(ChannelNewState::WaitExchange);
                            }
                        }
                    }
                }
                ChannelNewState::WaitExchange => {
                    let mut receiver =
                        self.receiver.as_ref().expect("receiver is None").borrow_mut();

                    match receiver.poll() {
                        Err(e) => self.on_error(e),
                        Ok(Async::Ready(None)) => {
                            error!("connection lost");
                            self.on_error(ChannelError::Closed("connection lost"));
                        }
                        Ok(Async::Ready(Some(buffer))) => {
                            trace!("ChannelNewState::WaitExchange\t[Ready]");

                            let (public_key, recv_key_salt, signature) =
                                match deserialize_exchange_message(buffer) {
                                    Err(e) => {
                                        self.on_error(e);
                                        continue;
                                    }
                                    Ok(res) => res,
                                };

                            let channel_rand_value = match self.sent_rand_value {
                                None => unreachable!(),
                                Some(ref rand_value) => rand_value,
                            };

                            // message = (channelRandValue + commPublicKey + keySalt)
                            let mut message = Vec::new();
                            message.extend_from_slice(channel_rand_value.as_bytes());
                            message.extend_from_slice(public_key.as_bytes());
                            message.extend_from_slice(recv_key_salt.as_bytes());

                            let neighbor_public_key = match self.neighbor_public_key {
                                None => unreachable!(),
                                Some(ref key) => key.clone(),
                            };

                            let sent_key_salt = match self.dh_key_salt {
                                None => unreachable!(),
                                Some(ref salt) => salt.clone(),
                            };

                            // Verify this message was signed by the neighbor with its private key
                            if ::crypto::identity::verify_signature(
                                &message,
                                &neighbor_public_key,
                                &signature,
                            ) {
                                let dh_private_key =
                                    mem::replace(&mut self.dh_private_key, None).unwrap();
                                // received public key + sent_key_salt -> symmetric key for sending data
                                let key_send =
                                    dh_private_key.derive_symmetric_key(&public_key, &sent_key_salt);
                                // received public key + recv_key_salt -> symmetric key for receiving data
                                let key_recv =
                                    dh_private_key.derive_symmetric_key(&public_key, &recv_key_salt);

                                let role = self.role.clone();
                                let channel_uid = gen_uid(&self.rng);
                                let mut networker_sender = self.networker_sender.clone();

                                let final_stage_fut = self.neighbors.clone().lock()
                                    .map_err(|_: ()| ChannelError::FutMutex)
                                    .and_then(move |mut neighbors| {
                                        let (channel_sender, channel_receiver) =
                                            mpsc::channel::<ToChannel>(0);

                                        match neighbors.get_mut(&neighbor_public_key) {
                                            None => return Err(ChannelError::Closed("unknown neighbor")),
                                            Some(neighbor) => {
                                                if neighbor.channels.is_empty() {
                                                    let msg = ChannelerToNetworker::ChannelOpened(
                                                        ChannelOpened {
                                                            remote_public_key: neighbor_public_key,
                                                            locally_initialized: role == Role::Initiator,
                                                        });

                                                    if networker_sender.try_send(msg).is_err() {
                                                        error!("failed to notify the networker");
                                                        return Err(ChannelError::SendToNetworkerFailed);
                                                    }
                                                }
                                                neighbor.channels.push((channel_uid, channel_sender));

                                                if role == Role::Initiator {
                                                    neighbor.num_pending_out_conn -= 1;
                                                }
                                            }
                                        }

                                        Ok((key_send, key_recv, channel_receiver))
                                    });

                                self.state.replace(
                                    ChannelNewState::FinalStage(Box::new(final_stage_fut))
                                );
                            } else {
                                error!("invalid signature");
                                return Err(ChannelError::Closed("invalid signature"));
                            }
                        }
                        Ok(Async::NotReady) => {
                            trace!("ChannelNewState::WaitExchange\t[NotReady]");

                            self.state.replace(ChannelNewState::WaitExchange);
                            return Ok(Async::NotReady);
                        }
                    }
                }
                ChannelNewState::FinalStage(mut final_stage_fut) => {
                    match final_stage_fut.poll() {
                        Err(e) => self.on_error(e),
                        Ok(Async::NotReady) => {
                            trace!("ChannelNewState::FinalStage\t\t[Not Ready]");

                            self.state.replace(ChannelNewState::FinalStage(final_stage_fut));
                            return Ok(Async::NotReady);
                        }
                        Ok(Async::Ready((key_send, key_recv, channel_receiver))) => {
                            trace!("ChannelNewState::FinalStage\t\t[Ready]");
                            return Ok(Async::Ready(Channel {
                                remote_public_key: self.neighbor_public_key.take().unwrap(),
                                inner_sender: self.networker_sender.clone(),
                                inner_receiver: channel_receiver,
                                inner_buffered: None,
                                outer_sender: self.sender.take().unwrap().into_inner(),
                                outer_receiver: self.receiver.take().unwrap().into_inner(),
                                outer_buffered: None,
                                send_counter: 0,
                                recv_counter: 0,
                                encryptor: Encryptor::new(&key_send, EncNonceCounter::new(&mut self.rng)),
                                decryptor: Decryptor::new(&key_recv),
                                remaining_tick_to_send_ka: KEEP_ALIVE_TICKS,
                                remaining_tick_to_recv_ka: 2 * KEEP_ALIVE_TICKS, // FIXME: suitable value?
                            }));
                        }
                    }
                }
            }
        }
    }
}

// ===== helper functions =====

#[inline]
fn update_counter(counter: &mut u64) {
    if *counter == u64::max_value() {
        *counter = 0;
    } else {
        *counter += 1;
    }
}
