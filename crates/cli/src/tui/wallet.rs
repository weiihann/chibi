#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use alloy_primitives::{Address, address};

    use super::*;
    use crate::config::Config;
    use crate::tui::transfer;

    const ANVIL: Address = address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    const ANVIL_PHRASE: &str = "test test test test test test test test test test test junk";
    const ANVIL_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    const PASSWORD: &str = "correct horse";

    fn address_of(text: &str) -> Address {
        signer(&parse(text).unwrap()).unwrap().address()
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn the_anvil_phrase_and_its_first_key_give_the_same_address() {
        assert!(matches!(parse(ANVIL_PHRASE), Ok(Secret::Phrase(_))));
        assert_eq!(address_of(ANVIL_PHRASE), ANVIL);
        assert!(matches!(parse(ANVIL_KEY), Ok(Secret::Key(_))));
        assert_eq!(address_of(ANVIL_KEY), ANVIL);
        assert_eq!(address_of(&ANVIL_KEY[2..]), ANVIL);
    }

    #[test]
    fn a_generated_phrase_round_trips_through_the_keystore() {
        let phrase = generate();
        assert_eq!(phrase.split(' ').count(), 12);
        let secret = parse(&phrase).unwrap();
        let address = signer(&secret).unwrap().address();
        let dir = tempfile::tempdir().unwrap();
        let Saved::Written(written, path) =
            save(&secret, PASSWORD, &dir.path().join("config.toml")).unwrap()
        else {
            panic!("a fresh directory has no keystore");
        };
        assert_eq!(written.address(), address);
        assert_eq!(
            transfer::unlock(&path, PASSWORD).unwrap().address(),
            address
        );
    }

    #[test]
    fn saving_writes_a_private_keystore_and_creates_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("chibi").join("config.toml");
        let saved = save(&parse(ANVIL_PHRASE).unwrap(), PASSWORD, &config).unwrap();
        let keystores = dir.path().join("chibi").join("keystores");
        let expected = keystores.join("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266.json");
        let Saved::Written(signer, path) = saved else {
            panic!("a fresh directory has no keystore");
        };
        assert_eq!((signer.address(), &path), (ANVIL, &expected));
        assert_eq!(mode(&keystores), 0o700);
        assert_eq!(mode(&path), 0o600);
        assert_eq!(transfer::unlock(&path, PASSWORD).unwrap().address(), ANVIL);
        assert!(matches!(
            transfer::unlock(&path, "wrong password"),
            Err(transfer::UnlockError::WrongPassword)
        ));
        assert_eq!(
            Config::load(&config).unwrap(),
            Some(Config {
                keystore: Some(expected),
                ..Config::default()
            })
        );
    }

    #[test]
    fn saving_keeps_every_other_config_key_and_never_overwrites_a_keystore() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let text = "# my chibi\n\
                    network = \"mainnet\"\n\
                    rpc_port = 8645\n\
                    addresses = [\"0x0000000000000000000000000000000000000b0b\"]\n\
                    \n\
                    [[contracts]]\n\
                    address = \"0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238\"\n\
                    slots = [\"0x0000000000000000000000000000000000000000000000000000000000000001\"]\n";
        fs::write(&config, text).unwrap();
        let before = Config::load(&config).unwrap().unwrap();

        let Saved::Written(_, path) = save(&parse(ANVIL_KEY).unwrap(), PASSWORD, &config).unwrap()
        else {
            panic!("a fresh directory has no keystore");
        };
        let written = fs::read_to_string(&config).unwrap();
        assert!(
            written.starts_with("# my chibi\nnetwork = \"mainnet\"\n"),
            "{written}"
        );
        assert_eq!(
            Config::load(&config).unwrap(),
            Some(Config {
                keystore: Some(path.clone()),
                ..before.clone()
            })
        );

        let keystore = fs::read(&path).unwrap();
        fs::write(&config, text).unwrap();
        let again = save(&parse(ANVIL_PHRASE).unwrap(), "another password", &config).unwrap();
        let Saved::Exists(address, kept) = again else {
            panic!("the keystore for the address is already there");
        };
        assert_eq!((address, &kept), (ANVIL, &path));
        assert_eq!(fs::read(&path).unwrap(), keystore);
        assert_eq!(transfer::unlock(&path, PASSWORD).unwrap().address(), ANVIL);
        assert_eq!(
            Config::load(&config).unwrap(),
            Some(Config {
                keystore: Some(path),
                ..before
            })
        );
    }
}
