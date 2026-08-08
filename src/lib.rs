mod annotations;
mod app;
mod cli;
mod codex_runtime;
mod completion;
mod config;
mod debuglog;
mod errors;
mod managed_server;
mod rate_limit_reset;
mod rpc;
mod session;
mod time_filter;
mod turns;

pub async fn run() -> i32 {
    app::run_cli(std::env::args_os()).await
}

#[cfg(test)]
mod tests {
    use super::cli::Cli;
    use super::config::{
        AppConfig, Endpoint, MANAGED_SERVER_ALIAS, ManagedConfig, ServerConfig,
        load_config_or_default, resolve_config_path_from, resolve_direct_target,
        resolve_target_from, validate_config,
    };
    #[cfg(unix)]
    use super::config::{
        managed_runtime_root, managed_runtime_root_from, managed_socket_path_from,
        resolve_codex_home, validate_managed_socket_path, validate_xdg_runtime_directory,
    };
    use std::collections::BTreeMap;
    #[cfg(unix)]
    use std::ffi::OsStr;
    use std::path::PathBuf;

    use clap::Parser;

    #[test]
    fn config_path_precedence_prefers_flag_then_env_then_default() {
        let home = PathBuf::from("/home/tester");
        assert_eq!(
            resolve_config_path_from(
                Some(PathBuf::from("/tmp/a.toml")),
                Some("/tmp/b.toml"),
                &home
            ),
            PathBuf::from("/tmp/a.toml")
        );
        assert_eq!(
            resolve_config_path_from(None, Some("/tmp/b.toml"), &home),
            PathBuf::from("/tmp/b.toml")
        );
        assert_eq!(
            resolve_config_path_from(None, None, &home),
            PathBuf::from("/home/tester/.config/codex-tamer/config.toml")
        );
    }

    #[test]
    fn cli_accepts_managed_codex_overrides() {
        let cli = Cli::try_parse_from([
            "codex-tamer",
            "--codex",
            "/opt/codex/bin/codex",
            "--codex-home",
            "/tmp/codex-home",
            "list",
            "--json",
        ])
        .unwrap();
        assert_eq!(cli.codex, Some(PathBuf::from("/opt/codex/bin/codex")));
        assert_eq!(cli.codex_home, Some(PathBuf::from("/tmp/codex-home")));
    }

    #[test]
    fn missing_implicit_config_is_empty_but_missing_explicit_config_fails() {
        let temp = tempfile::TempDir::new().unwrap();
        let missing = temp.path().join("missing.toml");

        let config = load_config_or_default(&missing, false).unwrap();
        assert!(config.servers.is_empty());
        assert!(load_config_or_default(&missing, true).is_err());
    }

    fn server(endpoint: &str) -> ServerConfig {
        ServerConfig {
            endpoint: Some(endpoint.to_string()),
            kind: None,
            path: None,
            auth_token_env: None,
            auth_token: None,
            model: None,
            model_reasoning_effort: None,
            allow_rate_limit_reset: false,
        }
    }

    fn legacy_server(path: &str) -> ServerConfig {
        ServerConfig {
            endpoint: None,
            kind: Some("uds".to_string()),
            path: Some(PathBuf::from(path)),
            auth_token_env: None,
            auth_token: None,
            model: None,
            model_reasoning_effort: None,
            allow_rate_limit_reset: false,
        }
    }

    #[test]
    fn rate_limit_reset_permission_is_per_server_and_defaults_to_disabled() {
        let config: AppConfig = toml::from_str(
            r#"
[servers.disabled]
endpoint = "unix:///tmp/disabled.sock"

[servers.enabled]
endpoint = "unix:///tmp/enabled.sock"
allow_rate_limit_reset = true
"#,
        )
        .unwrap();

        assert!(!config.servers["disabled"].allow_rate_limit_reset);
        assert!(config.servers["enabled"].allow_rate_limit_reset);
    }

    #[test]
    fn target_precedence_handles_connect_server_env_and_singleton() {
        let mut servers = BTreeMap::new();
        servers.insert("main".to_string(), server("unix:///tmp/main.sock"));
        let config = AppConfig {
            managed: ManagedConfig::default(),
            model: None,
            model_reasoning_effort: None,
            servers,
        };
        let target = resolve_direct_target("unix:///tmp/direct.sock", None, None).unwrap();
        assert_eq!(target.server, "unix:///tmp/direct.sock");
        assert_eq!(
            target.endpoint,
            Endpoint::Unix {
                path: PathBuf::from("/tmp/direct.sock")
            }
        );

        let target = resolve_target_from(&config, None, None).unwrap();
        assert_eq!(target.server, "main");

        let target = resolve_target_from(&config, None, Some("main")).unwrap();
        assert_eq!(target.server, "main");
    }

