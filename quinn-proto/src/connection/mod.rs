use std::{
    cmp,
    collections::{BTreeMap, VecDeque},
    fmt, io, mem,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use bytes::{Bytes, BytesMut};
use err_derive::Error;
use rand::{rngs::StdRng, Rng, SeedableRng};
use tracing::{debug, error, trace, trace_span, warn};

use crate::{
    cid_queue::CidQueue,
    coding::BufMutExt,
    config::{EndpointConfig, ServerConfig, TransportConfig},
    crypto::{self, HeaderKey, KeyPair, Keys, PacketKey},
    frame,
    frame::{Close, Datagram, FrameStruct},
    packet::{Header, LongType, Packet, PacketNumber, PartialDecode, SpaceId},
    range_set::RangeSet,
    shared::{
        ConnectionEvent, ConnectionEventInner, ConnectionId, EcnCodepoint, EndpointEvent,
        EndpointEventInner, IssuedCid,
    },
    transport_parameters::{self, TransportParameters},
    Dir, Frame, Side, StreamId, Transmit, TransportError, TransportErrorCode, VarInt,
    LOC_CID_COUNT, MAX_STREAM_COUNT, MIN_INITIAL_SIZE, MIN_MTU, RESET_TOKEN_SIZE,
    TIMER_GRANULARITY,
};

mod assembler;
mod send_buffer;

mod spaces;
use spaces::{PacketSpace, Retransmits, SentPacket};

mod streams;
use streams::Streams;
pub use streams::{FinishError, ReadError, StreamEvent, UnknownStream, WriteError};

mod timer;
use timer::{Timer, TimerTable};

/// Protocol state and logic for a single QUIC connection
///
/// Objects of this type receive `ConnectionEvent`s and emit `EndpointEvents` and application
/// `Event`s to make progress. To handle timeouts, a `Connection` returns timer updates and
/// expects timeouts through various methods. A number of simple getter methods are exposed
/// to allow callers to inspect some of the connection state.
pub struct Connection<S>
where
    S: crypto::Session,
{
    endpoint_config: Arc<EndpointConfig<S>>,
    server_config: Option<Arc<ServerConfig<S>>>,
    config: Arc<TransportConfig>,
    rng: StdRng,
    crypto: S,
    /// The CID we initially chose, for use during the handshake
    handshake_cid: ConnectionId,
    /// The destination CID we're currently sending to
    ///
    /// The earliest value in a sliding window of CIDs issued by the peer.
    rem_cid: ConnectionId,
    /// The CID the peer initially chose, for use during the handshake
    rem_handshake_cid: ConnectionId,
    /// Sequence number of `rem_cid`
    ///
    /// Exactly one prior to `self.rem_cids.offset` except during processing of certain
    /// NEW_CONNECTION_ID frames.
    rem_cid_seq: u64,
    path: PathData,
    prev_path: Option<PathData>,
    state: State,
    side: Side,
    mtu: u16,
    /// Whether or not 0-RTT was enabled during the handshake. Does not imply acceptance.
    zero_rtt_enabled: bool,
    /// Set if 0-RTT is supported, then cleared when no longer needed.
    zero_rtt_crypto: Option<ZeroRttCrypto<S>>,
    key_phase: bool,
    /// Transport parameters set by the peer
    params: TransportParameters,
    /// ConnectionId sent by this client on the first Initial, if a Retry was received.
    orig_rem_cid: Option<ConnectionId>,
    /// Total number of outgoing packets that have been deemed lost
    lost_packets: u64,
    events: VecDeque<Event>,
    endpoint_events: VecDeque<EndpointEventInner>,
    /// Number of local connection IDs that have been issued in NEW_CONNECTION_ID frames.
    cids_issued: u64,
    /// Whether the spin bit is in use for this connection
    spin_enabled: bool,
    /// Outgoing spin bit state
    spin: bool,
    /// Packet number spaces: initial, handshake, 1-RTT
    spaces: [PacketSpace<S>; 3],
    /// Highest usable packet number space
    highest_space: SpaceId,
    /// 1-RTT keys used prior to a key update
    prev_crypto: Option<PrevCrypto<S::PacketKey>>,
    /// 1-RTT keys to be used for the next key update
    ///
    /// These are generated in advance to prevent timing attacks and/or DoS by third-party attackers
    /// spoofing key updates.
    next_crypto: Option<KeyPair<S::PacketKey>>,
    /// Latest PATH_CHALLENGE token issued to the peer along the current path
    path_challenge: Option<u64>,
    accepted_0rtt: bool,
    /// Whether the idle timer should be reset the next time an ack-eliciting packet is transmitted.
    permit_idle_reset: bool,
    /// Negotiated idle timeout
    idle_timeout: Option<Duration>,
    timers: TimerTable,

    //
    // Queued non-retransmittable 1-RTT data
    //
    path_challenge_pending: bool,
    path_response: Option<PathResponse>,
    close: bool,

    //
    // Loss Detection
    //
    /// The number of times all unacknowledged CRYPTO data has been retransmitted without receiving
    /// an ack.
    crypto_count: u32,
    /// The number of times a PTO has been sent without receiving an ack.
    pto_count: u32,

    //
    // Congestion Control
    //
    /// Summary statistics of packets that have been sent, but not yet acked or deemed lost
    in_flight: InFlight,
    /// The time when QUIC first detects a loss, causing it to enter recovery. When a packet sent
    /// after this time is acknowledged, QUIC exits recovery.
    recovery_start_time: Instant,
    /// Explicit congestion notification (ECN) counters
    ecn_counters: frame::EcnCounts,
    /// Whether the most recently received packet had an ECN codepoint set
    receiving_ecn: bool,
    /// Whether the remote address used to initiate the connection has been validated. Not used for
    /// validating migration; see path_challenge.
    remote_validated: bool,
    /// Total UDP datagram bytes received, tracked for handshake anti-amplification
    total_recvd: u64,
    total_sent: u64,

    streams: Streams,
    /// Surplus remote CIDs for future use on new paths
    rem_cids: CidQueue,
    /// State of the unreliable datagram extension
    datagrams: DatagramState,
}

impl<S> Connection<S>
where
    S: crypto::Session,
{
    pub(crate) fn new(
        endpoint_config: Arc<EndpointConfig<S>>,
        server_config: Option<Arc<ServerConfig<S>>>,
        config: Arc<TransportConfig>,
        init_cid: ConnectionId,
        loc_cid: ConnectionId,
        rem_cid: ConnectionId,
        remote: SocketAddr,
        crypto: S,
        now: Instant,
    ) -> Self {
        let side = if server_config.is_some() {
            Side::Server
        } else {
            Side::Client
        };
        let initial_space = PacketSpace {
            crypto: Some(S::initial_keys(&init_cid, side)),
            ..PacketSpace::new(now)
        };
        let state = State::Handshake(state::Handshake {
            rem_cid_set: side.is_server(),
            token: None,
            client_hello: None,
        });
        let mut rng = StdRng::from_entropy();
        let remote_validated = server_config
            .as_ref()
            .map_or(false, |c| c.use_stateless_retry);
        let mut this = Self {
            endpoint_config,
            server_config,
            crypto,
            handshake_cid: loc_cid,
            rem_cid,
            rem_handshake_cid: rem_cid,
            rem_cid_seq: 0,
            path: PathData {
                remote,
                rtt: RttEstimator::new(),
                congestion_window: config.initial_window,
                ssthresh: u64::max_value(),
                sending_ecn: true,
            },
            prev_path: None,
            side,
            state,
            mtu: MIN_MTU,
            zero_rtt_enabled: false,
            zero_rtt_crypto: None,
            key_phase: false,
            params: TransportParameters::default(),
            orig_rem_cid: None,
            lost_packets: 0,
            events: VecDeque::new(),
            endpoint_events: VecDeque::new(),
            cids_issued: 0,
            spin_enabled: config.allow_spin && rng.gen_ratio(7, 8),
            spin: false,
            spaces: [initial_space, PacketSpace::new(now), PacketSpace::new(now)],
            highest_space: SpaceId::Initial,
            prev_crypto: None,
            next_crypto: None,
            path_challenge: None,
            accepted_0rtt: false,
            permit_idle_reset: true,
            idle_timeout: config.max_idle_timeout,
            timers: TimerTable::default(),

            path_challenge_pending: false,
            path_response: None,
            close: false,

            crypto_count: 0,
            pto_count: 0,

            in_flight: InFlight::new(),
            recovery_start_time: now,
            ecn_counters: frame::EcnCounts::ZERO,
            receiving_ecn: false,
            remote_validated,
            total_recvd: 0,
            total_sent: 0,

            streams: Streams::new(
                side,
                config.stream_window_uni,
                config.stream_window_bidi,
                config.send_window,
                config.receive_window as u64,
                config.stream_receive_window as u64,
            ),
            datagrams: DatagramState::new(),
            config,
            rem_cids: CidQueue::new(1),
            rng,
        };
        if side.is_client() {
            // Kick off the connection
            this.write_crypto();
            this.init_0rtt();
        }
        this
    }

    /// Returns the next time at which `handle_timeout` should be called
    ///
    /// The value returned may change after:
    /// - the application performed some I/O on the connection
    /// - a call was made to `handle_event`
    /// - a call to `poll_transmit` returned `Some`
    /// - a call was made to `handle_timeout`
    pub fn poll_timeout(&mut self) -> Option<Instant> {
        self.timers.next_timeout()
    }

    /// Returns application-facing events
    ///
    /// Connections should be polled for events after:
    /// - a call was made to `handle_event`
    /// - a call was made to `handle_timeout`
    pub fn poll(&mut self) -> Option<Event> {
        if let Some(event) = self.streams.poll() {
            return Some(Event::Stream(event));
        }

        if let Some(x) = self.events.pop_front() {
            return Some(x);
        }

        None
    }

    /// Return endpoint-facing events
    pub fn poll_endpoint_events(&mut self) -> Option<EndpointEvent> {
        self.endpoint_events.pop_front().map(EndpointEvent)
    }

    /// Returns packets to transmit
    ///
    /// Connections should be polled for transmit after:
    /// - the application performed some I/O on the connection
    /// - a call was made to `handle_event`
    /// - a call was made to `handle_timeout`
    pub fn poll_transmit(&mut self, now: Instant) -> Option<Transmit> {
        if self.state.is_handshake()
            && !self.remote_validated
            && self.side.is_server()
            && self.total_recvd * 3 < self.total_sent + u64::from(self.mtu)
        {
            trace!("blocked by anti-amplification");
            return None;
        }

        // If we need to send a probe, make sure we have something to send.
        for space in SpaceId::iter() {
            if self.space(space).loss_probes != 0 {
                self.ensure_probe_queued(space);
            }
        }

        // Select the set of spaces that have data to send so we can try to coalesce them
        let (spaces, close) = match self.state {
            State::Drained => {
                return None;
            }
            State::Draining | State::Closed(_) => {
                if mem::replace(&mut self.close, false) {
                    (vec![self.highest_space], true)
                } else {
                    return None;
                }
            }
            _ => (
                SpaceId::iter()
                    .filter(|&x| {
                        (self.space(x).crypto.is_some() && self.space(x).can_send())
                            || (x == SpaceId::Data
                                && ((self.space(x).crypto.is_some() && self.can_send_1rtt())
                                    || (self.zero_rtt_crypto.is_some()
                                        && self.side.is_client()
                                        && (self.space(x).can_send() || self.can_send_1rtt()))))
                    })
                    .collect(),
                false,
            ),
        };

        let mut buf = Vec::with_capacity(self.mtu as usize);
        let mut coalesce = spaces.len() > 1;
        let pad_space = if self.side.is_client() && spaces.first() == Some(&SpaceId::Initial) {
            spaces.last().cloned()
        } else {
            None
        };

        for space_id in spaces {
            let buf_start = buf.len();
            let mut ack_eliciting =
                !self.space(space_id).pending.is_empty() || self.space(space_id).ping_pending;
            if space_id == SpaceId::Data {
                ack_eliciting |= self.can_send_1rtt();
                // Tail loss probes must not be blocked by congestion, or a deadlock could arise
                if ack_eliciting
                    && self.space(space_id).loss_probes == 0
                    && self.congestion_blocked()
                {
                    continue;
                }
            }

            //
            // From here on, we've determined that a packet will definitely be sent.
            //

            if self.spaces[SpaceId::Initial as usize].crypto.is_some()
                && space_id == SpaceId::Handshake
                && self.side.is_client()
            {
                // A client stops both sending and processing Initial packets when it
                // sends its first Handshake packet.
                self.discard_space(SpaceId::Initial);
            }
            if let Some(ref mut prev) = self.prev_crypto {
                prev.update_unacked = false;
            }

            let space = &mut self.spaces[space_id as usize];
            space.loss_probes = space.loss_probes.saturating_sub(1);
            let exact_number = space.get_tx_number();
            let span = trace_span!("send", space = ?space_id, pn = exact_number);
            let _guard = span.enter();
            let number = PacketNumber::new(exact_number, space.largest_acked_packet.unwrap_or(0));
            let header = match space_id {
                SpaceId::Data if space.crypto.is_some() => Header::Short {
                    dst_cid: self.rem_cid,
                    number,
                    spin: if self.spin_enabled {
                        self.spin
                    } else {
                        self.rng.gen()
                    },
                    key_phase: self.key_phase,
                },
                SpaceId::Data => Header::Long {
                    ty: LongType::ZeroRtt,
                    src_cid: self.handshake_cid,
                    dst_cid: self.rem_cid,
                    number,
                },
                SpaceId::Handshake => Header::Long {
                    ty: LongType::Handshake,
                    src_cid: self.handshake_cid,
                    dst_cid: self.rem_cid,
                    number,
                },
                SpaceId::Initial => Header::Initial {
                    src_cid: self.handshake_cid,
                    dst_cid: self.rem_cid,
                    token: match self.state {
                        State::Handshake(ref state) => {
                            state.token.clone().unwrap_or_else(Bytes::new)
                        }
                        _ => Bytes::new(),
                    },
                    number,
                },
            };
            let partial_encode = header.encode(&mut buf);
            coalesce = coalesce && !header.is_short();

            let sent = if close {
                trace!("sending CONNECTION_CLOSE");
                let max_len = buf.capacity()
                    - partial_encode.start
                    - partial_encode.header_len
                    - space.crypto.as_ref().unwrap().packet.local.tag_len();
                match self.state {
                    State::Closed(state::Closed { ref reason }) => reason.encode(&mut buf, max_len),
                    State::Draining => frame::ConnectionClose {
                        error_code: TransportErrorCode::NO_ERROR,
                        frame_type: None,
                        reason: Bytes::new(),
                    }
                    .encode(&mut buf, max_len),
                    _ => unreachable!(
                        "tried to make a close packet when the connection wasn't closed"
                    ),
                }
                coalesce = false;
                None
            } else {
                Some(self.populate_packet(space_id, &mut buf))
            };

            let space = &mut self.spaces[space_id as usize];
            let (header_crypto, packet_crypto) = if let Some(ref crypto) = space.crypto {
                (&crypto.header.local, &crypto.packet.local)
            } else if space_id == SpaceId::Data {
                let zero_rtt = self.zero_rtt_crypto.as_ref().unwrap();
                (&zero_rtt.header, &zero_rtt.packet)
            } else {
                unreachable!("tried to send {:?} packet without keys", space_id);
            };

            let mut padded = if pad_space == Some(space_id)
                && buf.len() < MIN_INITIAL_SIZE - packet_crypto.tag_len()
            {
                // Initial-bearing packets MUST be padded
                buf.resize(MIN_INITIAL_SIZE - packet_crypto.tag_len(), 0);
                true
            } else {
                false
            };

            let pn_len = number.len();
            // To ensure that sufficient data is available for sampling, packets are padded so that the
            // combined lengths of the encoded packet number and protected payload is at least 4 bytes
            // longer than the sample required for header protection.
            let protected_payload_len = (buf.len() + packet_crypto.tag_len())
                - partial_encode.start
                - partial_encode.header_len;
            if let Some(padding_minus_one) =
                (header_crypto.sample_size() + 3).checked_sub(pn_len + protected_payload_len)
            {
                let padding = padding_minus_one + 1;
                padded = true;
                trace!("PADDING * {}", padding);
                buf.resize(buf.len() + padding, 0);
            }

            buf.resize(buf.len() + packet_crypto.tag_len(), 0);
            debug_assert!(buf.len() < self.mtu as usize);
            let packet_buf = &mut buf[partial_encode.start..];
            partial_encode.finish(
                packet_buf,
                header_crypto,
                Some((exact_number, packet_crypto)),
            );

            if let Some((sent, acks, stream_frames)) = sent {
                // If we sent any acks, don't immediately resend them. Setting this even if ack_only is
                // false needlessly prevents us from ACKing the next packet if it's ACK-only, but saves
                // the need for subtler logic to avoid double-transmitting acks all the time.
                space.permit_ack_only &= acks.is_empty();

                self.on_packet_sent(
                    now,
                    space_id,
                    exact_number,
                    SentPacket {
                        acks,
                        time_sent: now,
                        size: if padded || ack_eliciting {
                            (buf.len() - buf_start) as u16
                        } else {
                            0
                        },
                        ack_eliciting,
                        retransmits: sent,
                        stream_frames,
                    },
                );
            }

            if !coalesce || buf.capacity() - buf.len() < MIN_PACKET_SPACE {
                break;
            }
        }

        if buf.is_empty() {
            return None;
        }

        trace!("sending {} byte datagram", buf.len());
        self.total_sent = self.total_sent.wrapping_add(buf.len() as u64);

        Some(Transmit {
            destination: self.path.remote,
            contents: buf.into(),
            ecn: if self.path.sending_ecn {
                Some(EcnCodepoint::ECT0)
            } else {
                None
            },
        })
    }

    /// Process `ConnectionEvent`s generated by the associated `Endpoint`
    ///
    /// Will execute protocol logic upon receipt of a connection event, in turn preparing signals
    /// (including application `Event`s, `EndpointEvent`s and outgoing datagrams) that should be
    /// extracted through the relevant methods.
    pub fn handle_event(&mut self, event: ConnectionEvent) {
        use self::ConnectionEventInner::*;
        match event.0 {
            Datagram {
                now,
                remote,
                ecn,
                first_decode,
                remaining,
            } => {
                // If this packet could initiate a migration and we're a client or a server that
                // forbids migration, drop the datagram. This could be relaxed to heuristically
                // permit NAT-rebinding-like migration.
                if remote != self.path.remote
                    && self.server_config.as_ref().map_or(true, |x| !x.migration)
                {
                    trace!("discarding packet from unrecognized peer {}", remote);
                    return;
                }

                self.total_recvd = self.total_recvd.wrapping_add(first_decode.len() as u64);

                self.handle_decode(now, remote, ecn, first_decode);
                if let Some(data) = remaining {
                    self.handle_coalesced(now, remote, ecn, data);
                }
            }
            NewIdentifiers(ids) => {
                ids.into_iter().rev().for_each(|frame| {
                    self.space_mut(SpaceId::Data).pending.new_cids.push(frame);
                });
            }
        }
    }

    /// Process timer expirations
    ///
    /// Executes protocol logic, potentially preparing signals (including application `Event`s,
    /// `EndpointEvent`s and outgoing datagrams) that should be extracted through the relevant
    /// methods.
    pub fn handle_timeout(&mut self, now: Instant) {
        for &timer in &Timer::VALUES {
            if !self.timers.is_expired(timer, now) {
                continue;
            }
            self.timers.stop(timer);
            trace!(timer = ?timer, "timeout");
            match timer {
                Timer::Close => {
                    self.state = State::Drained;
                    self.endpoint_events.push_back(EndpointEventInner::Drained);
                }
                Timer::Idle => {
                    self.close_common();
                    self.events.push_back(ConnectionError::TimedOut.into());
                    self.state = State::Drained;
                    self.endpoint_events.push_back(EndpointEventInner::Drained);
                }
                Timer::KeepAlive => {
                    trace!("sending keep-alive");
                    self.ping();
                }
                Timer::LossDetection => {
                    self.on_loss_detection_timeout(now);
                }
                Timer::KeyDiscard => {
                    self.zero_rtt_crypto = None;
                    self.prev_crypto = None;
                }
                Timer::PathValidation => {
                    debug!("path validation failed");
                    self.path_challenge = None;
                    self.path_challenge_pending = false;
                    if let Some(prev) = self.prev_path.take() {
                        self.path = prev;
                    }
                }
            }
        }
    }

    /// Close a connection immediately
    ///
    /// This does not ensure delivery of outstanding data. It is the application's responsibility to
    /// call this only when all important communications have been completed, e.g. by calling
    /// [`Connection::finish`] on outstanding streams and waiting for the corresponding
    /// [`StreamFinished`] event.
    ///
    /// If [`Connection::send_streams`] returns 0, all outstanding stream data has been
    /// delivered. There may still be data from the peer that has not been received.
    ///
    /// [`StreamFinished`]: Event::StreamFinished
    pub fn close(&mut self, now: Instant, error_code: VarInt, reason: Bytes) {
        let was_closed = self.state.is_closed();
        if !was_closed {
            self.close_common();
            self.set_close_timer(now);
            self.close = true;
            self.state = State::Closed(state::Closed {
                reason: Close::Application(frame::ApplicationClose { error_code, reason }),
            });
        }
    }

    /// Open a single stream if possible
    ///
    /// Returns `None` if the streams in the given direction are currently exhausted.
    pub fn open(&mut self, dir: Dir) -> Option<StreamId> {
        if self.state.is_closed() {
            return None;
        }
        // TODO: Queue STREAM_ID_BLOCKED if this fails
        let id = self.streams.open(&self.params, dir)?;
        Some(id)
    }

    /// Accept a remotely initiated stream of a certain directionality, if possible
    ///
    /// Returns `None` if there are no new incoming streams for this connection.
    pub fn accept(&mut self, dir: Dir) -> Option<StreamId> {
        let id = self.streams.accept(dir)?;
        self.alloc_remote_stream(id.dir());
        Some(id)
    }

    /// Read from the given recv stream, in undefined order
    ///
    /// While stream data is usually returned to the application in order, for some applications
    /// it can make sense to leverage the reduced latency of unordered reads. For ordered reads,
    /// the sibling `read()` method can be used.
    ///
    /// The return value if `Ok` contains the bytes and their offset in the stream.
    pub fn read_unordered(&mut self, id: StreamId) -> Result<Option<(Bytes, u64)>, ReadError> {
        Ok(self.streams.read_unordered(id)?.map(|(buf, offset, more)| {
            self.add_read_credits(id, more);
            (buf, offset)
        }))
    }

    /// Read from the given recv stream
    pub fn read(&mut self, id: StreamId, buf: &mut [u8]) -> Result<Option<usize>, ReadError> {
        Ok(self.streams.read(id, buf)?.map(|(len, more)| {
            self.add_read_credits(id, more);
            len
        }))
    }

    /// Send data on the given stream
    ///
    /// Returns the number of bytes successfully written.
    pub fn write(&mut self, stream: StreamId, data: &[u8]) -> Result<usize, WriteError> {
        assert!(stream.dir() == Dir::Bi || stream.initiator() == self.side);
        if self.state.is_closed() {
            trace!(%stream, "write blocked; connection draining");
            return Err(WriteError::Blocked);
        }
        self.streams.write(stream, data)
    }

    /// Signal to the peer that it should stop sending on the given recv stream
    pub fn stop_sending(&mut self, id: StreamId, error_code: VarInt) -> Result<(), UnknownStream> {
        assert!(
            id.dir() == Dir::Bi || id.initiator() != self.side,
            "only streams supporting incoming data may be stopped"
        );
        // Only bother if there's data we haven't received yet
        if !self.streams.is_peer_finished(id)? {
            let space = &mut self.spaces[SpaceId::Data as usize];
            space
                .pending
                .stop_sending
                .push(frame::StopSending { id, error_code });
        }
        Ok(())
    }

    /// Finish a send stream, signalling that no more data will be sent.
    ///
    /// If this fails, no [`Event::StreamFinished`] will be generated.
    pub fn finish(&mut self, id: StreamId) -> Result<(), FinishError> {
        self.streams.finish(id)?;
        Ok(())
    }

    /// Prepare to transmit an unreliable, unordered datagram
    ///
    /// The returned `DatagramSender` must be used to actually send a datagram. This allows the
    /// caller to defer materializing a datagram until one can be sent immediately without redundant
    /// checks
    ///
    /// Returns `Err` iff a `len`-byte datagram cannot currently be sent
    pub fn send_datagram(&mut self, data: Bytes) -> Result<(), SendDatagramError> {
        if self.config.datagram_receive_buffer_size.is_none() {
            return Err(SendDatagramError::Disabled);
        }
        let max = self
            .max_datagram_size()
            .ok_or(SendDatagramError::UnsupportedByPeer)?;
        while self.datagrams.outgoing_total > self.config.datagram_send_buffer_size {
            let prev = self
                .datagrams
                .outgoing
                .pop_front()
                .expect("datagrams.outgoing_total desynchronized");
            trace!(len = prev.data.len(), "dropping outgoing datagram");
            self.datagrams.outgoing_total -= prev.data.len();
        }
        if data.len() > max {
            return Err(SendDatagramError::TooLarge);
        }
        self.datagrams.outgoing_total += data.len();
        self.datagrams.outgoing.push_back(Datagram { data });
        Ok(())
    }

    /// Receive an unreliable, unordered datagram
    pub fn recv_datagram(&mut self) -> Option<Bytes> {
        let x = self.datagrams.incoming.pop_front()?.data;
        self.datagrams.recv_buffered -= x.len();
        Some(x)
    }

    /// Compute the maximum size of datagrams that may passed to `send_datagram`
    ///
    /// Returns `None` if datagrams are unsupported by the peer or disabled locally.
    ///
    /// This may change over the lifetime of a connection according to variation in the path MTU
    /// estimate. The peer can also enforce an arbitrarily small fixed limit, but if the peer's
    /// limit is large this is guaranteed to be a little over a kilobyte at minimum.
    ///
    /// Not necessarily the maximum size of received datagrams.
    pub fn max_datagram_size(&self) -> Option<usize> {
        // This is usually 1182 bytes, but we shouldn't document that without a doctest.
        let max_size = self.mtu as usize
            - 1                 // flags byte
            - self.rem_cid.len()
            - 4                 // worst-case packet number size
            - self.space(SpaceId::Data).crypto.as_ref().map_or_else(|| &self.zero_rtt_crypto.as_ref().unwrap().packet, |x| &x.packet.local).tag_len()
            - Datagram::SIZE_BOUND;
        let limit = self.params.max_datagram_frame_size?.into_inner();
        Some(limit.min(max_size as u64) as usize)
    }

    /// Ping the remote endpoint
    ///
    /// Causes an ACK-eliciting packet to be transmitted.
    pub fn ping(&mut self) {
        self.spaces[self.highest_space as usize].ping_pending = true;
    }

    #[doc(hidden)]
    pub fn initiate_key_update(&mut self) {
        self.update_keys(None, false);
    }

    /// Get a session reference
    pub fn crypto_session(&self) -> &S {
        &self.crypto
    }

    /// The number of streams that may have unacknowledged data.
    pub fn send_streams(&self) -> usize {
        self.streams.send_streams()
    }

    /// If the connection is currently handshaking
    pub fn is_handshaking(&self) -> bool {
        self.state.is_handshake()
    }

    /// If the connection is closed
    pub fn is_closed(&self) -> bool {
        self.state.is_closed()
    }

    /// If the connection is drained
    pub fn is_drained(&self) -> bool {
        self.state.is_drained()
    }

    /// For clients, if the peer accepted the 0-RTT data packets
    ///
    /// The value is meaningless until after the handshake completes.
    pub fn accepted_0rtt(&self) -> bool {
        self.accepted_0rtt
    }

    /// Whether 0-RTT is/was possible during the handshake
    pub fn has_0rtt(&self) -> bool {
        self.zero_rtt_enabled
    }

    /// Look up whether we're the client or server of this Connection
    pub fn side(&self) -> Side {
        self.side
    }

    /// The latest socket address for this connection's peer
    pub fn remote_address(&self) -> SocketAddr {
        self.path.remote
    }

    fn on_packet_sent(
        &mut self,
        now: Instant,
        space: SpaceId,
        packet_number: u64,
        packet: SentPacket,
    ) {
        let SentPacket {
            size,
            ack_eliciting,
            ..
        } = packet;

        self.in_flight.insert(&packet);
        self.space_mut(space)
            .sent_packets
            .insert(packet_number, packet);
        self.reset_keep_alive(now);
        if size != 0 {
            if ack_eliciting {
                self.space_mut(space).time_of_last_sent_ack_eliciting_packet = Some(now);
                if self.permit_idle_reset {
                    self.reset_idle_timeout(now);
                }
                self.permit_idle_reset = false;
            }
            self.set_loss_detection_timer();
        }
    }

    fn on_ack_received(
        &mut self,
        now: Instant,
        space: SpaceId,
        ack: frame::Ack,
    ) -> Result<(), TransportError> {
        if ack.largest >= self.space(space).next_packet_number {
            return Err(TransportError::PROTOCOL_VIOLATION("unsent packet acked"));
        }
        let new_largest = {
            let space = self.space_mut(space);
            if space
                .largest_acked_packet
                .map_or(true, |pn| ack.largest > pn)
            {
                space.largest_acked_packet = Some(ack.largest);
                if let Some(info) = space.sent_packets.get(&ack.largest) {
                    // This should always succeed, but a misbehaving peer might ACK a packet we
                    // haven't sent. At worst, that will result in us spuriously reducing the
                    // congestion window.
                    space.largest_acked_packet_sent = info.time_sent;
                }
                true
            } else {
                false
            }
        };

        // Avoid DoS from unreasonably huge ack ranges by filtering out just the new acks.
        let newly_acked = ack
            .iter()
            .flat_map(|range| self.space(space).sent_packets.range(range).map(|(&n, _)| n))
            .collect::<Vec<_>>();
        if newly_acked.is_empty() {
            return Ok(());
        }

        let mut ack_eliciting_acked = false;
        for &packet in &newly_acked {
            if let Some(info) = self.space_mut(space).sent_packets.remove(&packet) {
                self.space_mut(space).pending_acks.subtract(&info.acks);
                ack_eliciting_acked |= info.ack_eliciting;
                self.on_packet_acked(info);
            }
        }

        if new_largest && ack_eliciting_acked {
            let ack_delay = if space != SpaceId::Data {
                Duration::from_micros(0)
            } else {
                cmp::min(
                    self.max_ack_delay(),
                    Duration::from_micros(ack.delay << self.params.ack_delay_exponent),
                )
            };
            let rtt = instant_saturating_sub(now, self.space(space).largest_acked_packet_sent);
            self.path.rtt.update(ack_delay, rtt);
        }

        // Must be called before crypto/pto_count are clobbered
        self.detect_lost_packets(now, space);

        self.crypto_count = 0;
        self.pto_count = 0;

        // Explicit congestion notification
        if self.path.sending_ecn {
            if let Some(ecn) = ack.ecn {
                // We only examine ECN counters from ACKs that we are certain we received in transmit
                // order, allowing us to compute an increase in ECN counts to compare against the number
                // of newly acked packets that remains well-defined in the presence of arbitrary packet
                // reordering.
                if new_largest {
                    let sent = self.space(space).largest_acked_packet_sent;
                    self.process_ecn(now, space, newly_acked.len() as u64, ecn, sent);
                }
            } else {
                // We always start out sending ECN, so any ack that doesn't acknowledge it disables it.
                debug!("ECN not acknowledged by peer");
                self.path.sending_ecn = false;
            }
        }

        self.set_loss_detection_timer();
        Ok(())
    }

    /// Process a new ECN block from an in-order ACK
    fn process_ecn(
        &mut self,
        now: Instant,
        space: SpaceId,
        newly_acked: u64,
        ecn: frame::EcnCounts,
        largest_sent_time: Instant,
    ) {
        match self.space_mut(space).detect_ecn(newly_acked, ecn) {
            Err(e) => {
                debug!("halting ECN due to verification failure: {}", e);
                self.path.sending_ecn = false;
                // Wipe out the existing value because it might be garbage and could interfere with
                // future attempts to use ECN on new paths.
                self.space_mut(space).ecn_feedback = frame::EcnCounts::ZERO;
            }
            Ok(false) => {}
            Ok(true) => {
                self.congestion_event(now, largest_sent_time);
            }
        }
    }

    // Not timing-aware, so it's safe to call this for inferred acks, such as arise from
    // high-latency handshakes
    fn on_packet_acked(&mut self, info: SentPacket) {
        let was_congestion_blocked = self.congestion_blocked();
        self.in_flight.remove(&info);
        if info.ack_eliciting {
            // Congestion control
            // Do not increase congestion window in recovery period or while migrating, or if we
            // weren't sending at max rate.
            if !self.in_recovery(info.time_sent) && !self.migrating() && was_congestion_blocked {
                if self.path.congestion_window < self.path.ssthresh {
                    // Slow start.
                    self.path.congestion_window += u64::from(info.size);
                } else {
                    // Congestion avoidance.
                    self.path.congestion_window += self.endpoint_config.max_udp_payload_size
                        * u64::from(info.size)
                        / self.path.congestion_window;
                }
            }
        }

        // Update state for confirmed delivery of frames
        for (id, _) in info.retransmits.reset_stream {
            self.streams.reset_acked(id);
        }

        for frame in info.stream_frames {
            self.streams.ack(frame);
        }
    }

    fn set_key_discard_timer(&mut self, now: Instant) {
        let start = if self.zero_rtt_crypto.is_some() {
            now
        } else {
            self.prev_crypto
                .as_ref()
                .expect("no previous keys")
                .end_packet
                .as_ref()
                .expect("update not acknowledged yet")
                .1
        };
        self.timers.set(Timer::KeyDiscard, start + self.pto() * 3);
    }

    fn on_loss_detection_timeout(&mut self, now: Instant) {
        if let Some((_, pn_space)) = self.earliest_time_and_space(|x| x.loss_time) {
            // Time threshold loss Detection
            self.detect_lost_packets(now, pn_space);
            self.set_loss_detection_timer();
            return;
        }

        // Send two probes to improve odds of getting through under lossy conditions
        let space = self
            .earliest_time_and_space(|x| x.time_of_last_sent_ack_eliciting_packet)
            .map(|(_, space)| space)
            .unwrap_or_else(|| {
                // PTO expired with no sent ack-eliciting packets! This should only happen on a
                // client that's discarded the initial packet space but hasn't received enough data
                // from the server to send an ack-eliciting handshake packet
                // yet. https://github.com/quicwg/base-drafts/pull/3162 will change the behavior
                // here, but for now we generate an anti-amplification packet at handshake level.
                debug_assert!(self.side.is_client() && self.highest_space == SpaceId::Handshake);
                SpaceId::Handshake
            });
        trace!(
            in_flight = self.in_flight.bytes,
            count = self.pto_count,
            ?space,
            "PTO fired"
        );
        self.space_mut(space).loss_probes = self.space(space).loss_probes.saturating_add(2);
        self.pto_count = self.pto_count.saturating_add(1);
        self.set_loss_detection_timer();
    }

    /// Queue data for a tail loss probe (or anti-amplification deadlock prevention) packet
    ///
    /// Probes are sent similarly to normal packets when an expect ACK has not arrived. We never
    /// deem a packet lost until we receive an ACK that should have included it, but if a trailing
    /// run of packets (or their ACKs) are lost, this might not happen in a timely fashion. We send
    /// probe packets to force an ACK, and exempt them from congestion control to prevent a deadlock
    /// when the congestion window is filled with lost tail packets.
    ///
    /// We prefer to send new data, to make the most efficient use of bandwidth. If there's no data
    /// waiting to be sent, then we retransmit in-flight data to reduce odds of loss. If there's no
    /// in-flight data either, we're probably a client guarding against a handshake
    /// anti-amplification deadlock and we just make something up.
    fn ensure_probe_queued(&mut self, space: SpaceId) {
        // Retransmit the data of the oldest in-flight packet
        let space = self.space_mut(space);
        if !space.pending.is_empty() {
            // There's real data to send here, no need to make something up
            return;
        }
        for packet in space.sent_packets.values_mut() {
            if !packet.retransmits.is_empty() {
                // Remove retransmitted data from the old packet so we don't end up retransmitting
                // it *again* even if the copy we're sending now gets acknowledged.
                space.pending += mem::take(&mut packet.retransmits);
                return;
            }
        }
        // Nothing new to send and nothing to retransmit, so fall back on a ping. This should only
        // happen in rare cases during the handshake when the server becomes blocked by
        // anti-amplification.
        space.ping_pending = true;
    }

    fn detect_lost_packets(&mut self, now: Instant, pn_space: SpaceId) {
        let mut lost_packets = Vec::<u64>::new();
        let rtt = self
            .path
            .rtt
            .smoothed
            .map_or(self.path.rtt.latest, |x| cmp::max(x, self.path.rtt.latest));
        let loss_delay = cmp::max(rtt.mul_f32(self.config.time_threshold), TIMER_GRANULARITY);

        // Packets sent before this time are deemed lost.
        let lost_send_time = now - loss_delay;
        let largest_acked_packet = self.space(pn_space).largest_acked_packet.unwrap();
        let packet_threshold = self.config.packet_threshold as u64;

        let space = self.space_mut(pn_space);
        space.loss_time = None;
        for (&packet, info) in space.sent_packets.range(0..largest_acked_packet) {
            if info.time_sent <= lost_send_time || largest_acked_packet >= packet + packet_threshold
            {
                lost_packets.push(packet);
            } else {
                let next_loss_time = info.time_sent + loss_delay;
                space.loss_time = Some(
                    space
                        .loss_time
                        .map_or(next_loss_time, |x| cmp::min(x, next_loss_time)),
                );
            }
        }

        // OnPacketsLost
        if let Some(largest_lost) = lost_packets.last().cloned() {
            let old_bytes_in_flight = self.in_flight.bytes;
            let largest_lost_sent = self.space(pn_space).sent_packets[&largest_lost].time_sent;
            self.lost_packets += lost_packets.len() as u64;
            trace!("packets lost: {:?}", lost_packets);
            for packet in &lost_packets {
                let info = self
                    .space_mut(pn_space)
                    .sent_packets
                    .remove(&packet)
                    .unwrap(); // safe: lost_packets is populated just above
                self.in_flight.remove(&info);
                for frame in info.stream_frames {
                    self.streams.retransmit(frame);
                }
                self.space_mut(pn_space).pending += info.retransmits;
            }
            // Don't apply congestion penalty for lost ack-only packets
            let lost_ack_eliciting = old_bytes_in_flight != self.in_flight.bytes;

            // InPersistentCongestion: Determine if all packets in the time period before the newest
            // lost packet, including the edges, are marked lost
            let congestion_period = self.pto() * self.config.persistent_congestion_threshold;
            let in_persistent_congestion = self.space(pn_space).largest_acked_packet_sent
                < largest_lost_sent - congestion_period;

            if lost_ack_eliciting {
                self.congestion_event(now, largest_lost_sent);
                if in_persistent_congestion {
                    self.path.congestion_window = self.config.minimum_window;
                }
            }
        }
    }

    fn congestion_event(&mut self, now: Instant, sent_time: Instant) {
        // Start a new recovery epoch if the lost packet is larger than the end of the
        // previous recovery epoch.
        if self.in_recovery(sent_time) {
            return;
        }
        self.recovery_start_time = now;
        // Converting a u64 to f32 risks some precision loss, but a modest amount of error in
        // congestion window reductions is harmless.
        self.path.congestion_window =
            (self.path.congestion_window as f32 * self.config.loss_reduction_factor) as u64;
        self.path.congestion_window =
            cmp::max(self.path.congestion_window, self.config.minimum_window);
        self.path.ssthresh = self.path.congestion_window;
    }

    fn in_recovery(&self, sent_time: Instant) -> bool {
        sent_time <= self.recovery_start_time
    }

    fn earliest_time_and_space(
        &self,
        get: impl Fn(&PacketSpace<S>) -> Option<Instant>,
    ) -> Option<(Instant, SpaceId)> {
        SpaceId::iter()
            .filter(|&id| id != SpaceId::Data || !self.is_handshaking())
            .filter_map(|id| get(self.space(id)).map(|x| (x, id)))
            .min_by_key(|&(time, _)| time)
    }

    fn peer_not_awaiting_address_validation(&self) -> bool {
        if self.side.is_server() {
            return true;
        }
        // The server is guaranteed to have validated our address if any of our handshake or 1-RTT
        // packets are acknowledged or we've seen HANDSHAKE_DONE and discarded handshake keys.
        self.space(SpaceId::Handshake)
            .largest_acked_packet
            .is_some()
            || self.space(SpaceId::Data).largest_acked_packet.is_some()
            || (self.space(SpaceId::Data).crypto.is_some()
                && self.space(SpaceId::Handshake).crypto.is_none())
    }

    fn set_loss_detection_timer(&mut self) {
        if let Some((loss_time, _)) = self.earliest_time_and_space(|x| x.loss_time) {
            // Time threshold loss detection.
            self.timers.set(Timer::LossDetection, loss_time);
            return;
        }

        // Don't arm timer if there are no ack-eliciting packets
        // in flight and the handshake is complete.
        if self.in_flight.ack_eliciting == 0 && self.peer_not_awaiting_address_validation() {
            self.timers.stop(Timer::LossDetection);
            return;
        }

        // Calculate PTO duration
        if let Some((sent_time, _)) =
            self.earliest_time_and_space(|x| x.time_of_last_sent_ack_eliciting_packet)
        {
            let timeout = self.pto() * 2u32.pow(cmp::min(self.pto_count, MAX_BACKOFF_EXPONENT));
            self.timers.set(Timer::LossDetection, sent_time + timeout);
        } else {
            // Arises at least due to https://github.com/quicwg/base-drafts/issues/3502
            self.timers.stop(Timer::LossDetection);
        }
    }

    /// Probe Timeout
    fn pto(&self) -> Duration {
        match self.path.rtt.smoothed {
            None => 2 * self.config.initial_rtt,
            Some(srtt) => {
                srtt + cmp::max(4 * self.path.rtt.var, TIMER_GRANULARITY) + self.max_ack_delay()
            }
        }
    }

    fn on_packet_authenticated(
        &mut self,
        now: Instant,
        space_id: SpaceId,
        ecn: Option<EcnCodepoint>,
        packet: Option<u64>,
        spin: bool,
        is_1rtt: bool,
    ) {
        self.reset_keep_alive(now);
        self.reset_idle_timeout(now);
        self.permit_idle_reset = true;
        self.receiving_ecn |= ecn.is_some();
        if let Some(x) = ecn {
            self.ecn_counters += x;
        }

        let packet = match packet {
            Some(x) => x,
            None => return,
        };
        trace!("authenticated");
        if self.side.is_server() {
            if self.spaces[SpaceId::Initial as usize].crypto.is_some()
                && space_id == SpaceId::Handshake
            {
                // A server stops sending and processing Initial packets when it receives its first Handshake packet.
                self.discard_space(SpaceId::Initial);
            }
            if self.zero_rtt_crypto.is_some() && is_1rtt {
                // Discard 0-RTT keys soon after receiving a 1-RTT packet
                self.set_key_discard_timer(now)
            }
        }
        let space = &mut self.spaces[space_id as usize];
        space.pending_acks.insert_one(packet);
        if space.pending_acks.len() > MAX_ACK_BLOCKS {
            space.pending_acks.pop_min();
        }
        if packet >= space.rx_packet {
            space.rx_packet = packet;
            space.rx_packet_time = now;
            // Update outgoing spin bit, inverting iff we're the client
            self.spin = self.side.is_client() ^ spin;
        }
    }

    fn reset_idle_timeout(&mut self, now: Instant) {
        let timeout = match self.idle_timeout {
            None => return,
            Some(x) => x,
        };
        if self.state.is_closed() {
            self.timers.stop(Timer::Idle);
            return;
        }
        let dt = cmp::max(timeout, 3 * self.pto());
        self.timers.set(Timer::Idle, now + dt);
    }

    fn reset_keep_alive(&mut self, now: Instant) {
        let interval = match self.config.keep_alive_interval {
            Some(x) if self.state.is_established() => x,
            _ => return,
        };
        self.timers.set(Timer::KeepAlive, now + interval);
    }

    /// Abandon transmitting data on a stream
    ///
    /// # Panics
    /// - when applied to a receive stream or an unopened send stream
    pub fn reset(&mut self, stream_id: StreamId, error_code: VarInt) {
        self.reset_inner(stream_id, error_code, false);
    }

    /// `stopped` should be set iff this is an internal implicit reset due to `STOP_SENDING`
    fn reset_inner(&mut self, stream_id: StreamId, error_code: VarInt, stopped: bool) {
        assert!(
            stream_id.dir() == Dir::Bi || stream_id.initiator() == self.side,
            "only streams supporting outgoing data may be reset"
        );

        let stop_reason = if stopped { Some(error_code) } else { None };
        self.streams.reset(stream_id, stop_reason);

        self.spaces[SpaceId::Data as usize]
            .pending
            .reset_stream
            .push((stream_id, error_code));
    }

    /// Handle the already-decrypted first packet from the client
    ///
    /// Decrypting the first packet in the `Endpoint` allows stateless packet handling to be more
    /// efficient.
    pub(crate) fn handle_first_packet(
        &mut self,
        now: Instant,
        remote: SocketAddr,
        ecn: Option<EcnCodepoint>,
        packet_number: u64,
        packet: Packet,
        remaining: Option<BytesMut>,
    ) -> Result<(), ConnectionError> {
        let span = trace_span!("first recv");
        let _guard = span.enter();
        debug_assert!(self.side.is_server());
        let len = packet.header_data.len() + packet.payload.len();
        self.total_recvd = len as u64;

        self.on_packet_authenticated(
            now,
            SpaceId::Initial,
            ecn,
            Some(packet_number),
            false,
            false,
        );
        self.process_decrypted_packet(now, remote, Some(packet_number), packet)?;
        if let Some(data) = remaining {
            self.handle_coalesced(now, remote, ecn, data);
        }
        Ok(())
    }

    fn init_0rtt(&mut self) {
        let (header, packet) = match self.crypto.early_crypto() {
            Some(x) => x,
            None => return,
        };
        if self.side.is_client() {
            match self.crypto.transport_parameters() {
                Ok(params) => {
                    let params = params
                        .expect("crypto layer didn't supply transport parameters with ticket");
                    // Certain values must not be cached
                    let params = TransportParameters {
                        original_connection_id: None,
                        preferred_address: None,
                        stateless_reset_token: None,
                        ack_delay_exponent: TransportParameters::default().ack_delay_exponent,
                        active_connection_id_limit: 0,
                        ..params
                    };
                    self.set_params(params);
                }
                Err(e) => {
                    error!("session ticket has malformed transport parameters: {}", e);
                    return;
                }
            }
        }
        trace!("0-RTT enabled");
        self.zero_rtt_enabled = true;
        self.zero_rtt_crypto = Some(ZeroRttCrypto { header, packet });
    }

    fn read_crypto(
        &mut self,
        space: SpaceId,
        crypto: &frame::Crypto,
    ) -> Result<(), TransportError> {
        let expected = if !self.state.is_handshake() {
            SpaceId::Data
        } else if self.highest_space == SpaceId::Initial {
            SpaceId::Initial
        } else {
            SpaceId::Handshake
        };
        let end = crypto.offset + crypto.data.len() as u64;
        if space < expected && end > self.space(space).crypto_stream.offset() {
            warn!(
                "received new {:?} CRYPTO data when expecting {:?}",
                space, expected
            );
            return Err(TransportError::PROTOCOL_VIOLATION(
                "new data at unexpected encryption level",
            ));
        }

        let space = &mut self.spaces[space as usize];
        let max = space.crypto_stream.offset() + self.config.crypto_buffer_size as u64;
        if end > max {
            return Err(TransportError::CRYPTO_BUFFER_EXCEEDED(""));
        }
        space
            .crypto_stream
            .insert(crypto.offset, crypto.data.clone());
        let mut buf = [0; 8192];
        loop {
            let n = space.crypto_stream.read(&mut buf);
            if n == 0 {
                return Ok(());
            }
            trace!("read {} CRYPTO bytes", n);
            self.crypto.read_handshake(&buf[..n])?;
        }
    }

    fn write_crypto(&mut self) {
        loop {
            let space = self.highest_space;
            let mut outgoing = Vec::new();
            if let Some(crypto) = self.crypto.write_handshake(&mut outgoing) {
                match space {
                    SpaceId::Initial => {
                        self.upgrade_crypto(SpaceId::Handshake, crypto);
                    }
                    SpaceId::Handshake => {
                        self.upgrade_crypto(SpaceId::Data, crypto);
                    }
                    _ => unreachable!("got updated secrets during 1-RTT"),
                }
            }
            if outgoing.is_empty() {
                break;
            }
            let offset = self.space_mut(space).crypto_offset;
            let outgoing = Bytes::from(outgoing);
            if let State::Handshake(ref mut state) = self.state {
                if space == SpaceId::Initial && offset == 0 && self.side.is_client() {
                    state.client_hello = Some(outgoing.clone());
                }
            }
            self.space_mut(space).crypto_offset += outgoing.len() as u64;
            trace!("wrote {} {:?} CRYPTO bytes", outgoing.len(), space);
            self.space_mut(space)
                .pending
                .crypto
                .push_back(frame::Crypto {
                    offset,
                    data: outgoing,
                });
        }
    }

    /// Switch to stronger cryptography during handshake
    fn upgrade_crypto(&mut self, space: SpaceId, crypto: Keys<S>) {
        debug_assert!(
            self.spaces[space as usize].crypto.is_none(),
            "already reached packet space {:?}",
            space
        );
        trace!("{:?} keys ready", space);
        if space == SpaceId::Data {
            // Precompute the first key update
            self.next_crypto = Some(self.crypto.next_1rtt_keys());
        }
        self.spaces[space as usize].crypto = Some(crypto);
        debug_assert!(space as usize > self.highest_space as usize);
        self.highest_space = space;
        if space == SpaceId::Data && self.side.is_client() {
            // Discard 0-RTT keys because 1-RTT keys are available.
            self.zero_rtt_crypto = None;
        }
    }

    fn discard_space(&mut self, space: SpaceId) {
        debug_assert!(space != SpaceId::Data);
        trace!("discarding {:?} keys", space);
        let space = self.space_mut(space);
        space.crypto = None;
        space.time_of_last_sent_ack_eliciting_packet = None;
        space.loss_time = None;
        let sent_packets = mem::replace(&mut space.sent_packets, BTreeMap::new());
        for (_, packet) in sent_packets.into_iter() {
            self.in_flight.remove(&packet);
        }
        self.set_loss_detection_timer()
    }

    fn handle_coalesced(
        &mut self,
        now: Instant,
        remote: SocketAddr,
        ecn: Option<EcnCodepoint>,
        data: BytesMut,
    ) {
        self.total_recvd = self.total_recvd.wrapping_add(data.len() as u64);
        let mut remaining = Some(data);
        while let Some(data) = remaining {
            match PartialDecode::new(data, self.endpoint_config.local_cid_len) {
                Ok((partial_decode, rest)) => {
                    remaining = rest;
                    self.handle_decode(now, remote, ecn, partial_decode);
                }
                Err(e) => {
                    trace!("malformed header: {}", e);
                    return;
                }
            }
        }
    }

    fn handle_decode(
        &mut self,
        now: Instant,
        remote: SocketAddr,
        ecn: Option<EcnCodepoint>,
        partial_decode: PartialDecode,
    ) {
        let header_crypto = if partial_decode.is_0rtt() {
            if let Some(ref crypto) = self.zero_rtt_crypto {
                Some(&crypto.header)
            } else {
                debug!("dropping unexpected 0-RTT packet");
                return;
            }
        } else if let Some(space) = partial_decode.space() {
            if let Some(ref crypto) = self.spaces[space as usize].crypto {
                Some(&crypto.header.remote)
            } else {
                debug!(
                    "discarding unexpected {:?} packet ({} bytes)",
                    space,
                    partial_decode.len(),
                );
                return;
            }
        } else {
            // Unprotected packet
            None
        };

        match partial_decode.finish(header_crypto) {
            Ok(packet) => self.handle_packet(now, remote, ecn, packet),
            Err(e) => {
                trace!("unable to complete packet decoding: {}", e);
            }
        }
    }

    fn handle_packet(
        &mut self,
        now: Instant,
        remote: SocketAddr,
        ecn: Option<EcnCodepoint>,
        mut packet: Packet,
    ) {
        trace!(
            "got {:?} packet ({} bytes) from {} using id {}",
            packet.header.space(),
            packet.payload.len() + packet.header_data.len(),
            remote,
            packet.header.dst_cid(),
        );

        if self.is_handshaking() && remote != self.path.remote {
            debug!("discarding packet with unexpected remote during handshake");
            return;
        }

        let was_closed = self.state.is_closed();
        let was_drained = self.state.is_drained();
        let stateless_reset = self.params.stateless_reset_token.map_or(false, |token| {
            packet.payload.len() >= RESET_TOKEN_SIZE
                && packet.payload[packet.payload.len() - RESET_TOKEN_SIZE..] == token[..]
        });

        let result = match self.decrypt_packet(now, &mut packet) {
            Err(Some(e)) => {
                warn!("illegal packet: {}", e);
                Err(e.into())
            }
            Err(None) => {
                if stateless_reset {
                    debug!("got stateless reset");
                    Err(ConnectionError::Reset)
                } else {
                    debug!("failed to authenticate packet");
                    return;
                }
            }
            Ok(number) => {
                let span = match number {
                    Some(pn) => trace_span!("recv", space = ?packet.header.space(), pn),
                    None => trace_span!("recv", space = ?packet.header.space()),
                };
                let _guard = span.enter();

                let is_duplicate = |n| self.space_mut(packet.header.space()).dedup.insert(n);
                if number.map_or(false, is_duplicate) {
                    if stateless_reset {
                        Err(ConnectionError::Reset)
                    } else {
                        warn!("discarding possible duplicate packet");
                        return;
                    }
                } else if self.state.is_handshake() && packet.header.is_short() {
                    // TODO: SHOULD buffer these to improve reordering tolerance.
                    trace!("dropping short packet during handshake");
                    return;
                } else {
                    if !self.state.is_closed() {
                        let spin = match packet.header {
                            Header::Short { spin, .. } => spin,
                            _ => false,
                        };
                        self.on_packet_authenticated(
                            now,
                            packet.header.space(),
                            ecn,
                            number,
                            spin,
                            packet.header.is_1rtt(),
                        );
                    }
                    self.process_decrypted_packet(now, remote, number, packet)
                }
            }
        };

        // State transitions for error cases
        if let Err(conn_err) = result {
            self.events.push_back(conn_err.clone().into());
            self.state = match conn_err {
                ConnectionError::ApplicationClosed(reason) => State::closed(reason),
                ConnectionError::ConnectionClosed(reason) => State::closed(reason),
                ConnectionError::Reset => State::Drained,
                ConnectionError::TimedOut => {
                    unreachable!("timeouts aren't generated by packet processing");
                }
                ConnectionError::TransportError(err) => {
                    debug!("closing connection due to transport error: {}", err);
                    State::closed(err)
                }
                ConnectionError::VersionMismatch => State::Draining,
                ConnectionError::LocallyClosed => {
                    unreachable!("LocallyClosed isn't generated by packet processing")
                }
            };
        }

        if !was_closed && self.state.is_closed() {
            self.close_common();
            if !self.state.is_drained() {
                self.set_close_timer(now);
            }
        }
        if !was_drained && self.state.is_drained() {
            self.endpoint_events.push_back(EndpointEventInner::Drained);
            // Close timer may have been started previously, e.g. if we sent a close and got a
            // stateless reset in response
            self.timers.stop(Timer::Close);
        }

        // Transmit CONNECTION_CLOSE if necessary
        if let State::Closed(_) = self.state {
            self.close = remote == self.path.remote;
        }
    }

    fn process_decrypted_packet(
        &mut self,
        now: Instant,
        remote: SocketAddr,
        number: Option<u64>,
        packet: Packet,
    ) -> Result<(), ConnectionError> {
        match self.state {
            State::Handshake(ref mut state) => {
                match packet.header {
                    Header::Retry {
                        src_cid: rem_cid, ..
                    } => {
                        if self.side.is_server() {
                            return Err(
                                TransportError::PROTOCOL_VIOLATION("client sent Retry").into()
                            );
                        }

                        if self.orig_rem_cid.is_some()
                            || packet.payload.len() <= 16 // token + 16 byte tag
                            || !S::is_valid_retry(
                                &self.rem_cid,
                                &packet.header_data,
                                &packet.payload,
                            )
                        {
                            trace!("discarding invalid Retry");
                            // - After the client has received and processed an Initial or Retry
                            //   packet from the server, it MUST discard any subsequent Retry
                            //   packets that it receives.
                            // - A client MUST discard a Retry packet with a zero-length Retry Token
                            //   field.
                            // - Clients MUST discard Retry packets that have a Retry Integrity Tag
                            //   that cannot be validated
                            return Ok(());
                        }

                        trace!("retrying with CID {}", rem_cid);
                        let client_hello = state.client_hello.take().unwrap();
                        self.orig_rem_cid = Some(self.rem_cid);
                        self.rem_cid = rem_cid;
                        self.rem_handshake_cid = rem_cid;

                        let space = self.space_mut(SpaceId::Initial);
                        if let Some(info) = space.sent_packets.remove(&0) {
                            space.pending_acks.subtract(&info.acks);
                            self.on_packet_acked(info);
                        };

                        self.discard_space(SpaceId::Initial); // Make sure we clean up after any retransmitted Initials
                        self.spaces[0] = PacketSpace {
                            crypto: Some(S::initial_keys(&rem_cid, self.side)),
                            next_packet_number: self.spaces[0].next_packet_number,
                            crypto_offset: client_hello.len() as u64,
                            ..PacketSpace::new(now)
                        };
                        self.spaces[0].pending.crypto.push_back(frame::Crypto {
                            offset: 0,
                            data: client_hello,
                        });

                        // Retransmit all 0-RTT data
                        let zero_rtt = mem::replace(
                            &mut self.space_mut(SpaceId::Data).sent_packets,
                            BTreeMap::new(),
                        );
                        for (_, info) in zero_rtt {
                            self.in_flight.remove(&info);
                            self.space_mut(SpaceId::Data).pending += info.retransmits;
                        }
                        self.streams.retransmit_all_for_0rtt();

                        let token_len = packet.payload.len() - 16;
                        self.state = State::Handshake(state::Handshake {
                            token: Some(packet.payload.freeze().split_to(token_len)),
                            rem_cid_set: false,
                            client_hello: None,
                        });
                        Ok(())
                    }
                    Header::Long {
                        ty: LongType::Handshake,
                        src_cid: rem_cid,
                        ..
                    } => {
                        if rem_cid != self.rem_handshake_cid {
                            debug!(
                                "discarding packet with mismatched remote CID: {} != {}",
                                self.rem_handshake_cid, rem_cid
                            );
                            return Ok(());
                        }
                        self.remote_validated = true;

                        let state = state.clone();
                        self.process_early_payload(now, packet)?;
                        if self.state.is_closed() {
                            return Ok(());
                        }

                        if self.crypto.is_handshaking() {
                            trace!("handshake ongoing");
                            self.state = State::Handshake(state::Handshake {
                                token: None,
                                ..state
                            });
                            return Ok(());
                        }

                        if self.side.is_client() {
                            // Client-only beceause server params were set from the client's Initial
                            let params = self.crypto.transport_parameters()?.ok_or_else(|| {
                                TransportError {
                                    code: TransportErrorCode::crypto(0x6d),
                                    frame: None,
                                    reason: "transport parameters missing".into(),
                                }
                            })?;

                            if self.has_0rtt() {
                                if !self.crypto.early_data_accepted().unwrap() {
                                    self.reject_0rtt();
                                } else {
                                    self.accepted_0rtt = true;
                                    params.validate_0rtt(&self.params)?;
                                }
                            }
                            if let Some(token) = params.stateless_reset_token {
                                self.endpoint_events
                                    .push_back(EndpointEventInner::ResetToken(
                                        self.path.remote,
                                        token,
                                    ));
                            }
                            self.validate_params(&params)?;
                            self.set_params(params);
                            self.issue_cids();
                        } else {
                            // Server-only
                            self.space_mut(SpaceId::Data).pending.handshake_done = true;
                            self.discard_space(SpaceId::Handshake);
                        }

                        self.events.push_back(Event::Connected);
                        self.state = State::Established;
                        trace!("established");
                        Ok(())
                    }
                    Header::Initial {
                        src_cid: rem_cid, ..
                    } => {
                        if !state.rem_cid_set {
                            trace!("switching remote CID to {}", rem_cid);
                            let mut state = state.clone();
                            self.rem_cid = rem_cid;
                            self.rem_handshake_cid = rem_cid;
                            state.rem_cid_set = true;
                            self.state = State::Handshake(state);
                        } else if rem_cid != self.rem_handshake_cid {
                            debug!(
                                "discarding packet with mismatched remote CID: {} != {}",
                                self.rem_handshake_cid, rem_cid
                            );
                            return Ok(());
                        }

                        let starting_space = self.highest_space;
                        self.process_early_payload(now, packet)?;

                        if self.side.is_server()
                            && starting_space == SpaceId::Initial
                            && self.highest_space != SpaceId::Initial
                        {
                            let params = self.crypto.transport_parameters()?.ok_or_else(|| {
                                TransportError {
                                    code: TransportErrorCode::crypto(0x6d),
                                    frame: None,
                                    reason: "transport parameters missing".into(),
                                }
                            })?;
                            self.validate_params(&params)?;
                            self.set_params(params);
                            self.issue_cids();
                            self.init_0rtt();
                        }
                        Ok(())
                    }
                    Header::Long {
                        ty: LongType::ZeroRtt,
                        ..
                    } => {
                        self.process_payload(
                            now,
                            remote,
                            number.unwrap(),
                            packet.payload.freeze(),
                        )?;
                        Ok(())
                    }
                    Header::VersionNegotiate { .. } => {
                        debug!("remote doesn't support our version");
                        Err(ConnectionError::VersionMismatch)
                    }
                    Header::Short { .. } => unreachable!(
                        "short packets received during handshake are discarded in handle_packet"
                    ),
                }
            }
            State::Established => {
                match packet.header.space() {
                    SpaceId::Data => {
                        self.process_payload(now, remote, number.unwrap(), packet.payload.freeze())?
                    }
                    _ => self.process_early_payload(now, packet)?,
                }
                Ok(())
            }
            State::Closed(_) => {
                for frame in frame::Iter::new(packet.payload.freeze()) {
                    if let Frame::Close(_) = frame {
                        trace!("draining");
                        self.state = State::Draining;
                        break;
                    }
                }
                Ok(())
            }
            State::Draining | State::Drained => Ok(()),
        }
    }

    /// Process an Initial or Handshake packet payload
    fn process_early_payload(
        &mut self,
        now: Instant,
        packet: Packet,
    ) -> Result<(), TransportError> {
        debug_assert_ne!(packet.header.space(), SpaceId::Data);
        for frame in frame::Iter::new(packet.payload.freeze()) {
            let span = match frame {
                Frame::Padding => None,
                _ => Some(trace_span!("frame", ty = %frame.ty())),
            };
            let _guard = span.as_ref().map(|x| x.enter());
            // Check for ack-eliciting frames
            match frame {
                Frame::Ack(_) | Frame::Padding | Frame::Close(Close::Connection(_)) => {}
                _ => {
                    self.space_mut(packet.header.space()).permit_ack_only = true;
                }
            }
            // Process frames
            match frame {
                Frame::Padding | Frame::Ping => {}
                Frame::Crypto(frame) => {
                    self.read_crypto(packet.header.space(), &frame)?;
                }
                Frame::Ack(ack) => {
                    self.on_ack_received(now, packet.header.space(), ack)?;
                }
                Frame::Close(reason) => {
                    self.events.push_back(ConnectionError::from(reason).into());
                    self.state = State::Draining;
                    return Ok(());
                }
                Frame::Invalid { ty, reason } => {
                    let mut err = TransportError::FRAME_ENCODING_ERROR(reason);
                    err.frame = Some(ty);
                    return Err(err);
                }
                _ => {
                    let mut err =
                        TransportError::PROTOCOL_VIOLATION("illegal frame type in handshake");
                    err.frame = Some(frame.ty());
                    return Err(err);
                }
            }
        }

        self.write_crypto();
        Ok(())
    }

    fn process_payload(
        &mut self,
        now: Instant,
        remote: SocketAddr,
        number: u64,
        payload: Bytes,
    ) -> Result<(), TransportError> {
        let is_0rtt = self.space(SpaceId::Data).crypto.is_none();
        let mut is_probing_packet = true;
        let mut close = None;
        for frame in frame::Iter::new(payload) {
            let span = match frame {
                Frame::Padding => None,
                _ => Some(trace_span!("frame", ty = %frame.ty())),
            };

            let _guard = span.as_ref().map(|x| x.enter());
            if is_0rtt {
                match frame {
                    Frame::Crypto(_) | Frame::Close(Close::Application(_)) => {
                        return Err(TransportError::PROTOCOL_VIOLATION(
                            "illegal frame type in 0-RTT",
                        ));
                    }
                    _ => {}
                }
            }

            // Check for ack-eliciting frames
            match frame {
                Frame::Ack(_) | Frame::Padding | Frame::Close(_) => {}
                _ => {
                    self.space_mut(SpaceId::Data).permit_ack_only = true;
                }
            }
            // Check whether this could be a probing packet
            match frame {
                Frame::Padding
                | Frame::PathChallenge(_)
                | Frame::PathResponse(_)
                | Frame::NewConnectionId(_) => {}
                _ => {
                    is_probing_packet = false;
                }
            }
            match frame {
                Frame::Invalid { ty, reason } => {
                    let mut err = TransportError::FRAME_ENCODING_ERROR(reason);
                    err.frame = Some(ty);
                    return Err(err);
                }
                Frame::Crypto(frame) => {
                    self.read_crypto(SpaceId::Data, &frame)?;
                }
                Frame::Stream(frame) => {
                    self.streams.received(frame)?;
                }
                Frame::Ack(ack) => {
                    self.on_ack_received(now, SpaceId::Data, ack)?;
                }
                Frame::Padding | Frame::Ping => {}
                Frame::Close(reason) => {
                    close = Some(reason);
                }
                Frame::PathChallenge(token) => {
                    if self
                        .path_response
                        .as_ref()
                        .map_or(true, |x| x.packet <= number)
                    {
                        self.path_response = Some(PathResponse {
                            packet: number,
                            token,
                        });
                    }
                    if remote == self.path.remote {
                        // PATH_CHALLENGE on active path, possible off-path packet forwarding
                        // attack. Send a non-probing packet to recover the active path.
                        self.ping();
                    }
                }
                Frame::PathResponse(token) => {
                    if self.path_challenge != Some(token) || remote != self.path.remote {
                        continue;
                    }
                    trace!("path validated");
                    self.timers.stop(Timer::PathValidation);
                    self.path_challenge = None;
                }
                Frame::MaxData(bytes) => {
                    self.streams.received_max_data(bytes);
                }
                Frame::MaxStreamData { id, offset } => {
                    self.streams.received_max_stream_data(id, offset)?;
                }
                Frame::MaxStreams { dir, count } => {
                    self.streams.received_max_streams(dir, count)?;
                }
                Frame::ResetStream(frame) => {
                    if self.streams.received_reset(frame)? {
                        self.space_mut(SpaceId::Data).pending.max_data = true;
                    }
                }
                Frame::DataBlocked { offset } => {
                    debug!(offset, "peer claims to be blocked at connection level");
                }
                Frame::StreamDataBlocked { id, offset } => {
                    if id.initiator() == self.side && id.dir() == Dir::Uni {
                        debug!("got STREAM_DATA_BLOCKED on send-only {}", id);
                        return Err(TransportError::STREAM_STATE_ERROR(
                            "STREAM_DATA_BLOCKED on send-only stream",
                        ));
                    }
                    debug!(
                        stream = %id,
                        offset, "peer claims to be blocked at stream level"
                    );
                }
                Frame::StreamsBlocked { dir, limit } => {
                    debug!(
                        "peer claims to be blocked opening more than {} {} streams",
                        limit, dir
                    );
                }
                Frame::StopSending(frame::StopSending { id, error_code }) => {
                    if id.initiator() != self.side {
                        if id.dir() == Dir::Uni {
                            debug!("got STOP_SENDING on recv-only {}", id);
                            return Err(TransportError::STREAM_STATE_ERROR(
                                "STOP_SENDING on recv-only stream",
                            ));
                        }
                    } else if self.streams.is_local_unopened(id) {
                        return Err(TransportError::STREAM_STATE_ERROR(
                            "STOP_SENDING on unopened stream",
                        ));
                    }
                    self.reset_inner(id, error_code, true);
                }
                Frame::RetireConnectionId { sequence } => {
                    if self.endpoint_config.local_cid_len == 0 {
                        return Err(TransportError::PROTOCOL_VIOLATION(
                            "RETIRE_CONNECTION_ID when CIDs aren't in use",
                        ));
                    }
                    if sequence > self.cids_issued {
                        debug!(
                            sequence,
                            "got RETIRE_CONNECTION_ID for unissued sequence number"
                        );
                        return Err(TransportError::PROTOCOL_VIOLATION(
                            "RETIRE_CONNECTION_ID for unissued sequence number",
                        ));
                    }
                    self.endpoint_events
                        .push_back(EndpointEventInner::RetireConnectionId(sequence));
                }
                Frame::NewConnectionId(frame) => {
                    trace!(
                        sequence = frame.sequence,
                        id = %frame.id,
                    );
                    if self.rem_cid.is_empty() {
                        return Err(TransportError::PROTOCOL_VIOLATION(
                            "NEW_CONNECTION_ID when CIDs aren't in use",
                        ));
                    }
                    if frame.retire_prior_to > frame.sequence {
                        return Err(TransportError::PROTOCOL_VIOLATION(
                            "NEW_CONNECTION_ID retiring unissued CIDs",
                        ));
                    }

                    let retired = self.rem_cids.retire_prior_to(frame.retire_prior_to);
                    self.space_mut(SpaceId::Data)
                        .pending
                        .retire_cids
                        .extend(retired);

                    // Must be after retire_prior_to processing in case this CID has the highest
                    // permitted sequence number
                    use crate::cid_queue::InsertError;
                    match self.rem_cids.insert(IssuedCid {
                        sequence: frame.sequence,
                        id: frame.id,
                        reset_token: frame.reset_token,
                    }) {
                        Ok(()) => {}
                        Err(InsertError::ExceedsLimit) => {
                            return Err(TransportError::CONNECTION_ID_LIMIT_ERROR(""));
                        }
                        Err(InsertError::Retired) => {
                            trace!("discarding already-retired");
                            self.space_mut(SpaceId::Data)
                                .pending
                                .retire_cids
                                .push(frame.sequence);
                            continue;
                        }
                    }

                    if self.side.is_server() && self.params.stateless_reset_token.is_none() {
                        // We're a server using the initial remote CID for the client, so let's
                        // switch immediately to enable clientside stateless resets.
                        debug_assert_eq!(self.rem_cid_seq, 0);
                        self.update_rem_cid().unwrap();
                    } else if frame.retire_prior_to > self.rem_cid_seq {
                        // If our current CID is earlier than the first unretired one we must not
                        // have adopted the one we just got, so it must be stored in rem_cids, so
                        // this unwrap is guaranteed to succeed.
                        self.update_rem_cid().unwrap();
                    }
                }
                Frame::NewToken { token } => {
                    if self.side.is_server() {
                        return Err(TransportError::PROTOCOL_VIOLATION("client sent NEW_TOKEN"));
                    }
                    if token.is_empty() {
                        return Err(TransportError::FRAME_ENCODING_ERROR("empty token"));
                    }
                    trace!("got new token");
                    // TODO: Cache, or perhaps forward to user?
                }
                Frame::Datagram(datagram) => {
                    let window = match self.config.datagram_receive_buffer_size {
                        None => {
                            return Err(TransportError::PROTOCOL_VIOLATION(
                                "unexpected DATAGRAM frame",
                            ));
                        }
                        Some(x) => x,
                    };
                    if datagram.data.len() > window {
                        return Err(TransportError::PROTOCOL_VIOLATION("oversized datagram"));
                    }
                    if self.datagrams.recv_buffered == 0 {
                        self.events.push_back(Event::DatagramReceived);
                    }
                    while datagram.data.len() + self.datagrams.recv_buffered > window {
                        debug!("dropping stale datagram");
                        self.recv_datagram();
                    }
                    self.datagrams.recv_buffered += datagram.data.len();
                    self.datagrams.incoming.push_back(datagram);
                }
                Frame::HandshakeDone => {
                    if self.side.is_server() {
                        return Err(TransportError::PROTOCOL_VIOLATION(
                            "client sent HANDSHAKE_DONE",
                        ));
                    }
                    if self.space(SpaceId::Handshake).crypto.is_some() {
                        self.discard_space(SpaceId::Handshake);
                    }
                }
            }
        }

        if let Some(reason) = close {
            self.events.push_back(ConnectionError::from(reason).into());
            self.state = State::Draining;
            self.close = true;
        }

        if remote != self.path.remote
            && !is_probing_packet
            && number == self.space(SpaceId::Data).rx_packet
        {
            debug_assert!(
                self.server_config
                    .as_ref()
                    .expect("packets from unknown remote should be dropped by clients")
                    .migration,
                "migration-initiating packets should have been dropped immediately"
            );
            self.migrate(now, remote);
            // Break linkability, if possible
            let _ = self.update_rem_cid();
        }

        Ok(())
    }

    /// Whether a migration has been initiated and the new path has not yet been validated
    fn migrating(&self) -> bool {
        self.path_challenge.is_some()
    }

    fn migrate(&mut self, now: Instant, remote: SocketAddr) {
        trace!(%remote, "migration initiated");
        // Reset rtt/congestion state for new path unless it looks like a NAT rebinding.
        let maybe_rebinding = remote.is_ipv4() && remote.ip() == self.path.remote.ip();
        // Note that the congestion window will not grow until validation terminates. Helps mitigate
        // amplification attacks performed by spoofing source addresses.
        let new_path = PathData {
            remote,
            rtt: if maybe_rebinding {
                self.path.rtt
            } else {
                RttEstimator::new()
            },
            congestion_window: if maybe_rebinding {
                self.path.congestion_window
            } else {
                self.config.initial_window
            },
            ssthresh: if maybe_rebinding {
                self.path.ssthresh
            } else {
                u64::max_value()
            },
            // Try ECN on the new path if it's probably not the same as an old broken path.
            sending_ecn: self.path.sending_ecn || !maybe_rebinding,
        };
        let prev = Some(mem::replace(&mut self.path, new_path));
        // Don't clobber the original path if the previous one hasn't been validated yet
        if !self.migrating() {
            self.prev_path = prev;
        }

        // Initiate path validation
        self.timers.set(
            Timer::PathValidation,
            now + 3 * cmp::max(self.pto(), 2 * self.config.initial_rtt),
        );
        self.path_challenge = Some(self.rng.gen());
        self.path_challenge_pending = true;
    }

    /// Returns Err(()) if no CIDs were available
    fn update_rem_cid(&mut self) -> Result<(), ()> {
        let (cid, retired) = self.rem_cids.next().ok_or(())?;
        trace!("switching to remote CID {}: {}", cid.sequence, cid.id);

        // Retire the current remote CID and any CIDs we had to skip.
        let retire_cids = &mut self.spaces[SpaceId::Data as usize].pending.retire_cids;
        retire_cids.push(self.rem_cid_seq);
        retire_cids.extend(retired);

        // Apply the new CID
        self.rem_cid = cid.id;
        self.rem_cid_seq = cid.sequence;
        self.endpoint_events
            .push_back(EndpointEventInner::ResetToken(
                self.path.remote,
                cid.reset_token,
            ));
        self.params.stateless_reset_token = Some(cid.reset_token);

        // Reduce linkability
        self.spin = false;
        Ok(())
    }

    /// Issue an initial set of connection IDs to the peer
    fn issue_cids(&mut self) {
        if self.endpoint_config.local_cid_len == 0 {
            return;
        }

        // Subtract 1 to account for the CID we supplied while handshaking
        let n = self.params.active_connection_id_limit.min(LOC_CID_COUNT) - 1;
        self.endpoint_events
            .push_back(EndpointEventInner::NeedIdentifiers(n));
        self.cids_issued += n;
    }

    fn populate_packet(
        &mut self,
        space_id: SpaceId,
        buf: &mut Vec<u8>,
    ) -> (Retransmits, RangeSet, Vec<frame::StreamMeta>) {
        let space = &mut self.spaces[space_id as usize];
        let mut sent = Retransmits::default();
        let zero_rtt_crypto = self.zero_rtt_crypto.as_ref();
        let tag_len = space
            .crypto
            .as_ref()
            .map_or_else(
                || {
                    debug_assert_eq!(
                        space_id,
                        SpaceId::Data,
                        "tried to send {:?} packet without keys",
                        space_id
                    );
                    &zero_rtt_crypto.unwrap().packet
                },
                |x| &x.packet.local,
            )
            .tag_len();
        let max_size = buf.capacity() - tag_len;
        let is_0rtt = space_id == SpaceId::Data && space.crypto.is_none();

        // HANDSHAKE_DONE
        if !is_0rtt && mem::replace(&mut space.pending.handshake_done, false) {
            buf.write(frame::Type::HANDSHAKE_DONE);
            sent.handshake_done = true;
        }

        // PING
        if mem::replace(&mut space.ping_pending, false) {
            trace!("PING");
            buf.write(frame::Type::PING);
        }

        // ACK
        // 0-RTT packets must never carry acks (which would have to be of handshake packets)
        let acks = if !space.pending_acks.is_empty() {
            debug_assert!(space.crypto.is_some(), "tried to send ACK in 0-RTT");
            trace!("ACK");
            let ecn = if self.receiving_ecn {
                Some(&self.ecn_counters)
            } else {
                None
            };
            frame::Ack::encode(0, &space.pending_acks, ecn, buf);
            space.pending_acks.clone()
        } else {
            RangeSet::new()
        };

        // PATH_CHALLENGE
        if buf.len() + 9 < max_size && space_id == SpaceId::Data {
            // Transmit challenges with every outgoing frame on an unvalidated path
            if let Some(token) = self.path_challenge {
                // But only send a packet solely for that purpose at most once
                self.path_challenge_pending = false;
                trace!("PATH_CHALLENGE {:08x}", token);
                buf.write(frame::Type::PATH_CHALLENGE);
                buf.write(token);
            }
        }

        // PATH_RESPONSE
        if buf.len() + 9 < max_size && space_id == SpaceId::Data {
            if let Some(response) = self.path_response.take() {
                trace!("PATH_RESPONSE {:08x}", response.token);
                buf.write(frame::Type::PATH_RESPONSE);
                buf.write(response.token);
            }
        }

        // CRYPTO
        while buf.len() + frame::Crypto::SIZE_BOUND < max_size && !is_0rtt {
            let mut frame = match space.pending.crypto.pop_front() {
                Some(x) => x,
                None => break,
            };
            let len = cmp::min(
                frame.data.len(),
                max_size as usize - buf.len() - frame::Crypto::SIZE_BOUND,
            );
            let data = frame.data.split_to(len);
            let truncated = frame::Crypto {
                offset: frame.offset,
                data,
            };
            trace!(
                "CRYPTO: off {} len {}",
                truncated.offset,
                truncated.data.len()
            );
            truncated.encode(buf);
            sent.crypto.push_back(truncated);
            if !frame.data.is_empty() {
                frame.offset += len as u64;
                space.pending.crypto.push_front(frame);
            }
        }

        if space_id == SpaceId::Data {
            self.streams
                .write_control_frames(buf, &mut space.pending, &mut sent, max_size);
        }

        // NEW_CONNECTION_ID
        while buf.len() + 44 < max_size {
            let issued = match space.pending.new_cids.pop() {
                Some(x) => x,
                None => break,
            };
            trace!(
                sequence = issued.sequence,
                id = %issued.id,
                "NEW_CONNECTION_ID"
            );
            frame::NewConnectionId {
                sequence: issued.sequence,
                retire_prior_to: 0,
                id: issued.id,
                reset_token: issued.reset_token,
            }
            .encode(buf);
            sent.new_cids.push(issued);
        }

        // RETIRE_CONNECTION_ID
        while buf.len() + frame::RETIRE_CONNECTION_ID_SIZE_BOUND < max_size {
            let seq = match space.pending.retire_cids.pop() {
                Some(x) => x,
                None => break,
            };
            trace!(sequence = seq, "RETIRE_CONNECTION_ID");
            buf.write(frame::Type::RETIRE_CONNECTION_ID);
            buf.write_var(seq);
            sent.retire_cids.push(seq);
        }

        // DATAGRAM
        while buf.len() + Datagram::SIZE_BOUND < max_size && space_id == SpaceId::Data {
            let datagram = match self.datagrams.outgoing.pop_front() {
                Some(x) => x,
                None => break,
            };
            if buf.len() + datagram.size(true) > max_size {
                // Future work: we could be more clever about cramming small datagrams into
                // mostly-full packets when a larger one is queued first
                self.datagrams.outgoing.push_front(datagram);
                break;
            }
            self.datagrams.outgoing_total -= datagram.data.len();
            datagram.encode(true, buf);
        }

        // STREAM
        let stream_frames = if space_id == SpaceId::Data {
            self.streams.write_stream_frames(buf, max_size)
        } else {
            Vec::new()
        };

        (sent, acks, stream_frames)
    }

    fn close_common(&mut self) {
        trace!("connection closed");
        for &timer in &Timer::VALUES {
            self.timers.stop(timer);
        }
    }

    fn set_close_timer(&mut self, now: Instant) {
        self.timers.set(Timer::Close, now + 3 * self.pto());
    }

    /// Validate transport parameters received from the peer
    fn validate_params(&mut self, params: &TransportParameters) -> Result<(), TransportError> {
        if self.side.is_client() && self.orig_rem_cid != params.original_connection_id {
            debug!(
                "original connection ID mismatch: expected {:x?}, actual {:x?}",
                self.orig_rem_cid, params.original_connection_id
            );
            return Err(TransportError::TRANSPORT_PARAMETER_ERROR(
                "original CID mismatch",
            ));
        }
        if params.initial_max_streams_bidi > MAX_STREAM_COUNT
            || params.initial_max_streams_uni > MAX_STREAM_COUNT
        {
            return Err(TransportError::STREAM_LIMIT_ERROR(
                "unrepresentable initial stream limit",
            ));
        }

        Ok(())
    }

    fn set_params(&mut self, params: TransportParameters) {
        self.streams.set_params(&params);
        self.idle_timeout = match (self.config.max_idle_timeout, params.max_idle_timeout) {
            (None, 0) => None,
            (None, x) => Some(Duration::from_millis(x)),
            (Some(x), 0) => Some(x),
            (Some(x), y) => Some(cmp::min(x, Duration::from_millis(y))),
        };
        self.params = params;
    }

    /// Permit an additional remote `ty` stream.
    fn alloc_remote_stream(&mut self, dir: Dir) {
        let space = &mut self.spaces[SpaceId::Data as usize];
        match dir {
            Dir::Bi => {
                space.pending.max_bi_stream_id = true;
            }
            Dir::Uni => {
                space.pending.max_uni_stream_id = true;
            }
        }
        self.streams.alloc_remote_stream(&self.params, dir);
    }

    fn add_read_credits(&mut self, id: StreamId, more: bool) {
        let space = &mut self.spaces[SpaceId::Data as usize];
        space.pending.max_data = true;
        if more {
            // Only bother issuing stream credit if the peer wants to send more
            space.pending.max_stream_data.insert(id);
        }
    }

    /// Whether UDP transmits are currently blocked by link congestion
    fn congestion_blocked(&self) -> bool {
        self.in_flight.bytes + u64::from(self.mtu) >= self.path.congestion_window
    }

    fn decrypt_packet(
        &mut self,
        now: Instant,
        packet: &mut Packet,
    ) -> Result<Option<u64>, Option<TransportError>> {
        if !packet.header.is_protected() {
            // Unprotected packets also don't have packet numbers
            return Ok(None);
        }
        let space = packet.header.space();
        let rx_packet = self.space(space).rx_packet;
        let number = packet.header.number().ok_or(None)?.expand(rx_packet + 1);
        let key_phase = packet.header.key_phase();

        let mut crypto_update = false;
        let crypto = if packet.header.is_0rtt() {
            &self.zero_rtt_crypto.as_ref().unwrap().packet
        } else if key_phase == self.key_phase || space != SpaceId::Data {
            &self.spaces[space as usize]
                .crypto
                .as_mut()
                .unwrap()
                .packet
                .remote
        } else if let Some(prev) = self.prev_crypto.as_ref().and_then(|crypto| {
            // If this packet comes prior to acknowledgment of the key update by the peer,
            if crypto.end_packet.map_or(true, |(pn, _)| number < pn) {
                // use the previous keys.
                Some(crypto)
            } else {
                // Otherwise, this must be a remotely-initiated key update, so fall through to the
                // final case.
                None
            }
        }) {
            &prev.crypto.remote
        } else {
            // We're in the Data space with a key phase mismatch and either there is no locally
            // initiated key update or the locally initiated key update was acknowledged by a
            // lower-numbered packet. The key phase mismatch must therefore represent a new
            // remotely-initiated key update.
            crypto_update = true;
            &self.next_crypto.as_ref().unwrap().remote
        };

        crypto
            .decrypt(number, &packet.header_data, &mut packet.payload)
            .map_err(|()| {
                trace!("decryption failed with packet number {}", number);
                None
            })?;

        if let Some(ref mut prev) = self.prev_crypto {
            if prev.end_packet.is_none() && key_phase == self.key_phase {
                // Outgoing key update newly acknowledged
                prev.end_packet = Some((number, now));
                self.set_key_discard_timer(now);
            }
        }

        if !packet.reserved_bits_valid() {
            return Err(Some(TransportError::PROTOCOL_VIOLATION(
                "reserved bits set",
            )));
        }

        if crypto_update {
            // Validate and commit incoming key update
            if number <= rx_packet
                || self
                    .prev_crypto
                    .as_ref()
                    .map_or(false, |x| x.update_unacked)
            {
                return Err(Some(TransportError::KEY_UPDATE_ERROR("")));
            }
            trace!("key update authenticated");
            self.update_keys(Some((number, now)), true);
            self.set_key_discard_timer(now);
        }

        Ok(Some(number))
    }

    fn update_keys(&mut self, end_packet: Option<(u64, Instant)>, remote: bool) {
        // Generate keys for the key phase after the one we're switching to, store them in
        // `next_crypto`, make the contents of `next_crypto` current, and move the current keys into
        // `prev_crypto`.
        let new = self.crypto.next_1rtt_keys();
        let old = mem::replace(
            &mut self.spaces[SpaceId::Data as usize]
                .crypto
                .as_mut()
                .unwrap() // safe because update_keys() can only be triggered by short packets
                .packet,
            mem::replace(self.next_crypto.as_mut().unwrap(), new),
        );
        self.prev_crypto = Some(PrevCrypto {
            crypto: old,
            end_packet,
            update_unacked: remote,
        });
        self.key_phase = !self.key_phase;
    }

    /// The number of bytes of packets containing retransmittable frames that have not been
    /// acknowledged or declared lost.
    #[cfg(test)]
    pub(crate) fn bytes_in_flight(&self) -> u64 {
        self.in_flight.bytes
    }

    /// Number of bytes worth of non-ack-only packets that may be sent
    #[cfg(test)]
    pub(crate) fn congestion_state(&self) -> u64 {
        self.path
            .congestion_window
            .saturating_sub(self.in_flight.bytes)
    }

    /// Whether no timers but keepalive and idle are running
    #[cfg(test)]
    pub(crate) fn is_idle(&self) -> bool {
        Timer::VALUES
            .iter()
            .filter(|&&t| t != Timer::KeepAlive)
            .filter_map(|&t| Some((t, self.timers.get(t)?)))
            .min_by_key(|&(_, time)| time)
            .map_or(true, |(timer, _)| timer == Timer::Idle)
    }

    /// Total number of outgoing packets that have been deemed lost
    #[cfg(test)]
    pub(crate) fn lost_packets(&self) -> u64 {
        self.lost_packets
    }

    /// Whether explicit congestion notification is in use on outgoing packets.
    #[cfg(test)]
    pub(crate) fn using_ecn(&self) -> bool {
        self.path.sending_ecn
    }

    fn max_ack_delay(&self) -> Duration {
        Duration::from_micros(self.params.max_ack_delay * 1000)
    }

    fn space(&self, id: SpaceId) -> &PacketSpace<S> {
        &self.spaces[id as usize]
    }

    fn space_mut(&mut self, id: SpaceId) -> &mut PacketSpace<S> {
        &mut self.spaces[id as usize]
    }

    /// Whether we have 1-RTT data to send
    ///
    /// See also `self.space(SpaceId::Data).can_send()`
    fn can_send_1rtt(&self) -> bool {
        self.streams.can_send()
            || self.path_challenge_pending
            || self.path_response.is_some()
            || !self.datagrams.outgoing.is_empty()
    }

    /// Reset state to account for 0-RTT being ignored by the server
    fn reject_0rtt(&mut self) {
        debug_assert!(self.side.is_client());
        debug!("0-RTT rejected");
        self.accepted_0rtt = false;
        self.streams.zero_rtt_rejected();
        // Discard already-queued frames
        self.space_mut(SpaceId::Data).pending = Retransmits::default();
        // Discard 0-RTT packets
        let sent_packets = mem::replace(
            &mut self.space_mut(SpaceId::Data).sent_packets,
            BTreeMap::new(),
        );
        for (_, packet) in sent_packets {
            self.in_flight.remove(&packet);
        }
    }
}

impl<S> fmt::Debug for Connection<S>
where
    S: crypto::Session,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Connection")
            .field("handshake_cid", &self.handshake_cid)
            .finish()
    }
}

