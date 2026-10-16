//! snap/1 as an RLPx sub-protocol beside reth's native snap/2, so peers whose highest snap
//! version is snap/1 serve proofs too.
//!
//! RLPx keeps the highest version of a protocol both sides announce, so a peer announcing
//! snap/2 runs on reth's snap/2 session and this handler connects only to the others. Messages
//! 0x00 to 0x05 encode the same in both versions, so reth's snap message types frame the
//! requests and decode the replies. reth numbers snap/2 requests itself; chibi numbers snap/1
//! requests here, per connection. Inbound requests get an empty reply keeping their request id,
//! because peers drop sessions whose requests go unanswered.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use alloy_primitives::bytes::{BufMut, BytesMut};
use alloy_rlp::{Decodable, EMPTY_LIST_CODE, Encodable, Header};
use futures::{Stream, StreamExt};
use parking_lot::Mutex;
use reth_eth_wire::capability::SharedCapabilities;
use reth_eth_wire::multiplex::ProtocolConnection;
use reth_eth_wire::protocol::Protocol;
use reth_eth_wire_types::snap::{
    AccountRangeMessage, ByteCodesMessage, SnapProtocolMessage, StorageRangesMessage,
};
use reth_network::protocol::{ConnectionHandler, OnNotSupported, ProtocolHandler};
use reth_network_api::{Direction, PeerId};
use reth_network_p2p::error::{RequestError, RequestResult};
use reth_network_p2p::snap::client::SnapResponse;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, Sleep, sleep_until};
use tracing::debug;

use crate::devp2p::{SNAP_1, SnapReply};
use crate::peers::REQUEST_TIMEOUT;

const GET_ACCOUNT_RANGE: u8 = 0x00;
const ACCOUNT_RANGE: u8 = 0x01;
const GET_STORAGE_RANGES: u8 = 0x02;
const STORAGE_RANGES: u8 = 0x03;
const GET_BYTE_CODES: u8 = 0x04;
const BYTE_CODES: u8 = 0x05;
const GET_TRIE_NODES: u8 = 0x06;
const TRIE_NODES: u8 = 0x07;

/// snap/1 with its eight message ids, 0x00 to 0x07.
const PROTOCOL: Protocol = Protocol::new(SNAP_1, 8);

type Responder = oneshot::Sender<RequestResult<SnapResponse>>;

/// Closes when the connection is done with a request: answered, timed out, dropped unsent, or
/// failed by the connection closing.
pub(crate) type Retired = oneshot::Receiver<Infallible>;

/// A request for a snap/1 connection and its caller.
#[derive(Debug)]
struct Command {
    request: SnapProtocolMessage,
    waiting: Waiting,
}

/// Where a request's reply goes, and the handle whose drop retires it.
#[derive(Debug)]
struct Waiting {
    reply: Responder,
    _retire: oneshot::Sender<Infallible>,
}

/// Each peer's open snap/1 connections, by the serial each opened under.
type Registry = HashMap<PeerId, BTreeMap<u64, mpsc::UnboundedSender<Command>>>;

/// The open snap/1 connections and the handler reth opens them through.
#[derive(Clone, Debug, Default)]
pub(crate) struct Snap1 {
    connections: Arc<Mutex<Registry>>,
    opened: Arc<AtomicU64>,
}

impl Snap1 {
    /// Sends `request` on `peer`'s newest open snap/1 connection. The reply fails when `peer`
    /// has none or it closes before the peer answers, when the peer does not answer within
    /// [`REQUEST_TIMEOUT`], and for a request snap/1 lacks. A caller that drops the reply
    /// before the request is framed withdraws it.
    pub(crate) fn send(&self, peer: PeerId, request: SnapProtocolMessage) -> (SnapReply, Retired) {
        let (reply, replied) = oneshot::channel();
        let (retire, retired) = oneshot::channel();
        let command = Command {
            request,
            waiting: Waiting {
                reply,
                _retire: retire,
            },
        };
        if let Some(commands) = self
            .connections
            .lock()
            .get(&peer)
            .and_then(|open| open.values().rev().find(|commands| !commands.is_closed()))
        {
            // A closed connection drops the command, which fails the reply.
            commands.send(command).ok();
        }
        (replied, retired)
    }

