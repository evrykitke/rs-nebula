//! Proof of concept: configuration layering and validation.

use nebula::Config;

#[test]
fn defaults_apply_without_any_files() {
    figment::Jail::expect_with(|jail| {
        let config = Config::load_from(jail.directory()).expect("defaults must load");
        assert_eq!(config.environment, "development");
        assert_eq!(config.server.port, 5000);
        assert!(!config.multitenancy.enabled);
        Ok(())
    });
}

#[test]
fn file_overrides_defaults_and_env_overrides_file() {
    figment::Jail::expect_with(|jail| {
        jail.create_file(
            "nebula.toml",
            r#"
                [server]
                port = 6000

                [multitenancy]
                enabled = true
            "#,
        )?;
        jail.set_env("NEBULA__SERVER__PORT", "7000");

        let config = Config::load_from(jail.directory()).expect("layered config must load");
        // env beats file
        assert_eq!(config.server.port, 7000);
        // file beats default
        assert!(config.multitenancy.enabled);
        Ok(())
    });
}

#[test]
fn environment_specific_file_overrides_base_file() {
    figment::Jail::expect_with(|jail| {
        jail.create_file("nebula.toml", "[server]\nport = 6000\n")?;
        jail.create_file("nebula.staging.toml", "[server]\nport = 6500\n")?;
        jail.set_env("NEBULA_ENV", "staging");

        let config = Config::load_from(jail.directory()).expect("config must load");
        assert_eq!(config.environment, "staging");
        assert_eq!(config.server.port, 6500);
        Ok(())
    });
}

#[test]
fn invalid_configuration_fails_at_boot() {
    figment::Jail::expect_with(|jail| {
        jail.create_file("nebula.toml", "[server]\nport = 0\n")?;
        let err = Config::load_from(jail.directory()).expect_err("port 0 must be rejected");
        assert!(err.to_string().contains("server.port"));
        Ok(())
    });
}

#[test]
fn secrets_never_leak_through_debug_output() {
    let secret = nebula::config::Secret::new("postgres://user:hunter2@host/db");
    assert_eq!(format!("{secret:?}"), "***");
    assert_eq!(secret.to_string(), "***");
    assert_eq!(secret.expose(), "postgres://user:hunter2@host/db");
}