pub fn initial_close<S, R>(
    crypto: &Keys<S>,
    remote_id: &ConnectionId,
    local_id: &ConnectionId,
    packet_number: u8,
    reason: R,
) -> Box<[u8]>
where
    S: crypto::Session,
    R: Into<Close>,
{
    let number = PacketNumber::U8(packet_number);
    let header = Header::Initial {
        dst_cid: *remote_id,
        src_cid: *local_id,
        number,
        token: Bytes::new(),
    };

    let mut buf = Vec::<u8>::new();
    let partial_encode = header.encode(&mut buf);
    let max_len = MIN_MTU as usize - partial_encode.header_len - crypto.packet.local.tag_len();
    reason.into().encode(&mut buf, max_len);
    buf.resize(buf.len() + crypto.packet.local.tag_len(), 0);
    partial_encode.finish(
        &mut buf,
        &crypto.header.local,
        Some((u64::from(packet_number), &crypto.packet.local)),
    );
    buf.into()
}

/// Reasons why a connection might be lost.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ConnectionError {
    /// The peer doesn't implement any supported version.
    #[error(display = "peer doesn't implement any supported version")]
    VersionMismatch,
    /// The peer violated the QUIC specification as understood by this implementation.
    #[error(display = "{}", _0)]
    TransportError(#[source] TransportError),
    /// The peer's QUIC stack aborted the connection automatically.
    #[error(display = "aborted by peer: {}", 0)]
    ConnectionClosed(frame::ConnectionClose),
    /// The peer closed the connection.
    #[error(display = "closed by peer: {}", 0)]
    ApplicationClosed(frame::ApplicationClose),
    /// The peer is unable to continue processing this connection, usually due to having restarted.
    #[error(display = "reset by peer")]
    Reset,
    /// The peer has become unreachable.
    #[error(display = "timed out")]
    TimedOut,
    /// The local application closed the connection.
    #[error(display = "closed")]
    LocallyClosed,
}

