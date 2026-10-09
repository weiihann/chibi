//! The wallet the TUI's setup makes: a new recovery phrase, an imported private key or
//! phrase, and the encrypted keystore it saves beside the config.

use std::fs::{self, DirBuilder, File, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use alloy_primitives::{Address, hex};
use alloy_signer_local::coins_bip39::{English, Mnemonic, MnemonicError, Wordlist};
use alloy_signer_local::{LocalSignerError, MnemonicBuilder, PrivateKeySigner};
use zeroize::{Zeroize, Zeroizing};

use crate::config::{Config, ConfigError};

/// The longest secret accepted at import, in bytes: its buffer's fixed capacity, room for
/// 24 words of up to 8 letters.
pub const SECRET_BYTES: usize = 256;

/// Words in a generated recovery phrase.
pub const PHRASE_WORDS: usize = 12;

/// What the wallet's key is made from. Not `Debug`: it is the key.
pub enum Secret {
    /// A BIP-39 English recovery phrase, single-spaced and lowercase; its first account
    /// m/44'/60'/0'/0/0 is the key.
    Phrase(Zeroizing<String>),
    /// A secp256k1 private key.
    Key(Zeroizing<[u8; 32]>),
}

/// Why imported text is not a secret.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SecretError {
    /// Neither a key nor a phrase.
    #[error("expected a private key (64 hex digits) or a recovery phrase (12 or 24 words)")]
    Unrecognized,
    /// Words, but not 12 or 24 of them.
    #[error("a recovery phrase has 12 or 24 words; this has {0}")]
    WordCount(usize),
    /// The word at this 1-based position is not in the word list.
    #[error("word {0} is not in the English BIP-39 word list")]
    Word(usize),
    /// Every word is in the list, but the last does not carry the others' checksum.
    #[error("the recovery phrase's checksum does not match; check the words and their order")]
    Checksum,
    /// 64 hex digits that are zero or not below the curve order.
    #[error("this is not a valid private key")]
    Key,
}

/// A new 12-word English recovery phrase from 16 bytes of the thread's CSPRNG.
pub fn generate() -> Zeroizing<String> {
    Zeroizing::new(Mnemonic::<English>::new(&mut rand::thread_rng()).to_phrase())
}

/// Two distinct positions of a [`PHRASE_WORDS`]-word phrase, 0-based, to ask back.
pub fn positions() -> [usize; 2] {
    let picked = rand::seq::index::sample(&mut rand::thread_rng(), PHRASE_WORDS, 2);
    [picked.index(0), picked.index(1)]
}

/// One token of 64 hex digits, with or without `0x`, is a private key; 12 or 24 words are a
/// recovery phrase, checked against the word list and its checksum.
pub fn parse(text: &str) -> Result<Secret, SecretError> {
    let count = text.split_whitespace().count();
    let words = text
        .split_whitespace()
        .all(|word| word.bytes().all(|b| b.is_ascii_alphabetic()));
    match count {
        1 => parse_key(text.trim()),
        2.. if words => parse_phrase(text, count),
        _ => Err(SecretError::Unrecognized),
    }
}

fn parse_key(token: &str) -> Result<Secret, SecretError> {
    let digits = token
        .strip_prefix("0x")
        .or_else(|| token.strip_prefix("0X"))
        .unwrap_or(token);
    let mut key = Zeroizing::new([0u8; 32]);
    if digits.len() != 64 || hex::decode_to_slice(digits, key.as_mut_slice()).is_err() {
        return Err(SecretError::Unrecognized);
    }
    PrivateKeySigner::from_slice(key.as_slice()).map_err(|_| SecretError::Key)?;
    Ok(Secret::Key(key))
}

fn parse_phrase(text: &str, count: usize) -> Result<Secret, SecretError> {
    if !matches!(count, 12 | 24) {
        return Err(SecretError::WordCount(count));
    }
    let mut phrase = Zeroizing::new(String::with_capacity(SECRET_BYTES));
    for (at, word) in text.split_whitespace().enumerate() {
        if at > 0 {
            phrase.push(' ');
        }
        let start = phrase.len();
        phrase.extend(word.chars().map(|c| c.to_ascii_lowercase()));
        if !English::get_all().contains(&&phrase[start..]) {
            return Err(SecretError::Word(at + 1));
        }
    }
    match Mnemonic::<English>::new_from_phrase(&phrase) {
        Ok(_) => Ok(Secret::Phrase(phrase)),
        Err(err) => {
            if let MnemonicError::InvalidPhrase(mut echoed) = err {
                echoed.zeroize();
            }
            Err(SecretError::Checksum)
        }
    }
}