    #[test]
    fn target_resolution_merges_global_and_server_model_defaults() {
        let mut servers = BTreeMap::new();
        servers.insert(
            "main".to_string(),
            ServerConfig {
                endpoint: Some("unix:///tmp/main.sock".to_string()),
                kind: None,
                path: None,
                auth_token_env: None,
                auth_token: None,
                model: None,
                model_reasoning_effort: Some("high".to_string()),
                allow_rate_limit_reset: false,
            },
        );
        servers.insert(
            "work".to_string(),
            ServerConfig {
                endpoint: Some("unix:///tmp/work.sock".to_string()),
                kind: None,
                path: None,
                auth_token_env: None,
                auth_token: None,
                model: Some("gpt-5.5".to_string()),
                model_reasoning_effort: None,
                allow_rate_limit_reset: false,
            },
        );
        let config = AppConfig {
            managed: ManagedConfig::default(),
            model: Some("gpt-global".to_string()),
            model_reasoning_effort: Some("low".to_string()),
            servers,
        };

        let main = resolve_target_from(&config, Some("main"), None).unwrap();
        assert_eq!(main.model.as_deref(), Some("gpt-global"));
        assert_eq!(main.model_reasoning_effort.as_deref(), Some("high"));

        let work = resolve_target_from(&config, Some("work"), None).unwrap();
        assert_eq!(work.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(work.model_reasoning_effort.as_deref(), Some("low"));

        let direct = resolve_direct_target("unix:///tmp/direct.sock", None, None).unwrap();
        assert_eq!(direct.model, None);
        assert_eq!(direct.model_reasoning_effort, None);
    }

    #[test]
    fn singleton_target_resolution_uses_model_defaults() {
        let mut servers = BTreeMap::new();
        servers.insert(
            "main".to_string(),
            ServerConfig {
                endpoint: Some("unix:///tmp/main.sock".to_string()),
                kind: None,
                path: None,
                auth_token_env: None,
                auth_token: None,
                model: Some("gpt-5.5".to_string()),
                model_reasoning_effort: None,
                allow_rate_limit_reset: false,
            },
        );
        let config = AppConfig {
            managed: ManagedConfig::default(),
            model: Some("gpt-global".to_string()),
            model_reasoning_effort: Some("high".to_string()),
            servers,
        };

        let target = resolve_target_from(&config, None, None).unwrap();
        assert_eq!(target.server, "main");
        assert_eq!(target.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(target.model_reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn config_validation_rejects_empty_model_defaults() {
        let config = AppConfig {
            managed: ManagedConfig::default(),
            model: Some("   ".to_string()),
            model_reasoning_effort: None,
            servers: BTreeMap::new(),
        };
        assert!(validate_config(&config).is_err());

        let config = AppConfig {
            managed: ManagedConfig::default(),
            model: None,
            model_reasoning_effort: Some("   ".to_string()),
            servers: BTreeMap::new(),
        };
        assert!(validate_config(&config).is_err());

        let config = AppConfig {
            managed: ManagedConfig::default(),
            model: None,
            model_reasoning_effort: Some("provider-private-effort".to_string()),
            servers: BTreeMap::new(),
        };
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn config_validation_accepts_legacy_uds_and_rejects_mixed_endpoint_shape() {
        let mut servers = BTreeMap::new();
        servers.insert("main".to_string(), legacy_server("/tmp/main.sock"));
        assert!(
            validate_config(&AppConfig {
                managed: ManagedConfig::default(),
                model: None,
                model_reasoning_effort: None,
                servers,
            })
            .is_ok()
        );

        let mut servers = BTreeMap::new();
        let mut mixed = server("unix:///tmp/main.sock");
        mixed.kind = Some("uds".to_string());
        servers.insert("main".to_string(), mixed);
        let err = validate_config(&AppConfig {
            managed: ManagedConfig::default(),
            model: None,
            model_reasoning_effort: None,
            servers,
        })
        .expect_err("mixed endpoint and legacy fields should fail");
        assert!(err.to_string().contains("cannot combine"));
    }

    #[test]
    fn config_validation_rejects_path_without_legacy_type() {
        let mut servers = BTreeMap::new();
        servers.insert(
            "main".to_string(),
            ServerConfig {
                endpoint: None,
                kind: None,
                path: Some(PathBuf::from("/tmp/main.sock")),
                auth_token_env: None,
                auth_token: None,
                model: None,
                model_reasoning_effort: None,
                allow_rate_limit_reset: false,
            },
        );
        let err = validate_config(&AppConfig {
            managed: ManagedConfig::default(),
            model: None,
            model_reasoning_effort: None,
            servers,
        })
        .expect_err("path alone should fail");
        assert!(err.to_string().contains("path"));
    }

    #[test]
    fn config_validation_rejects_websocket_tokens_on_insecure_non_loopback_ws() {
        let mut servers = BTreeMap::new();
        let mut server = server("ws://example.com:1234");
        server.auth_token = Some("secret".to_string());
        servers.insert("main".to_string(), server);
        let err = validate_config(&AppConfig {
            managed: ManagedConfig::default(),
            model: None,
            model_reasoning_effort: None,
            servers,
        })
        .expect_err("non-loopback ws auth token should fail");
        assert!(err.to_string().contains("wss:// or loopback ws://"));
    }

    #[test]
    fn config_parse_rejects_unknown_fields() {
        let err = toml::from_str::<AppConfig>(
            r#"[servers.main]
endpoint = "ws://127.0.0.1:1234"
auth_token_en = "CODEX_APP_SERVER_TOKEN"
"#,
        )
        .expect_err("misspelled auth field should fail");
        assert!(err.to_string().contains("unknown field"));
    }

    #[test]
    fn config_validation_rejects_the_reserved_managed_alias() {
        let mut config = AppConfig::default();
        config.servers.insert(
            MANAGED_SERVER_ALIAS.to_string(),
            server("unix:///tmp/configured-managed.sock"),
        );

        let error = validate_config(&config).expect_err("managed alias must stay synthetic");

        assert!(error.to_string().contains("reserved"));
        assert!(error.to_string().contains(MANAGED_SERVER_ALIAS));
    }

    #[cfg(unix)]
    #[test]
    fn empty_config_resolves_one_managed_target_for_codex_home() {
        let temp = tempfile::TempDir::new().unwrap();
        let configured_home = temp.path().join("codex-home-a");
        let canonical_home = std::fs::canonicalize(temp.path())
            .unwrap()
            .join("codex-home-a");
        let config = AppConfig {
            managed: ManagedConfig {
                codex: Some(PathBuf::from("/opt/codex/bin/codex")),
                codex_home: Some(configured_home),
            },
            model: Some("gpt-5.5".to_string()),
            model_reasoning_effort: Some("high".to_string()),
            servers: BTreeMap::new(),
        };

        let target = resolve_target_from(&config, None, None).unwrap();
        assert_eq!(target.server, MANAGED_SERVER_ALIAS);
        assert_eq!(
            target.endpoint,
            Endpoint::Unix {
                path: managed_socket_path_from(&canonical_home, &managed_runtime_root().unwrap())
            }
        );
        assert_eq!(target.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(target.model_reasoning_effort.as_deref(), Some("high"));
    }

    #[cfg(unix)]
    #[test]
    fn explicit_managed_alias_selects_the_synthetic_target() {
        let mut config = AppConfig::default();
        config.managed.codex_home = Some(PathBuf::from("/tmp/codex-home-a"));
        config
            .servers
            .insert("work".to_string(), server("unix:///tmp/work.sock"));

        let target = resolve_target_from(&config, Some(MANAGED_SERVER_ALIAS), None).unwrap();

        assert_eq!(target.server, MANAGED_SERVER_ALIAS);
        assert!(matches!(target.endpoint, Endpoint::Unix { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn fallback_managed_runtime_root_keeps_socket_paths_below_the_macos_limit() {
        let runtime_root = managed_runtime_root_from(None::<&OsStr>, 501).unwrap();
        let socket =
            managed_socket_path_from(PathBuf::from("/tmp/codex-home").as_path(), &runtime_root);

        assert_eq!(runtime_root, PathBuf::from("/tmp/codex-tamer-501"));
        assert!(
            socket.as_os_str().as_encoded_bytes().len() <= 103,
            "fallback socket path is too long: {}",
            socket.display()
        );
    }

    #[cfg(unix)]
    #[test]
    fn managed_socket_paths_reject_the_portable_unix_limit() {
        let socket = PathBuf::from("/tmp").join("x".repeat(104));

        let error = validate_managed_socket_path(&socket).unwrap_err();

        assert!(error.to_string().contains("Unix socket path"));
        assert!(error.to_string().contains("103 bytes"));
    }

    #[cfg(unix)]
    #[test]
    fn xdg_runtime_directory_must_be_private() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::TempDir::new().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o755)).unwrap();

        let error = validate_xdg_runtime_directory(temp.path()).unwrap_err();

        assert!(error.to_string().contains("XDG_RUNTIME_DIR"));
        assert!(error.to_string().contains("0700"));
    }

    #[test]
    fn configured_server_still_wins_over_managed_defaults() {
        let config: AppConfig = toml::from_str(
            r#"
[managed]
codex = "/opt/codex/bin/codex"
codex_home = "/tmp/codex-home-a"

[servers.work]
endpoint = "unix:///tmp/work.sock"
"#,
        )
        .unwrap();

        let target = resolve_target_from(&config, None, None).unwrap();
        assert_eq!(target.server, "work");
        assert_eq!(
            target.endpoint,
            Endpoint::Unix {
                path: PathBuf::from("/tmp/work.sock")
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn missing_codex_home_keeps_one_identity_across_symlinked_parent_creation() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::TempDir::new().unwrap();
        let real_parent = temp.path().join("real");
        let linked_parent = temp.path().join("linked");
        std::fs::create_dir(&real_parent).unwrap();
        symlink(&real_parent, &linked_parent).unwrap();

        let expected_home = std::fs::canonicalize(&real_parent)
            .unwrap()
            .join("future-codex-home");
        let configured_home = linked_parent.join("future-codex-home");
        let config = AppConfig {
            managed: ManagedConfig {
                codex: None,
                codex_home: Some(configured_home),
            },
            model: None,
            model_reasoning_effort: None,
            servers: BTreeMap::new(),
        };
        let before_creation = resolve_codex_home(&config).unwrap();
        std::fs::create_dir(real_parent.join("future-codex-home")).unwrap();
        let after_creation = resolve_codex_home(&config).unwrap();

        assert_eq!(before_creation, after_creation);
        assert_eq!(after_creation, expected_home);
    }

    #[cfg(unix)]
    #[test]
    fn codex_home_resolves_parent_components_after_following_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::TempDir::new().unwrap();
        let real_parent = temp.path().join("real");
        let real_child = real_parent.join("child");
        let expected_home = real_parent.join("home");
        let linked_child = temp.path().join("linked-child");
        std::fs::create_dir_all(&real_child).unwrap();
        std::fs::create_dir(&expected_home).unwrap();
        let expected_home = std::fs::canonicalize(expected_home).unwrap();
        symlink(&real_child, &linked_child).unwrap();

        let mut config = AppConfig::default();
        config.managed.codex_home = Some(linked_child.join("../home"));

        assert_eq!(resolve_codex_home(&config).unwrap(), expected_home);
    }

    #[cfg(unix)]
    #[test]
    fn dangling_codex_home_symlink_keeps_its_target_identity_after_creation() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::TempDir::new().unwrap();
        let target_parent = temp.path().join("targets");
        let target = target_parent.join("future-home");
        let link = temp.path().join("codex-home");
        std::fs::create_dir(&target_parent).unwrap();
        symlink(&target, &link).unwrap();
        let expected_target = std::fs::canonicalize(&target_parent)
            .unwrap()
            .join("future-home");

        let mut config = AppConfig::default();
        config.managed.codex_home = Some(link);
        let before_creation = resolve_codex_home(&config).unwrap();
        std::fs::create_dir(&target).unwrap();
        let after_creation = resolve_codex_home(&config).unwrap();

        assert_eq!(before_creation, expected_target);
        assert_eq!(after_creation, expected_target);
    }

    #[test]
    fn config_validation_rejects_empty_codex_paths() {
        let config: AppConfig = toml::from_str("[managed]\ncodex = \"\"").unwrap();
        assert!(validate_config(&config).is_err());

        let config: AppConfig = toml::from_str("[managed]\ncodex_home = \"\"").unwrap();
        assert!(validate_config(&config).is_err());
    }
}