impl From<Close> for ConnectionError {
    fn from(x: Close) -> Self {
        match x {
            Close::Connection(reason) => ConnectionError::ConnectionClosed(reason),
            Close::Application(reason) => ConnectionError::ApplicationClosed(reason),
        }
    }
}

// For compatibility with API consumers
impl From<ConnectionError> for io::Error {
    fn from(x: ConnectionError) -> io::Error {
        use self::ConnectionError::*;
        let kind = match x {
            TimedOut => io::ErrorKind::TimedOut,
            Reset => io::ErrorKind::ConnectionReset,
            ApplicationClosed(_) | ConnectionClosed(_) => io::ErrorKind::ConnectionAborted,
            TransportError(_) | VersionMismatch | LocallyClosed => io::ErrorKind::Other,
        };
        io::Error::new(kind, x)
    }
}

impl From<transport_parameters::Error> for ConnectionError {
    fn from(e: transport_parameters::Error) -> Self {
        TransportError::from(e).into()
    }
}

#[derive(Clone)]
enum State {
    Handshake(state::Handshake),
    Established,
    Closed(state::Closed),
    Draining,
    /// Waiting for application to call close so we can dispose of the resources
    Drained,
}

impl State {
    fn closed<R: Into<Close>>(reason: R) -> Self {
        State::Closed(state::Closed {
            reason: reason.into(),
        })
    }

