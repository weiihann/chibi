//! Where the `chibi` binary looks for its config: `--config`, `CHIBI_CONFIG`, then XDG.

use std::path::Path;
use std::process::{Command, Output, Stdio};

const ALICE: &str = "0x8f3Cf7ad23Cd3CaDbD9735AFf958023239c6A063";

/// Runs chibi with HOME at `home`, no XDG variables, and `CHIBI_CONFIG` as given.
fn chibi(home: &Path, chibi_config: Option<&Path>, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_chibi"));
    command
        .args(args)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("CHIBI_CONFIG")
        .stdin(Stdio::null());
    if let Some(path) = chibi_config {
        command.env("CHIBI_CONFIG", path);
    }
    match command.output() {
        Ok(output) => output,
        Err(err) => panic!("cannot run chibi: {err}"),
    }
}

fn init_args(network: &str) -> Vec<&str> {
    vec![
        "init",
        "--network",
        network,
        "--address",
        ALICE,
        "--rpc-host",
        "127.0.0.1",
        "--rpc-port",
        "8545",
    ]
}

fn stdout(output: &Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn init_defaults_to_dot_config_under_home_when_chibi_config_is_unset_or_empty() {
    for chibi_config in [None, Some(Path::new(""))] {
        let home = tempfile::tempdir().unwrap();
        let written = stdout(&chibi(home.path(), chibi_config, &init_args("sepolia")));
        let path = home.path().join(".config/chibi/config.toml");
        assert!(path.is_file(), "{chibi_config:?}: {written}");
        assert!(written.contains(&path.display().to_string()), "{written}");
        assert!(!written.contains("--config"), "{written}");
    }
}

#[test]
fn chibi_config_selects_the_config_and_the_flag_overrides_it() {
    let home = tempfile::tempdir().unwrap();
    let from_env = home.path().join("custom/chibi.toml");
    let from_flag = home.path().join("flag.toml");

    stdout(&chibi(home.path(), Some(&from_env), &init_args("mainnet")));
    assert!(from_env.is_file());
    assert!(!home.path().join(".config").exists());

    let status = stdout(&chibi(home.path(), Some(&from_env), &["status"]));
    assert!(status.contains("no snapshot for mainnet"), "{status}");

    let mut args = vec!["--config", from_flag.to_str().unwrap()];
    args.extend(init_args("sepolia"));
    stdout(&chibi(home.path(), Some(&from_env), &args));
    let status = stdout(&chibi(
        home.path(),
        Some(&from_env),
        &["status", "--config", from_flag.to_str().unwrap()],
    ));
    assert!(status.contains("no snapshot for sepolia"), "{status}");
}

#[test]
fn a_named_config_that_does_not_exist_fails_every_command_but_init() {
    let home = tempfile::tempdir().unwrap();
    let missing = home.path().join("typo.toml");
    let shown = missing.display().to_string();
    for args in [&["run"][..], &["run", "-b"], &["status"], &["stop"]] {
        for (chibi_config, flag) in [(Some(missing.as_path()), None), (None, Some(&shown))] {
            let mut argv: Vec<&str> = args.to_vec();
            if let Some(flag) = flag {
                argv.extend(["--config", flag]);
            }
            let output = chibi(home.path(), chibi_config, &argv);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(!output.status.success(), "{argv:?}");
            assert!(stderr.contains(&shown), "{argv:?}: {stderr}");
        }
    }
    assert!(!missing.exists());
    assert!(!home.path().join(".local").exists());

    let status = stdout(&chibi(home.path(), None, &["status"]));
    assert!(status.contains("no snapshot for sepolia"), "{status}");
    let stopped = stdout(&chibi(home.path(), None, &["stop"]));
    assert!(stopped.contains("not running on sepolia"), "{stopped}");
}
