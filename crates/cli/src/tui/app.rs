//! The TUI's screens as a state machine: keys, start-up progress, peer counts and node
//! results in; what the event loop should do out. No terminal and no network here.

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, b256};

    use super::*;

    const SENDER: Address = address!("0x17c5185167401ed00cf5f5b2fc97d9bbfdb7d025");
    const BOB: &str = "0x0000000000000000000000000000000000000b0b";
    const TOKEN: Address = address!("0x00000000000000000000000000000000000070c3");
    const TKN: Token = Token {
        symbol: "TKN",
        name: "Test token",
        address: TOKEN,
        decimals: 6,
    };
    const REGISTRY: &[Token] = &[TKN];
    const SEPOLIA_ID: u64 = 11_155_111;
    const FEE: Fee = Fee {
        gas_limit: 21_000,
        max_fee_per_gas: 2_750_000_000,
        max_priority_fee_per_gas: 1_000_000_000,
    };

    fn press(app: &mut App, code: KeyCode) -> Option<Action> {
        app.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    fn eth(milli: u64) -> U256 {
        U256::from(milli) * U256::from(1_000_000_000_000_000u64)
    }

    /// Unlocked, serving with a state peer, holding 3 ETH and 1.5 TKN.
    fn ready(network: Network) -> App {
        let mut app = App::new(network, SEPOLIA_ID, REGISTRY, false);
        app.unlocked(Ok(SENDER));
        app.startup(Startup::Serving);
        app.health(100, 1);
        app.balances(Balances {
            eth: Some(eth(3_000)),
            tokens: vec![(TKN, U256::from(1_500_000))],
        });
        app
    }

    /// Fills the form, answers its estimate with `fee`, and presses Enter.
    fn submit(app: &mut App, amount: &str, to: &str, fee: Result<Fee, String>) {
        app.form.field = Field::Amount;
        typed(app, amount);
        app.form.field = Field::To;
        typed(app, to);
        if let Some(draft) = app.estimate_request() {
            app.estimated(draft, app.head, fee);
        }
        press(app, KeyCode::Enter);
    }

    #[test]
    fn unlock_then_wait_then_perk_up_then_serve() {
        let mut app = App::new(Network::Sepolia, SEPOLIA_ID, REGISTRY, false);
        assert_eq!(app.screen, Screen::Unlock);
        assert_eq!(press(&mut app, KeyCode::Enter), None);
        typed(&mut app, "qlx");
        assert_eq!(
            press(&mut app, KeyCode::Enter),
            Some(Action::Unlock("qlx".to_owned()))
        );
        assert!(app.unlocking && app.password.is_empty());
        app.unlocked(Err("wrong password".to_owned()));
        assert_eq!(
            (app.screen, app.unlock_error.as_deref()),
            (Screen::Unlock, Some("wrong password, try again"))
        );

        typed(&mut app, "hunter2");
        press(&mut app, KeyCode::Enter);
        app.unlocked(Ok(SENDER));
        assert_eq!((app.screen, app.sender), (Screen::Waiting, Some(SENDER)));

        app.startup(Startup::Waiting {
            peers: 7,
            state_peers: 0,
            elapsed: Duration::from_secs(80),
        });
        assert_eq!(
            (app.screen, app.waiting),
            (
                Screen::Waiting,
                WaitCounts {
                    peers: 7,
                    state_peers: 0,
                    elapsed: Duration::from_secs(80),
                }
            )
        );
        app.startup(Startup::FoundStatePeer);
        assert_eq!(app.screen, Screen::Excited);
        app.startup(Startup::Serving);
        assert_eq!(app.screen, Screen::Ready);
    }

    #[test]
    fn losing_every_state_peer_pauses_the_form_and_keeps_its_input() {
        let mut app = ready(Network::Sepolia);
        typed(&mut app, "1.5");
        app.health(101, 0);
        assert_eq!(app.screen, Screen::Paused);
        assert_eq!(press(&mut app, KeyCode::Enter), None);
        assert_eq!(app.screen, Screen::Paused);
        assert_eq!(app.estimate_request(), None);
        app.health(102, 2);
        assert_eq!((app.screen, app.form.amount.as_str()), (Screen::Ready, "1.5"));

        submit(&mut app, "", BOB, Ok(FEE));
        assert_eq!(app.screen, Screen::Review);
        app.health(103, 0);
        assert_eq!((app.screen, app.review), (Screen::Paused, None));
        assert_eq!(press(&mut app, KeyCode::Char('y')), None);
        app.health(104, 1);
        assert_eq!(app.screen, Screen::Ready);
    }

    #[test]
    fn review_shows_the_transfer_and_y_sends_it() {
        let mut app = ready(Network::Sepolia);
        submit(&mut app, "1.25", BOB, Ok(FEE));
        let draft = Draft {
            asset: Asset::Eth,
            to: parse_address(BOB).unwrap(),
            amount: eth(1_250),
        };
        assert_eq!(app.screen, Screen::Review);
        assert_eq!(
            app.review,
            Some(Review {
                draft,
                fee: FEE,
                refused: None,
            })
        );
        assert_eq!(press(&mut app, KeyCode::Char('n')), None);
        assert_eq!(app.screen, Screen::Ready);
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            press(&mut app, KeyCode::Char('y')),
            Some(Action::Send(draft, FEE))
        );
        assert_eq!(app.screen, Screen::Sending);
    }

    #[test]
    fn mainnet_shows_balances_but_review_refuses_to_send() {
        let mut app = ready(Network::Mainnet);
        submit(&mut app, "1", BOB, Ok(FEE));
        assert_eq!(app.screen, Screen::Review);
        assert_eq!(
            app.review.and_then(|review| review.refused),
            Some("sending on mainnet is not enabled yet")
        );
        assert_eq!(press(&mut app, KeyCode::Char('y')), None);
        assert_eq!(app.screen, Screen::Review);
    }

    fn errors(app: &App) -> [Option<&str>; 3] {
        [
            app.form.amount_error.as_deref(),
            app.form.to_error.as_deref(),
            app.form.fee_error.as_deref(),
        ]
    }

    #[test]
    fn each_field_says_why_it_cannot_be_sent_and_keeps_its_input() {
        let checksum = "0x7f02C3E3c98b133055B8B348B2Ac625669Ed295d";
        for (asset, amount, to, expected) in [
            (
                Asset::Eth,
                "1",
                checksum,
                [
                    None,
                    Some(
                        "the checksum does not match; check the address, or type it all lowercase",
                    ),
                    None,
                ],
            ),
            (
                Asset::Eth,
                "1",
                "0x0000000000000000000000000000000000000000",
                [None, Some("refusing to send to the zero address"), None],
            ),
            (
                Asset::Eth,
                "1",
                "0x123",
                [None, Some("expected 0x followed by 40 hex digits"), None],
            ),
            (
                Asset::Eth,
                "",
                "",
                [
                    Some("enter an amount"),
                    Some("enter the recipient's address"),
                    None,
                ],
            ),
            (
                Asset::Token(TKN),
                "0.0000001",
                BOB,
                [Some("TKN has at most 6 decimals"), None, None],
            ),
            (
                Asset::Token(TKN),
                "0",
                BOB,
                [Some("the amount must be more than 0"), None, None],
            ),
            (
                Asset::Token(TKN),
                "1.6",
                BOB,
                [Some("more than your balance of 1.50 TKN"), None, None],
            ),
            (
                Asset::Eth,
                "3.1",
                BOB,
                [Some("more than your balance of 3.00 ETH"), None, None],
            ),
            (
                Asset::Eth,
                "3",
                BOB,
                [
                    Some(
                        "the amount plus the max fee 0.00005775 ETH is more than your balance of 3.00 ETH",
                    ),
                    None,
                    None,
                ],
            ),
            (
                Asset::Eth,
                "1.2.3",
                BOB,
                [Some("not a decimal amount"), None, None],
            ),
        ] {
            let mut app = ready(Network::Sepolia);
            app.form.asset = asset;
            submit(&mut app, amount, to, Ok(FEE));
            assert_eq!(
                (app.screen, errors(&app)),
                (Screen::Ready, expected),
                "{amount} to {to}"
            );
            assert_eq!(
                (app.form.amount.as_str(), app.form.to.as_str()),
                (amount, to)
            );
        }
    }

    #[test]
    fn a_reverting_or_unknown_fee_blocks_review() {
        let mut app = ready(Network::Sepolia);
        app.form.asset = Asset::Token(TKN);
        submit(
            &mut app,
            "1",
            BOB,
            Err("the transfer reverts: paused".to_owned()),
        );
        assert_eq!(
            (app.screen, errors(&app)),
            (
                Screen::Ready,
                [None, None, Some("the transfer reverts: paused")]
            )
        );

        let mut app = ready(Network::Sepolia);
        typed(&mut app, "1");
        app.form.field = Field::To;
        typed(&mut app, BOB);
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            errors(&app),
            [None, None, Some("waiting for the fee estimate")]
        );
    }

    #[test]
    fn max_sends_the_token_balance_or_the_eth_balance_less_the_max_fee() {
        let mut app = ready(Network::Sepolia);
        app.form.asset = Asset::Token(TKN);
        submit(&mut app, "max", BOB, Ok(FEE));
        assert_eq!(
            app.review.map(|review| review.draft.amount),
            Some(U256::from(1_500_000))
        );

        let mut app = ready(Network::Sepolia);
        submit(&mut app, "max", BOB, Ok(FEE));
        assert_eq!(
            app.review.map(|review| review.draft.amount),
            Some(U256::from(2_999_942_250_000_000_000u64))
        );

        let mut app = ready(Network::Sepolia);
        app.balances(Balances {
            eth: Some(U256::from(1_000)),
            tokens: Vec::new(),
        });
        submit(&mut app, "max", BOB, Ok(FEE));
        assert_eq!(
            errors(&app),
            [
                Some("your balance of 0.000000000000001 ETH does not cover the max fee 0.00005775 ETH"),
                None,
                None
            ]
        );
    }

    #[test]
    fn a_token_sent_to_its_own_contract_warns_without_blocking() {
        let mut app = ready(Network::Sepolia);
        press(&mut app, KeyCode::Up);
        assert_eq!(app.form.field, Field::Token);
        press(&mut app, KeyCode::Right);
        assert_eq!(app.form.asset, Asset::Token(TKN));
        press(&mut app, KeyCode::Right);
        assert_eq!(app.form.asset, Asset::Eth);
        press(&mut app, KeyCode::Left);
        assert_eq!(app.form.asset, Asset::Token(TKN));
        submit(
            &mut app,
            "1",
            "0x00000000000000000000000000000000000070c3",
            Ok(FEE),
        );
        assert_eq!(
            app.to_warning().as_deref(),
            Some("this is the TKN contract itself; tokens sent to it are usually lost")
        );
        assert_eq!(app.screen, Screen::Review);
    }

    #[test]
    fn a_sent_transfer_is_tracked_to_its_verified_receipt() {
        let hash = b256!("0x1111111111111111111111111111111111111111111111111111111111111111");
        let mut app = ready(Network::Sepolia);
        submit(&mut app, "1", BOB, Ok(FEE));
        press(&mut app, KeyCode::Char('y'));
        assert_eq!(app.tracking(), None);
        app.sent(Ok(hash));
        assert_eq!(app.tracking(), Some(hash));
        app.tracked(Tracked::Pending);
        app.health(109, 1);
        assert!(!app.sending.as_ref().unwrap().still_pending());
        app.health(110, 1);
        assert!(app.sending.as_ref().unwrap().still_pending());

        assert_eq!(press(&mut app, KeyCode::Char('q')), None);
        assert_eq!(
            app.notice.as_deref(),
            Some(
                "a transfer is still pending; chibi's outbox rebroadcasts it on the next run. press q again to quit"
            )
        );
        app.tracked(Tracked::Included);
        app.tracked(Tracked::Receipt { success: true });
        assert_eq!(
            app.sending.as_ref().map(|s| &s.state),
            Some(&SendState::Succeeded)
        );
        assert_eq!(app.tracking(), None);
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            (app.screen, app.form.amount.as_str()),
            (Screen::Ready, "")
        );
        assert_eq!(press(&mut app, KeyCode::Char('q')), Some(Action::Quit));
    }

    #[test]
    fn a_failed_receipt_or_a_refused_submission_keeps_the_form() {
        let mut app = ready(Network::Sepolia);
        submit(&mut app, "1", BOB, Ok(FEE));
        press(&mut app, KeyCode::Char('y'));
        app.sent(Ok(TxHash::ZERO));
        app.tracked(Tracked::Receipt { success: false });
        assert_eq!(
            app.sending.as_ref().map(|s| &s.state),
            Some(&SendState::Failed)
        );
        press(&mut app, KeyCode::Esc);
        assert_eq!((app.screen, app.form.amount.as_str()), (Screen::Ready, "1"));

        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('y'));
        app.sent(Err("invalid transaction: nonce too low".to_owned()));
        assert_eq!(
            app.sending.as_ref().map(|s| &s.state),
            Some(&SendState::Rejected(
                "invalid transaction: nonce too low".to_owned()
            ))
        );
        assert_eq!(press(&mut app, KeyCode::Char('q')), Some(Action::Quit));
    }

    #[test]
    fn l_toggles_the_log_pane_and_ctrl_c_quits() {
        let mut app = ready(Network::Sepolia);
        press(&mut app, KeyCode::Char('l'));
        assert!(app.logs);
        press(&mut app, KeyCode::Char('l'));
        assert!(!app.logs);
        assert_eq!(
            app.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Action::Quit)
        );
    }
}