    fn is_handshake(&self) -> bool {
        match *self {
            State::Handshake(_) => true,
            _ => false,
        }
    }

    fn is_established(&self) -> bool {
        match *self {
            State::Established => true,
            _ => false,
        }
    }

    fn is_closed(&self) -> bool {
        match *self {
            State::Closed(_) => true,
            State::Draining => true,
            State::Drained => true,
            _ => false,
        }
    }

    fn is_drained(&self) -> bool {
        if let State::Drained = *self {
            true
        } else {
            false
        }
    }
}

mod state {
    use super::*;

    #[derive(Clone)]
    pub struct Handshake {
        /// Whether the remote CID has been set by the peer yet
        ///
        /// Always set for servers
        pub rem_cid_set: bool,
        /// Stateless retry token, if the peer has provided one
        ///
        /// Only set for clients
        pub token: Option<Bytes>,
        /// First cryptographic message
        ///
        /// Only set for clients
        pub client_hello: Option<Bytes>,
    }

    #[derive(Clone)]
    pub struct Closed {
        pub reason: Close,
    }
}

/// Ensures we can always fit all our ACKs in a single minimum-MTU packet with room to spare
const MAX_ACK_BLOCKS: usize = 64;

struct PrevCrypto<K>
where
    K: crypto::PacketKey,
{
    /// The keys used for the previous key phase, temporarily retained to decrypt packets sent by
    /// the peer prior to its own key update.
    crypto: KeyPair<K>,
    /// The incoming packet that ends the interval for which these keys are applicable, and the time
    /// of its receipt.
    ///
    /// Incoming packets should be decrypted using these keys iff this is `None` or their packet
    /// number is lower. `None` indicates that we have not yet received a packet using newer keys,
    /// which implies that the update was locally initiated.
    end_packet: Option<(u64, Instant)>,
    /// Whether the following key phase is from a remotely initiated update that we haven't acked
    update_unacked: bool,
}

