//! Proof of concept: configuration layering (defaults < {env}.yaml <
//! {env}.local.yaml < env vars), validation, and secret redaction.

use nebula::Config;

#[test]
fn defaults_apply_without_any_files() {
    figment::Jail::expect_with(|jail| {
        let config = Config::load_from(jail.directory()).expect("defaults must load");
        assert_eq!(config.environment, "dev");
        assert_eq!(config.server.port, 5000);
        assert!(!config.multitenancy.enabled);
        assert!(config.currencies.is_empty());
        Ok(())
    });
}

#[test]
fn file_overrides_defaults_and_env_overrides_file() {
    figment::Jail::expect_with(|jail| {
        jail.create_file(
            "dev.yaml",
            "server:\n  port: 6000\nmultitenancy:\n  enabled: true\n",
        )?;
        jail.set_env("NEBULA__SERVER__PORT", "7000");

        let config = Config::load_from(jail.directory()).expect("layered config must load");
        assert_eq!(config.server.port, 7000, "env var beats file");
        assert!(config.multitenancy.enabled, "file beats default");
        Ok(())
    });
}

#[test]
fn local_overlay_overrides_the_environment_file() {
    figment::Jail::expect_with(|jail| {
        jail.create_file("dev.yaml", "server:\n  port: 6000\n")?;
        jail.create_file("dev.local.yaml", "server:\n  port: 6500\n")?;

        let config = Config::load_from(jail.directory()).expect("config must load");
        assert_eq!(config.server.port, 6500);
        Ok(())
    });
}

#[test]
fn nebula_env_selects_the_environment_file() {
    figment::Jail::expect_with(|jail| {
        jail.create_file("dev.yaml", "server:\n  port: 6000\n")?;
        jail.create_file("test.yaml", "server:\n  port: 6500\n")?;
        jail.set_env("NEBULA_ENV", "test");

        let config = Config::load_from(jail.directory()).expect("config must load");
        assert_eq!(config.environment, "test");
        assert_eq!(config.server.port, 6500);
        Ok(())
    });
}

#[test]
fn currencies_come_from_configuration() {
    figment::Jail::expect_with(|jail| {
        jail.create_file(
            "dev.yaml",
            "currencies:\n  - { code: KES, minor_units: 2 }\n  - { code: JPY, minor_units: 0 }\n",
        )?;
        let config = Config::load_from(jail.directory()).expect("config must load");
        assert_eq!(config.currencies.len(), 2);
        assert_eq!(config.currencies[0].code, "KES");
        Ok(())
    });
}

#[test]
fn invalid_configuration_fails_at_boot() {
    figment::Jail::expect_with(|jail| {
        jail.create_file("dev.yaml", "server:\n  port: 0\n")?;
        let err = Config::load_from(jail.directory()).expect_err("port 0 must be rejected");
        assert!(err.to_string().contains("server.port"));
        Ok(())
    });
}

#[test]
fn invalid_currency_config_fails_at_boot() {
    figment::Jail::expect_with(|jail| {
        jail.create_file("dev.yaml", "currencies:\n  - { code: kes, minor_units: 2 }\n")?;
        let err = Config::load_from(jail.directory()).expect_err("lowercase code must be rejected");
        assert!(err.to_string().contains("kes"), "got: {err}");
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
