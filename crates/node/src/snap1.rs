//! snap/1 as an RLPx sub-protocol beside reth's native snap/2, so peers whose highest snap
//! version is snap/1 serve proofs too.

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::atomic::AtomicBool;
    use std::task::{Wake, Waker};

    use alloy_primitives::{B256, B512};
    use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
    use reth_eth_wire_types::snap::{
        GetAccountRangeMessage, GetBlockAccessListsMessage, GetByteCodesMessage,
        GetStorageRangesMessage,
    };
    use tokio::sync::oneshot::error::TryRecvError;

    use super::*;

    const PEER: PeerId = B512::repeat_byte(7);
    const RESPONSE_BYTES: u64 = 512 * 1024;

    type Wire = Connection<UnboundedReceiver<BytesMut>>;

    fn connect(snap1: &Snap1) -> (UnboundedSender<BytesMut>, Wire) {
        let (to_chibi, inbound) = unbounded();
        (to_chibi, snap1.open(PEER, inbound))
    }

    fn poll_with(connection: &mut Wire, waker: &Waker) -> Poll<Option<Vec<u8>>> {
        connection
            .poll_next_unpin(&mut Context::from_waker(waker))
            .map(|frame| frame.map(|frame| frame.to_vec()))
    }

    fn poll(connection: &mut Wire) -> Poll<Option<Vec<u8>>> {
        poll_with(connection, Waker::noop())
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

    #[test]
    fn requests_are_framed_with_the_ids_chibi_assigns() {
        let snap1 = Snap1::default();
        let (_to_chibi, mut connection) = connect(&snap1);
        let _replies = [
            snap1.send(PEER, account_request()),
            snap1.send(PEER, account_request()),
            snap1.send(PEER, SnapProtocolMessage::GetStorageRanges(storage_message(99))),
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
        let mut reply = snap1.send(
            PEER,
            SnapProtocolMessage::GetBlockAccessLists(GetBlockAccessListsMessage {
                request_id: 0,
                block_hashes: vec![B256::repeat_byte(0x55)],
                response_bytes: RESPONSE_BYTES,
            }),
        );
        assert_eq!(drain(&mut connection), Vec::<Vec<u8>>::new());
        assert_eq!(
            reply.try_recv(),
            Ok(Err(RequestError::UnsupportedCapability))
        );
        assert!(connection.pending.is_empty());
    }

    #[test]
    fn replies_reach_their_callers_by_request_id() {
        let snap1 = Snap1::default();
        let (to_chibi, mut connection) = connect(&snap1);
        let mut first = snap1.send(PEER, account_request());
        let mut second = snap1.send(PEER, account_request());
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

    #[test]
    fn replies_to_unknown_ids_and_garbage_frames_are_dropped() {
        let snap1 = Snap1::default();
        let (to_chibi, mut connection) = connect(&snap1);
        let mut waiting = snap1.send(PEER, account_request());
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
        let trie_nodes = [&[0x06, 0xe7, 0x0a, 0xa0][..], &[0x11; 32], &[0xc0, 0x83, 0x08, 0x00, 0x00]]
            .concat();
        receive(&to_chibi, &[&[0x00][..], &alloy_rlp::encode(account_message(7))].concat());
        receive(&to_chibi, &[&[0x02][..], &alloy_rlp::encode(storage_message(8))].concat());
        receive(&to_chibi, &[&[0x04][..], &alloy_rlp::encode(code_message(0x0102))].concat());
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
                frame(ACCOUNT_RANGE, &AccountRangeMessage {
                    request_id: 7,
                    accounts: Vec::new(),
                    proof: Vec::new(),
                }),
                frame(STORAGE_RANGES, &StorageRangesMessage {
                    request_id: 8,
                    slots: Vec::new(),
                    proof: Vec::new(),
                }),
                frame(BYTE_CODES, &ByteCodesMessage {
                    request_id: 0x0102,
                    codes: Vec::new(),
                }),
            ]
            .map(|frame| frame.to_vec()),
            replies[..3],
        );
    }

    #[derive(Default)]
    struct Woken(AtomicBool);

    impl Wake for Woken {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn a_caller_that_stops_waiting_leaves_no_pending_entry() {
        let snap1 = Snap1::default();
        let (_to_chibi, mut connection) = connect(&snap1);
        let abandoned = snap1.send(PEER, account_request());
        let _kept = snap1.send(PEER, account_request());
        drain(&mut connection);

        let woken = Arc::new(Woken::default());
        assert_eq!(
            poll_with(&mut connection, &Waker::from(Arc::clone(&woken))),
            Poll::Pending
        );
        assert_eq!(connection.pending.len(), 2);
        drop(abandoned);
        assert!(woken.0.load(Ordering::SeqCst));
        drain(&mut connection);
        assert_eq!(connection.pending.keys().copied().collect::<Vec<_>>(), [1]);
    }

    #[test]
    fn closing_the_session_fails_every_pending_and_queued_caller() {
        let snap1 = Snap1::default();
        let (to_chibi, mut connection) = connect(&snap1);
        let mut sent = snap1.send(PEER, account_request());
        drain(&mut connection);
        let mut queued = snap1.send(PEER, account_request());

        drop(to_chibi);
        assert_eq!(poll(&mut connection), Poll::Ready(None));
        drop(connection);
        assert_eq!(sent.try_recv(), Ok(Err(RequestError::ConnectionDropped)));
        assert_eq!(queued.try_recv(), Ok(Err(RequestError::ConnectionDropped)));
        let mut late = snap1.send(PEER, account_request());
        assert_eq!(late.try_recv(), Err(TryRecvError::Closed));
    }

    #[test]
    fn an_older_connection_closing_keeps_the_newer_one_registered() {
        let snap1 = Snap1::default();
        let (_old_wire, old) = connect(&snap1);
        let (_new_wire, mut new) = connect(&snap1);
        drop(old);
        let _reply = snap1.send(PEER, account_request());
        assert_eq!(drain(&mut new).len(), 1);
    }
}