struct InFlight {
    /// Sum of the sizes of all sent packets considered "in flight" by congestion control
    ///
    /// The size does not include IP or UDP overhead. Packets only containing ACK frames do not
    /// count towards this to ensure congestion control does not impede congestion feedback.
    bytes: u64,
    /// Number of packets in flight containing frames other than ACK and PADDING
    ///
    /// This can be 0 even when bytes is not 0 because PADDING frames cause a packet to be
    /// considered "in flight" by congestion control. However, if this is nonzero, bytes will always
    /// also be nonzero.
    ack_eliciting: u64,
}

impl InFlight {
    pub fn new() -> Self {
        Self {
            bytes: 0,
            ack_eliciting: 0,
        }
    }

    fn insert(&mut self, packet: &SentPacket) {
        self.bytes += u64::from(packet.size);
        self.ack_eliciting += u64::from(packet.ack_eliciting);
    }

    /// Update counters to account for a packet becoming acknowledged, lost, or abandoned
    fn remove(&mut self, packet: &SentPacket) {
        self.bytes -= u64::from(packet.size);
        self.ack_eliciting -= u64::from(packet.ack_eliciting);
    }
}

#[derive(Copy, Clone)]
struct RttEstimator {
    /// The most recent RTT measurement made when receiving an ack for a previously unacked packet
    latest: Duration,
    /// The smoothed RTT of the connection, computed as described in RFC6298
    smoothed: Option<Duration>,
    /// The RTT variance, computed as described in RFC6298
    var: Duration,
    /// The minimum RTT seen in the connection, ignoring ack delay.
    min: Duration,
}