    pub(crate) fn open<S>(&self, peer: PeerId, inbound: S) -> Connection<S> {
        let (commands_tx, commands) = mpsc::unbounded_channel();
        let serial = self.opened.fetch_add(1, Ordering::Relaxed);
        self.connections
            .lock()
            .entry(peer)
            .or_default()
            .insert(serial, commands_tx);
        Connection {
            inbound,
            commands,
            pending: HashMap::new(),
            deadlines: VecDeque::new(),
            timer: None,
            replies: VecDeque::new(),
            next_request: 0,
            peer,
            serial,
            registry: self.clone(),
        }
    }
}

impl ProtocolHandler for Snap1 {
    type ConnectionHandler = Self;

    fn on_incoming(&self, _: SocketAddr) -> Option<Self> {
        Some(self.clone())
    }

    fn on_outgoing(&self, _: SocketAddr, _: PeerId) -> Option<Self> {
        Some(self.clone())
    }
}

impl ConnectionHandler for Snap1 {
    type Connection = Connection<ProtocolConnection>;

    fn protocol(&self) -> Protocol {
        PROTOCOL
    }

    /// Keeps the session: a peer without snap/1 may speak snap/2, or serve eth only.
    fn on_unsupported_by_peer(
        self,
        _: &SharedCapabilities,
        _: Direction,
        _: PeerId,
    ) -> OnNotSupported {
        OnNotSupported::KeepAlive
    }

    fn into_connection(
        self,
        _: Direction,
        peer: PeerId,
        conn: ProtocolConnection,
    ) -> Self::Connection {
        debug!(target: "chibi::devp2p", %peer, "snap/1 connection opened");
        self.open(peer, conn)
    }
}

/// One peer's snap/1 connection: a stream of the frames to send, fed by `inbound`, the frames
/// the peer sent, and by the commands [`Snap1::send`] queues. reth closes the connection when
/// the stream ends, and the stream ends when `inbound` does.
#[derive(Debug)]
pub(crate) struct Connection<S> {
    inbound: S,
    commands: mpsc::UnboundedReceiver<Command>,
    /// Callers waiting for a reply, by the request id chibi gave their request. An entry stays
    /// until the peer answers or [`REQUEST_TIMEOUT`] passes, even once its caller stops waiting.
    pending: HashMap<u64, Waiting>,
    /// When each request sent times out, oldest first.
    deadlines: VecDeque<(Instant, u64)>,
    timer: Option<Pin<Box<Sleep>>>,
    /// Empty replies to the peer's requests, not yet sent.
    replies: VecDeque<BytesMut>,
    next_request: u64,
    peer: PeerId,
    serial: u64,
    registry: Snap1,
}

impl<S> Connection<S> {
    /// Handles a frame from the peer: queues the empty reply to a request, hands a response to
    /// the caller waiting for its request id, and drops anything else.
    fn receive(&mut self, frame: &[u8]) {
        let Some((&id, body)) = frame.split_first() else {
            return;
        };
        let (reply, lists) = match id {
            GET_ACCOUNT_RANGE => (ACCOUNT_RANGE, 2),
            GET_STORAGE_RANGES => (STORAGE_RANGES, 2),
            GET_BYTE_CODES => (BYTE_CODES, 1),
            GET_TRIE_NODES => (TRIE_NODES, 1),
            _ => {
                if let Some((request_id, response)) = response(id, body)
                    && let Some(waiting) = self.pending.remove(&request_id)
                {
                    waiting.reply.send(Ok(response)).ok();
                }
                return;
            }
        };
        if let Some(request_id) = request_id(body) {
            self.replies
                .push_back(empty_reply(reply, request_id, lists));
        }
    }

    /// Fails every pending request whose deadline passed with a timeout, and arms the timer
    /// for the next.
    fn expire(&mut self, cx: &mut Context<'_>) {
        while let Some(&(deadline, request_id)) = self.deadlines.front() {
            if self.pending.contains_key(&request_id) {
                let timer = self
                    .timer
                    .get_or_insert_with(|| Box::pin(sleep_until(deadline)));
                if timer.deadline() != deadline {
                    timer.as_mut().reset(deadline);
                }
                if timer.as_mut().poll(cx).is_pending() {
                    return;
                }
            }
            self.deadlines.pop_front();
            if let Some(waiting) = self.pending.remove(&request_id) {
                waiting.reply.send(Err(RequestError::Timeout)).ok();
            }
        }
    }

