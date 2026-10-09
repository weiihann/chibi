//! Drawing the [`App`] with ratatui. Reads the state; changes nothing.

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, address};
    use chibi_node::{Network, Startup, Token};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::tui::app::Balances;

    const SENDER: Address = address!("0x17c5185167401ed00cf5f5b2fc97d9bbfdb7d025");
    const TKN: Token = Token {
        symbol: "TKN",
        name: "Test token",
        address: address!("0x00000000000000000000000000000000000070c3"),
        decimals: 6,
    };
    const REGISTRY: &[Token] = &[TKN];

    fn screen(app: &App, logs: &[String]) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| draw(frame, app, logs)).unwrap();
        let buffer = terminal.backend().buffer();
        let width = usize::from(buffer.area().width);
        buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn waiting() -> App {
        let mut app = App::new(Network::Sepolia, 11_155_111, REGISTRY, false);
        app.unlocked(Ok(SENDER));
        app.startup(Startup::Waiting {
            peers: 12,
            state_peers: 0,
            elapsed: Duration::from_secs(80),
        });
        app
    }

    #[test]
    fn unlock_masks_the_password() {
        let mut app = App::new(Network::Sepolia, 11_155_111, REGISTRY, false);
        app.password = "hunter2".to_owned();
        let shown = screen(&app, &[]);
        assert!(shown.contains("password  *******"), "{shown}");
        assert!(!shown.contains("hunter2"), "{shown}");
    }

    #[test]
    fn waiting_shows_the_dozing_cat_and_the_peer_counts() {
        let mut app = waiting();
        let shown = screen(&app, &[]);
        assert!(shown.contains(" ( -.- )  z"), "{shown}");
        assert!(
            shown.contains("looking for a state peer · peers 12 · state peers 0 · 1m20s"),
            "{shown}"
        );
        app.tick();
        app.tick();
        assert!(screen(&app, &[]).contains(" ( o.- )"));
    }

    #[test]
    fn excited_shows_the_found_peer() {
        let mut app = waiting();
        app.startup(Startup::FoundStatePeer);
        let shown = screen(&app, &[]);
        assert!(shown.contains("found a state peer!"), "{shown}");
        assert!(shown.contains("( ^o^ )"), "{shown}");
    }

    fn ready() -> App {
        let mut app = waiting();
        app.startup(Startup::Serving);
        app.health(100, 1);
        app.balances(Balances {
            eth: Some(U256::from(3_000_000_000_000_000_000u64)),
            tokens: vec![(TKN, U256::from(1_500_000))],
        });
        app
    }

    #[test]
    fn ready_shows_the_form() {
        let shown = screen(&ready(), &[]);
        for expected in [
            "from    0x17c5185167401eD00cF5F5b2fc97D9BBfDb7D025   3.00 ETH",
            "  token   < ETH >   3.00 ETH",
            "> amount",
            "  to",
            "  fee",
            "Enter review",
        ] {
            assert!(shown.contains(expected), "{expected}\n{shown}");
        }
    }

    #[test]
    fn paused_shows_the_sleepy_cat_and_why() {
        let mut app = ready();
        app.health(101, 0);
        let shown = screen(&app, &[]);
        assert!(shown.contains("( u.u )"), "{shown}");
        assert!(shown.contains("waiting for a state peer"), "{shown}");
    }

    #[test]
    fn the_log_pane_shows_the_tail_of_the_log() {
        let mut app = ready();
        let logs: Vec<String> = (0..30).map(|n| format!("log line {n}")).collect();
        assert!(!screen(&app, &logs).contains("log line 29"));
        app.logs = true;
        let shown = screen(&app, &logs);
        assert!(shown.contains("log line 29"), "{shown}");
        assert!(shown.contains("log line 20"), "{shown}");
        assert!(!shown.contains("log line 19"), "{shown}");
    }

    #[test]
    fn elapsed_time_reads_like_1m20s() {
        assert_eq!(elapsed(Duration::from_secs(45)), "45s");
        assert_eq!(elapsed(Duration::from_secs(80)), "1m20s");
        assert_eq!(elapsed(Duration::from_secs(3_725)), "1h02m05s");
    }
}