impl RttEstimator {
    fn new() -> Self {
        Self {
            latest: Duration::new(0, 0),
            smoothed: None,
            var: Duration::new(0, 0),
            min: Duration::new(u64::max_value(), 0),
        }
    }

    fn update(&mut self, ack_delay: Duration, rtt: Duration) {
        self.latest = rtt;
        // min_rtt ignores ack delay.
        self.min = cmp::min(self.min, self.latest);
        // Adjust for ack delay if it's plausible.
        if self.latest - self.min > ack_delay {
            self.latest -= ack_delay;
        }
        // Based on RFC6298.
        if let Some(smoothed) = self.smoothed {
            let var_sample = if smoothed > self.latest {
                smoothed - self.latest
            } else {
                self.latest - smoothed
            };
            self.var = (3 * self.var + var_sample) / 4;
            self.smoothed = Some((7 * smoothed + self.latest) / 8);
        } else {
            self.smoothed = Some(self.latest);
            self.var = self.latest / 2;
        }
    }
}

/// Events of interest to the application
#[derive(Debug)]
pub enum Event {
    /// The connection was successfully established
    Connected,
    /// The connection was lost
    ///
    /// Emitted if the peer closes the connection or an error is encountered.
    ConnectionLost {
        /// Reason that the connection was closed
        reason: ConnectionError,
    },
    /// Stream events
    Stream(StreamEvent),
    /// One or more application datagrams have been received
    DatagramReceived,
}