    /// Frames `command`'s request under the next request id and keeps its caller waiting.
    fn request(&mut self, command: Command) -> Option<BytesMut> {
        let Command {
            mut request,
            waiting,
        } = command;
        let request_id = self.next_request;
        let frame = match &mut request {
            SnapProtocolMessage::GetAccountRange(message) => {
                message.request_id = request_id;
                frame(GET_ACCOUNT_RANGE, message)
            }
            SnapProtocolMessage::GetStorageRanges(message) => {
                message.request_id = request_id;
                frame(GET_STORAGE_RANGES, message)
            }
            SnapProtocolMessage::GetByteCodes(message) => {
                message.request_id = request_id;
                frame(GET_BYTE_CODES, message)
            }
            SnapProtocolMessage::GetBlockAccessLists(_)
            | SnapProtocolMessage::AccountRange(_)
            | SnapProtocolMessage::StorageRanges(_)
            | SnapProtocolMessage::ByteCodes(_)
            | SnapProtocolMessage::BlockAccessLists(_) => {
                waiting
                    .reply
                    .send(Err(RequestError::UnsupportedCapability))
                    .ok();
                return None;
            }
        };
        self.next_request = request_id.wrapping_add(1);
        self.pending.insert(request_id, waiting);
        self.deadlines
            .push_back((Instant::now() + REQUEST_TIMEOUT, request_id));
        Some(frame)
    }
}

impl<S: Stream<Item = BytesMut> + Unpin> Stream for Connection<S> {
    type Item = BytesMut;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<BytesMut>> {
        let this = self.get_mut();
        loop {
            match this.inbound.poll_next_unpin(cx) {
                Poll::Ready(Some(frame)) => this.receive(&frame),
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => break,
            }
        }
        if let Some(reply) = this.replies.pop_front() {
            return Poll::Ready(Some(reply));
        }
        this.expire(cx);
        while let Poll::Ready(Some(command)) = this.commands.poll_recv(cx) {
            if command.waiting.reply.is_closed() {
                continue;
            }
            if let Some(frame) = this.request(command) {
                return Poll::Ready(Some(frame));
            }
        }
        Poll::Pending
    }
}

impl<S> Drop for Connection<S> {
    fn drop(&mut self) {
        let mut connections = self.registry.connections.lock();
        if let Some(open) = connections.get_mut(&self.peer) {
            open.remove(&self.serial);
            if open.is_empty() {
                connections.remove(&self.peer);
            }
        }
        drop(connections);
        self.commands.close();
        while let Ok(command) = self.commands.try_recv() {
            command
                .waiting
                .reply
                .send(Err(RequestError::ConnectionDropped))
                .ok();
        }
        for (_, waiting) in self.pending.drain() {
            waiting
                .reply
                .send(Err(RequestError::ConnectionDropped))
                .ok();
        }
    }
}

fn frame(id: u8, message: &impl Encodable) -> BytesMut {
    let mut frame = BytesMut::with_capacity(1 + message.length());
    frame.put_u8(id);
    message.encode(&mut frame);
    frame
}

/// The response a frame with message id `id` carries, with its request id, if it is a
/// well-formed snap/1 response.
fn response(id: u8, mut body: &[u8]) -> Option<(u64, SnapResponse)> {
    match id {
        ACCOUNT_RANGE => {
            let message = AccountRangeMessage::decode(&mut body).ok()?;
            Some((message.request_id, SnapResponse::AccountRange(message)))
        }
        STORAGE_RANGES => {
            let message = StorageRangesMessage::decode(&mut body).ok()?;
            Some((message.request_id, SnapResponse::StorageRanges(message)))
        }
        BYTE_CODES => {
            let message = ByteCodesMessage::decode(&mut body).ok()?;
            Some((message.request_id, SnapResponse::ByteCodes(message)))
        }
        _ => None,
    }
}

/// The request id every snap request body starts with.
fn request_id(mut body: &[u8]) -> Option<u64> {
    Header::decode(&mut body)
        .ok()
        .filter(|header| header.list)?;
    u64::decode(&mut body).ok()
}