/// The signer `secret` holds: the key itself, or the phrase's first account.
pub fn signer(secret: &Secret) -> Result<PrivateKeySigner, LocalSignerError> {
    match secret {
        Secret::Key(key) => PrivateKeySigner::from_slice(key.as_slice()).map_err(Into::into),
        Secret::Phrase(phrase) => MnemonicBuilder::<English>::default()
            .phrase(phrase.as_str())
            .build(),
    }
}

/// Where a saved wallet's keystore is.
pub enum Saved {
    /// Newly encrypted to this path, with its decrypted signer.
    Written(PrivateKeySigner, PathBuf),
    /// A keystore for this address was already at this path and is kept as it was.
    Exists(Address, PathBuf),
}

/// Why the wallet could not be saved.
#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    /// The secret gives no signer.
    #[error("cannot derive the key: {0}")]
    Derive(LocalSignerError),
    /// A directory or file could not be made.
    #[error("cannot write {}: {source}", path.display())]
    Write {
        /// What was being written.
        path: PathBuf,
        /// Why it failed.
        source: io::Error,
    },
    /// The keystore could not be encrypted or written.
    #[error("cannot encrypt the keystore: {0}")]
    Encrypt(LocalSignerError),
    /// The config file exists but is not TOML chibi can edit.
    #[error("cannot add keystore to {}: {reason}", path.display())]
    Config {
        /// The config file.
        path: PathBuf,
        /// What was wrong.
        reason: String,
    },
    /// The config could not be created.
    #[error(transparent)]
    Save(#[from] ConfigError),
}

/// Encrypts `secret`'s key with `password` into `keystores/<checksummed address>.json` beside
/// the config file at `config` (directory 0700, file 0600), unless a keystore for the address
/// is already there, then records it as the config's `keystore`.
pub fn save(secret: &Secret, password: &str, config: &Path) -> Result<Saved, SaveError> {
    save_with(secret, password, config, &|_| Ok(()))
}

/// [`save`], calling `fault` with each temporary file once it is written, before it is
/// published.
fn save_with(
    secret: &Secret,
    password: &str,
    config: &Path,
    fault: &dyn Fn(&Path) -> io::Result<()>,
) -> Result<Saved, SaveError> {
    let derived = signer(secret).map_err(SaveError::Derive)?;
    let address = derived.address();
    let dir = config
        .parent()
        .unwrap_or_else(|| Path::new("/"))
        .join("keystores");
    let write_error = |path: &Path| {
        let path = path.to_owned();
        move |source| SaveError::Write { path, source }
    };
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .and_then(|()| fs::set_permissions(&dir, Permissions::from_mode(0o700)))
        .map_err(write_error(&dir))?;
    let name = format!("{}.json", address.to_checksum(None));
    let path = dir.join(&name);
    if path.symlink_metadata().is_ok() {
        record(config, &path)?;
        return Ok(Saved::Exists(address, path));
    }
    let temp = dir.join(format!(".{name}.{:016x}", rand::random::<u64>()));
    DirBuilder::new()
        .mode(0o700)
        .create(&temp)
        .map_err(write_error(&temp))?;
    let key = Zeroizing::new(derived.to_bytes().0);
    drop(derived);
    let published = publish(&temp, &name, &path, key.as_slice(), password, fault);
    let _ = fs::remove_dir_all(&temp);
    let saved = match published {
        Ok(signer) => Saved::Written(signer, path.clone()),
        Err(SaveError::Write { source, .. }) if source.kind() == io::ErrorKind::AlreadyExists => {
            Saved::Exists(address, path.clone())
        }
        Err(err) => return Err(err),
    };
    File::open(&dir)
        .and_then(|dir| dir.sync_all())
        .map_err(write_error(&dir))?;
    record(config, &path)?;
    Ok(saved)
}

/// Encrypts `key` to `name` in the private directory `temp`, makes it 0600 and durable,
/// then hard-links it to `path`, which fails with `AlreadyExists` if anything is there.
fn publish(
    temp: &Path,
    name: &str,
    path: &Path,
    key: &[u8],
    password: &str,
    fault: &dyn Fn(&Path) -> io::Result<()>,
) -> Result<PrivateKeySigner, SaveError> {
    let (signer, _) =
        PrivateKeySigner::encrypt_keystore(temp, &mut rand::thread_rng(), key, password, Some(name))
            .map_err(SaveError::Encrypt)?;
    let file = temp.join(name);
    fs::set_permissions(&file, Permissions::from_mode(0o600))
        .and_then(|()| File::open(&file)?.sync_all())
        .and_then(|()| fault(&file))
        .and_then(|()| fs::hard_link(&file, path))
        .map_err(|source| SaveError::Write {
            path: path.to_owned(),
            source,
        })?;
    Ok(signer)
}