impl From<ConnectionError> for Event {
    fn from(x: ConnectionError) -> Self {
        Event::ConnectionLost { reason: x }
    }
}

struct PathResponse {
    /// The packet number the corresponding PATH_CHALLENGE was received in
    packet: u64,
    token: u64,
}

fn instant_saturating_sub(x: Instant, y: Instant) -> Duration {
    if x > y {
        x - y
    } else {
        Duration::new(0, 0)
    }
}

// Prevents overflow and improves behavior in extreme circumstances
const MAX_BACKOFF_EXPONENT: u32 = 16;
// Minimal remaining size to allow packet coalescing
const MIN_PACKET_SPACE: usize = 40;

/// Description of a particular network path
struct PathData {
    remote: SocketAddr,
    rtt: RttEstimator,
    /// Maximum number of bytes in flight that may be sent.
    congestion_window: u64,
    /// Slow start threshold in bytes. When the congestion window is below ssthresh, the mode is
    /// slow start and the window grows by the number of bytes acknowledged.
    ssthresh: u64,
    /// Whether we're enabling ECN on outgoing packets
    sending_ecn: bool,
}

/// Errors that can arise when sending a datagram
#[derive(Debug, Error, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum SendDatagramError {
    /// The peer does not support receiving datagram frames
    #[error(display = "datagrams not supported by peer")]
    UnsupportedByPeer,
    /// Datagram support is disabled locally
    #[error(display = "datagram support disabled")]
    Disabled,
    /// The datagram is larger than the connection can currently accommodate
    ///
    /// Indicates that the path MTU minus overhead or the limit advertised by the peer has been
    /// exceeded.
    #[error(display = "datagram too large")]
    TooLarge,
}

struct DatagramState {
    /// Number of bytes of datagrams that have been received by the local transport but not
    /// delivered to the application
    recv_buffered: usize,
    incoming: VecDeque<Datagram>,
    outgoing: VecDeque<Datagram>,
    outgoing_total: usize,
}

impl DatagramState {
    fn new() -> Self {
        Self {
            recv_buffered: 0,
            incoming: VecDeque::new(),
            outgoing: VecDeque::new(),
            outgoing_total: 0,
        }
    }
}

struct ZeroRttCrypto<S: crypto::Session> {
    header: S::HeaderKey,
    packet: S::PacketKey,
}