/// The reply `id` to request `request_id` holding nothing: its request id and `lists` empty
/// lists.
fn empty_reply(id: u8, request_id: u64, lists: usize) -> BytesMut {
    let header = Header {
        list: true,
        payload_length: request_id.length() + lists,
    };
    let mut frame = BytesMut::with_capacity(1 + header.length_with_payload());
    frame.put_u8(id);
    header.encode(&mut frame);
    request_id.encode(&mut frame);
    frame.put_bytes(EMPTY_LIST_CODE, lists);
    frame
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::task::Waker;
    use std::time::Duration;

    use alloy_primitives::{B256, B512};
    use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
    use reth_eth_wire_types::snap::{
        GetAccountRangeMessage, GetBlockAccessListsMessage, GetByteCodesMessage,
        GetStorageRangesMessage,
    };
    use tokio::sync::oneshot::error::TryRecvError;

    use super::*;
    use crate::peers::REQUEST_TIMEOUT;

    const PEER: PeerId = B512::repeat_byte(7);
    const RESPONSE_BYTES: u64 = 512 * 1024;

    type Wire = Connection<UnboundedReceiver<BytesMut>>;

    fn connect(snap1: &Snap1) -> (UnboundedSender<BytesMut>, Wire) {
        let (to_chibi, inbound) = unbounded();
        (to_chibi, snap1.open(PEER, inbound))
    }

    fn poll(connection: &mut Wire) -> Poll<Option<Vec<u8>>> {
        connection
            .poll_next_unpin(&mut Context::from_waker(Waker::noop()))
            .map(|frame| frame.map(|frame| frame.to_vec()))
    }

    /// Every frame the connection sends before it has nothing more to send.
    fn drain(connection: &mut Wire) -> Vec<Vec<u8>> {
        let mut sent = Vec::new();
        while let Poll::Ready(frame) = poll(connection) {
            sent.push(frame.expect("the connection stays open"));
        }
        sent
    }

    fn receive(to_chibi: &UnboundedSender<BytesMut>, frame: &[u8]) {
        to_chibi.unbounded_send(BytesMut::from(frame)).unwrap();
    }

    fn account_message(request_id: u64) -> GetAccountRangeMessage {
        GetAccountRangeMessage {
            request_id,
            root_hash: B256::repeat_byte(0x11),
            starting_hash: B256::repeat_byte(0x22),
            limit_hash: B256::repeat_byte(0x33),
            response_bytes: RESPONSE_BYTES,
        }
    }

    fn account_request() -> SnapProtocolMessage {
        SnapProtocolMessage::GetAccountRange(account_message(99))
    }

    fn storage_message(request_id: u64) -> GetStorageRangesMessage {
        GetStorageRangesMessage {
            request_id,
            root_hash: B256::repeat_byte(0x11),
            account_hashes: vec![B256::repeat_byte(0x22)],
            starting_hash: B256::ZERO.into(),
            limit_hash: B256::repeat_byte(0xff).into(),
            response_bytes: RESPONSE_BYTES,
        }
    }

    fn code_message(request_id: u64) -> GetByteCodesMessage {
        GetByteCodesMessage {
            request_id,
            hashes: vec![B256::repeat_byte(0x44)],
            response_bytes: RESPONSE_BYTES,
        }
    }

    fn empty_accounts(request_id: u64) -> SnapResponse {
        SnapResponse::AccountRange(AccountRangeMessage {
            request_id,
            accounts: Vec::new(),
            proof: Vec::new(),
        })
    }

    #[tokio::test]
    async fn requests_are_framed_with_the_ids_chibi_assigns() {
        let snap1 = Snap1::default();
        let (_to_chibi, mut connection) = connect(&snap1);
        let _replies = [
            snap1.send(PEER, account_request()),
            snap1.send(PEER, account_request()),
            snap1.send(
                PEER,
                SnapProtocolMessage::GetStorageRanges(storage_message(99)),
            ),
            snap1.send(PEER, SnapProtocolMessage::GetByteCodes(code_message(99))),
        ];
        let account_range = |request_id: u8| {
            [
                &[0x00, 0xf8, 0x68, request_id, 0xa0][..],
                &[0x11; 32],
                &[0xa0],
                &[0x22; 32],
                &[0xa0],
                &[0x33; 32],
                &[0x83, 0x08, 0x00, 0x00],
            ]
            .concat()
        };
        assert_eq!(
            account_range(0x80),
            [&[0x00][..], &alloy_rlp::encode(account_message(0))].concat()
        );
        assert_eq!(
            drain(&mut connection),
            [
                account_range(0x80),
                account_range(0x01),
                [&[0x02][..], &alloy_rlp::encode(storage_message(2))].concat(),
                [&[0x04][..], &alloy_rlp::encode(code_message(3))].concat(),
            ]
        );
        assert_eq!(
            connection.pending.keys().copied().collect::<HashSet<_>>(),
            HashSet::from([0, 1, 2, 3])
        );
    }

    #[test]
    fn a_request_snap1_lacks_fails_without_reaching_the_peer() {
        let snap1 = Snap1::default();
        let (_to_chibi, mut connection) = connect(&snap1);
        let mut reply = snap1
            .send(
                PEER,
                SnapProtocolMessage::GetBlockAccessLists(GetBlockAccessListsMessage {
                    request_id: 0,
                    block_hashes: vec![B256::repeat_byte(0x55)],
                    response_bytes: RESPONSE_BYTES,
                }),
            )
            .0;
        assert_eq!(drain(&mut connection), Vec::<Vec<u8>>::new());
        assert_eq!(
            reply.try_recv(),
            Ok(Err(RequestError::UnsupportedCapability))
        );
        assert!(connection.pending.is_empty());
    }

    #[tokio::test]
    async fn replies_reach_their_callers_by_request_id() {
        let snap1 = Snap1::default();
        let (to_chibi, mut connection) = connect(&snap1);
        let mut first = snap1.send(PEER, account_request()).0;
        let mut second = snap1.send(PEER, account_request()).0;
        assert_eq!(drain(&mut connection).len(), 2);

        receive(&to_chibi, &[0x01, 0xc3, 0x01, 0xc0, 0xc0]);
        assert_eq!(drain(&mut connection), Vec::<Vec<u8>>::new());
        assert_eq!(second.try_recv(), Ok(Ok(empty_accounts(1))));
        assert_eq!(first.try_recv(), Err(TryRecvError::Empty));

        receive(&to_chibi, &[0x01, 0xc3, 0x80, 0xc0, 0xc0]);
        drain(&mut connection);
        assert_eq!(first.try_recv(), Ok(Ok(empty_accounts(0))));
        assert!(connection.pending.is_empty());
    }

    #[tokio::test]
    async fn replies_to_unknown_ids_and_garbage_frames_are_dropped() {
        let snap1 = Snap1::default();
        let (to_chibi, mut connection) = connect(&snap1);
        let mut waiting = snap1.send(PEER, account_request()).0;
        drain(&mut connection);

        let dropped: [&[u8]; 12] = [
            &[],
            &[0x01],
            &[0x01, 0xc3, 0x05, 0xc0, 0xc0],
            &[0x01, 0xff],
            &[0x01, 0xc3, 0x80, 0xc0],
            &[0x03, 0xc1],
            &[0x05, 0xc2, 0x80, 0xc1],
            &[0x07, 0xc2, 0x80, 0xc0],
            &[0x08, 0xc2, 0x80, 0xc0],
            &[0xff, 0xc3, 0x80, 0xc0, 0xc0],
            &[0x00, 0x05],
            &[0x04, 0xc0],
        ];
        for frame in dropped {
            receive(&to_chibi, frame);
        }
        assert_eq!(drain(&mut connection), Vec::<Vec<u8>>::new());
        assert_eq!(waiting.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(connection.pending.len(), 1);

        receive(&to_chibi, &[0x01, 0xc3, 0x80, 0xc0, 0xc0]);
        drain(&mut connection);
        assert_eq!(waiting.try_recv(), Ok(Ok(empty_accounts(0))));
    }

    #[test]
    fn every_inbound_request_gets_an_empty_reply_keeping_its_id() {
        let snap1 = Snap1::default();
        let (to_chibi, mut connection) = connect(&snap1);
        let trie_nodes = [
            &[0x06, 0xe7, 0x0a, 0xa0][..],
            &[0x11; 32],
            &[0xc0, 0x83, 0x08, 0x00, 0x00],
        ]
        .concat();
        receive(
            &to_chibi,
            &[&[0x00][..], &alloy_rlp::encode(account_message(7))].concat(),
        );
        receive(
            &to_chibi,
            &[&[0x02][..], &alloy_rlp::encode(storage_message(8))].concat(),
        );
        receive(
            &to_chibi,
            &[&[0x04][..], &alloy_rlp::encode(code_message(0x0102))].concat(),
        );
        receive(&to_chibi, &trie_nodes);

        let replies = [
            vec![0x01, 0xc3, 0x07, 0xc0, 0xc0],
            vec![0x03, 0xc3, 0x08, 0xc0, 0xc0],
            vec![0x05, 0xc4, 0x82, 0x01, 0x02, 0xc0],
            vec![0x07, 0xc2, 0x0a, 0xc0],
        ];
        assert_eq!(drain(&mut connection), replies);
        assert_eq!(
            [
                frame(
                    ACCOUNT_RANGE,
                    &AccountRangeMessage {
                        request_id: 7,
                        accounts: Vec::new(),
                        proof: Vec::new(),
                    }
                ),
                frame(
                    STORAGE_RANGES,
                    &StorageRangesMessage {
                        request_id: 8,
                        slots: Vec::new(),
                        proof: Vec::new(),
                    }
                ),
                frame(
                    BYTE_CODES,
                    &ByteCodesMessage {
                        request_id: 0x0102,
                        codes: Vec::new(),
                    }
                ),
            ]
            .map(|frame| frame.to_vec()),
            replies[..3],
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_sent_request_whose_caller_gave_up_stays_pending_until_it_times_out() {
        let snap1 = Snap1::default();
        let (_to_chibi, mut connection) = connect(&snap1);
        let (abandoned, mut retired) = snap1.send(PEER, account_request());
        drain(&mut connection);
        drop(abandoned);
        drain(&mut connection);
        assert_eq!(connection.pending.len(), 1);
        assert_eq!(retired.try_recv(), Err(TryRecvError::Empty));

        tokio::time::advance(REQUEST_TIMEOUT).await;
        drain(&mut connection);
        assert!(connection.pending.is_empty());
        assert_eq!(retired.try_recv(), Err(TryRecvError::Closed));
    }

    #[tokio::test]
    async fn closing_the_session_fails_every_pending_and_queued_caller() {
        let snap1 = Snap1::default();
        let (to_chibi, mut connection) = connect(&snap1);
        let (mut sent, mut sent_retired) = snap1.send(PEER, account_request());
        drain(&mut connection);
        let mut queued = snap1.send(PEER, account_request()).0;

        drop(to_chibi);
        assert_eq!(poll(&mut connection), Poll::Ready(None));
        drop(connection);
        assert_eq!(sent.try_recv(), Ok(Err(RequestError::ConnectionDropped)));
        assert_eq!(queued.try_recv(), Ok(Err(RequestError::ConnectionDropped)));
        assert_eq!(sent_retired.try_recv(), Err(TryRecvError::Closed));
        let mut late = snap1.send(PEER, account_request()).0;
        assert_eq!(late.try_recv(), Err(TryRecvError::Closed));
    }

    #[tokio::test]
    async fn an_older_connection_closing_keeps_the_newer_one_registered() {
        let snap1 = Snap1::default();
        let (_old_wire, old) = connect(&snap1);
        let (_new_wire, mut new) = connect(&snap1);
        drop(old);
        let _reply = snap1.send(PEER, account_request());
        assert_eq!(drain(&mut new).len(), 1);
    }

    #[tokio::test]
    async fn a_second_connection_closing_keeps_the_first_one_registered() {
        let snap1 = Snap1::default();
        let (_first_wire, mut first) = connect(&snap1);
        let (_second_wire, second) = connect(&snap1);
        drop(second);
        let _reply = snap1.send(PEER, account_request());
        assert_eq!(drain(&mut first).len(), 1);
    }

    #[tokio::test]
    async fn a_command_whose_caller_gave_up_before_it_was_sent_is_dropped() {
        let snap1 = Snap1::default();
        let (_to_chibi, mut connection) = connect(&snap1);
        drop(snap1.send(PEER, account_request()));
        let _kept = snap1.send(PEER, account_request());
        assert_eq!(
            drain(&mut connection),
            [[&[0x00][..], &alloy_rlp::encode(account_message(0))].concat()]
        );
        assert_eq!(connection.pending.keys().copied().collect::<Vec<_>>(), [0]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_the_peer_leaves_unanswered_fails_at_the_timeout() {
        let snap1 = Snap1::default();
        let (_to_chibi, mut connection) = connect(&snap1);
        let mut reply = snap1.send(PEER, account_request()).0;
        drain(&mut connection);

        tokio::time::advance(REQUEST_TIMEOUT - Duration::from_millis(1)).await;
        drain(&mut connection);
        assert_eq!(reply.try_recv(), Err(TryRecvError::Empty));

        tokio::time::advance(Duration::from_millis(1)).await;
        drain(&mut connection);
        assert_eq!(reply.try_recv(), Ok(Err(RequestError::Timeout)));
        assert!(connection.pending.is_empty());
    }
}