/// Sets `keystore` in the config file at `config`, keeping everything else in it, or creates
/// it with the defaults and `keystore`.
fn record(config: &Path, keystore: &Path) -> Result<(), SaveError> {
    let text = match fs::read_to_string(config) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let created = Config {
                keystore: Some(keystore.to_owned()),
                ..Config::default()
            };
            return Ok(created.save(config)?);
        }
        Err(err) => {
            return Err(SaveError::Write {
                path: config.to_owned(),
                source: err,
            });
        }
    };
    let invalid = |reason: String| SaveError::Config {
        path: config.to_owned(),
        reason,
    };
    let value = keystore
        .to_str()
        .ok_or_else(|| invalid(format!("{} is not UTF-8", keystore.display())))?;
    let mut document: toml_edit::DocumentMut = text
        .parse()
        .map_err(|err: toml_edit::TomlError| invalid(err.to_string()))?;
    document["keystore"] = toml_edit::value(value);
    fs::write(config, document.to_string()).map_err(|source| SaveError::Write {
        path: config.to_owned(),
        source,
    })
}

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

    fn entries(dir: &Path) -> Vec<PathBuf> {
        let mut entries: Vec<PathBuf> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        entries.sort();
        entries
    }

    fn injected(path: &Path) -> io::Result<()> {
        Err(io::Error::other(format!("injected at {}", path.display())))
    }

    #[test]
    fn a_keystore_is_published_whole_and_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let keystores = dir.path().join("keystores");
        let failed = save_with(&parse(ANVIL_KEY).unwrap(), PASSWORD, &config, &|path| {
            if path.starts_with(&keystores) {
                injected(path)
            } else {
                Ok(())
            }
        });
        assert!(matches!(failed, Err(SaveError::Write { .. })));
        assert_eq!(entries(&keystores), Vec::<PathBuf>::new());
        assert!(!config.exists());

        let Saved::Written(_, path) = save(&parse(ANVIL_KEY).unwrap(), PASSWORD, &config).unwrap()
        else {
            panic!("a fresh directory has no keystore");
        };
        assert_eq!(entries(&keystores), vec![path.clone()]);
        assert_eq!(mode(&path), 0o600);
        assert_eq!(transfer::unlock(&path, PASSWORD).unwrap().address(), ANVIL);
    }

    #[test]
    fn a_file_already_at_the_keystore_path_is_left_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        let keystores = dir.path().join("keystores");
        fs::create_dir(&keystores).unwrap();
        let path = keystores.join("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266.json");
        fs::write(&path, "not a keystore").unwrap();
        let saved = save(
            &parse(ANVIL_KEY).unwrap(),
            PASSWORD,
            &dir.path().join("config.toml"),
        )
        .unwrap();
        assert!(matches!(saved, Saved::Exists(ANVIL, ref kept) if *kept == path));
        assert_eq!(fs::read(&path).unwrap(), b"not a keystore");
        assert_eq!(entries(&keystores), vec![path]);
    }

    #[test]
    fn a_config_is_replaced_whole_keeping_its_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let keystores = dir.path().join("keystores");
        let text = "# mine\nnetwork = \"mainnet\"\n";
        fs::write(&config, text).unwrap();
        fs::set_permissions(&config, Permissions::from_mode(0o640)).unwrap();
        let _ = save_with(&parse(ANVIL_KEY).unwrap(), PASSWORD, &config, &|path| {
            if path.starts_with(&keystores) {
                Ok(())
            } else {
                injected(path)
            }
        });
        assert_eq!(fs::read_to_string(&config).unwrap(), text);
        assert_eq!(entries(dir.path()), vec![config.clone(), keystores.clone()]);

        fs::remove_dir_all(&keystores).unwrap();
        save(&parse(ANVIL_KEY).unwrap(), PASSWORD, &config).unwrap();
        assert!(fs::read_to_string(&config).unwrap().starts_with(text));
        assert_eq!(mode(&config), 0o640);
        assert_eq!(entries(dir.path()), vec![config, keystores]);
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
