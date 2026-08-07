use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use clap::Parser;
use serde_json::{Map, Value, json};

use crate::annotations::{
    AnnotationListItem, clear_annotation, clear_annotations, list_annotations, load_annotation,
    set_annotation,
};
use crate::cli::*;
use crate::completion::{
    completion_candidates, completion_instructions, completion_script, normalize_shell,
};
use crate::config::{
    AppConfig, Target, is_valid_reasoning_effort, legacy_server_warnings, load_config,
    resolve_config_path, resolve_direct_target, resolve_target,
};
use crate::errors::{ExitError, app_server_error, usage_error};
use crate::rate_limit_reset::select_best_rate_limit_reset_credit;
use crate::rpc::RpcClient;
use crate::session::{
    ListThreadsRequest, LoadedStatusRequest, MessagesRequest, SearchThreadsRequest,
    ShowThreadRequest, ThreadForkOptions, ThreadProjection, ThreadStartOptions,
    ThreadStatusRequest, fork_thread, is_thread_not_found_error, list_threads, load_messages,
    loaded_status, read_thread_detail, request_with_resume_retry, resume_thread_for_inspection,
    search_threads, start_thread, thread_id_from_fork, thread_id_from_start, thread_status,
};
use crate::time_filter::parse_since;
use crate::turns::{
    AttachTurnOptions, TurnStartOptions, TurnTerminal, TurnWaitOutcome, attach_turn,
    interrupt_turn as interrupt_turn_request, read_turn_result, start_turn as start_turn_request,
    steer_turn as steer_turn_request, wait_for_turn,
};

const DEFAULT_LIST_LIMIT: u32 = 50;
const DEFAULT_SHOW_LAST: u32 = 20;
const TURN_SCAN_LIMIT: u32 = 200;
const TURN_WAIT_TIMEOUT_SECS: u64 = 60 * 60;
const MAX_INJECT_JSON_BYTES: usize = 16 * 1024 * 1024;
const THREAD_LABEL_WIDTH: usize = 56;
const SEARCH_SNIPPET_WIDTH: usize = 48;
const ANNOTATION_WIDTH: usize = 40;

pub async fn run_cli<I, T>(args: I) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(err) => {
            let _ = err.print();
            return err.exit_code();
        }
    };

    match run(cli).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            classify_error(&err)
        }
    }
}

async fn run(cli: Cli) -> Result<i32> {
    match &cli.command {
        Command::Completion(command) => {
            match &command.command {
                Some(CompletionSubcommand::Script(script)) => {
                    io::stdout().write_all(completion_script(script.shell).as_bytes())?;
                }
                None => {
                    let shell = normalize_shell(command.shell)?;
                    io::stdout().write_all(completion_instructions(shell).as_bytes())?;
                }
            }
            io::stdout().flush()?;
            return Ok(0);
        }
        Command::Complete(command) => {
            io::stdout()
                .write_all(completion_candidates(&command.prefix, &command.words).as_bytes())?;
            io::stdout().flush()?;
            return Ok(0);
        }
        _ => {}
    }

    let config_path = resolve_config_path(cli.config.clone());
    let yolo = !cli.no_yolo;
    if let Command::Servers(command) = &cli.command {
        return servers_command(
            &config_path,
            cli.connect.as_deref(),
            cli.connect_auth_token_env.as_deref(),
            cli.connect_auth_token.as_deref(),
            command,
        )
        .await;
    }
    let config = if cli.connect.is_some() {
        AppConfig::default()
    } else {
        let config = load_config(&config_path)?;
        print_legacy_warnings(&config);
        config
    };
    match cli.command {
        Command::Servers(_) => unreachable!(),
        Command::List(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                cli.connect_auth_token_env.as_deref(),
                cli.connect_auth_token.as_deref(),
                command.server.server.clone(),
                |target, client| async move { list_command(target, client, command).await },
            )
            .await
        }
        Command::Search(command) => match command.command {
            SearchSubcommand::Threads(command) => {
                with_client(
                    &config,
                    cli.connect.as_deref(),
                    cli.connect_auth_token_env.as_deref(),
                    cli.connect_auth_token.as_deref(),
                    command.server.server.clone(),
                    |target, client| async move {
                        search_threads_command(target, client, command).await
                    },
                )
                .await
            }
        },
        Command::Show(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                cli.connect_auth_token_env.as_deref(),
                cli.connect_auth_token.as_deref(),
                command.server.server.clone(),
                |target, client| async move { show_command(target, client, command).await },
            )
            .await
        }
        Command::Messages(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                cli.connect_auth_token_env.as_deref(),
                cli.connect_auth_token.as_deref(),
                command.server.server.clone(),
                |target, client| async move { messages_command(target, client, command).await },
            )
            .await
        }
        Command::New(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                cli.connect_auth_token_env.as_deref(),
                cli.connect_auth_token.as_deref(),
                command.server.server.clone(),
                |target, client| async move { new_command(target, client, command, yolo).await },
            )
            .await
        }
        Command::Fork(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                cli.connect_auth_token_env.as_deref(),
                cli.connect_auth_token.as_deref(),
                command.server.server.clone(),
                |target, client| async move { fork_command(target, client, command, yolo).await },
            )
            .await
        }
        Command::Send(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                cli.connect_auth_token_env.as_deref(),
                cli.connect_auth_token.as_deref(),
                command.server.server.clone(),
                |target, client| async move { send_command(target, client, command, yolo).await },
            )
            .await
        }
        Command::Wait(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                cli.connect_auth_token_env.as_deref(),
                cli.connect_auth_token.as_deref(),
                command.server.server.clone(),
                |target, client| async move { wait_command(target, client, command, yolo).await },
            )
            .await
        }
        Command::Result(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                cli.connect_auth_token_env.as_deref(),
                cli.connect_auth_token.as_deref(),
                command.server.server.clone(),
                |target, client| async move { result_command(target, client, command).await },
            )
            .await
        }
        Command::Events(command) => match command.command {
            EventsSubcommand::Follow(command) => {
                with_client(
                    &config,
                    cli.connect.as_deref(),
                    cli.connect_auth_token_env.as_deref(),
                    cli.connect_auth_token.as_deref(),
                    command.server.server.clone(),
                    |target, client| async move {
                        events_follow_command(target, client, command, yolo).await
                    },
                )
                .await
            }
        },
        Command::Inject(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                cli.connect_auth_token_env.as_deref(),
                cli.connect_auth_token.as_deref(),
                command.server.server.clone(),
                |target, client| async move { inject_command(target, client, command, yolo).await },
            )
            .await
        }
        Command::Settings(command) => {
            match command.command {
                SettingsSubcommand::Show(command) => {
                    with_client(
                        &config,
                        cli.connect.as_deref(),
                        cli.connect_auth_token_env.as_deref(),
                        cli.connect_auth_token.as_deref(),
                        command.server.server.clone(),
                        |target, client| async move {
                            settings_show_command(target, client, command).await
                        },
                    )
                    .await
                }
                SettingsSubcommand::Set(command) => {
                    with_client(
                        &config,
                        cli.connect.as_deref(),
                        cli.connect_auth_token_env.as_deref(),
                        cli.connect_auth_token.as_deref(),
                        command.server.server.clone(),
                        |target, client| async move {
                            settings_set_command(target, client, command, yolo).await
                        },
                    )
                    .await
                }
            }
        }
        Command::Status(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                cli.connect_auth_token_env.as_deref(),
                cli.connect_auth_token.as_deref(),
                command.server.server.clone(),
                |target, client| async move { status_command(target, client, command).await },
            )
            .await
        }
        Command::Steer(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                cli.connect_auth_token_env.as_deref(),
                cli.connect_auth_token.as_deref(),
                command.server.server.clone(),
                |target, client| async move { steer_command(target, client, command, yolo).await },
            )
            .await
        }
        Command::Interrupt(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                cli.connect_auth_token_env.as_deref(),
                cli.connect_auth_token.as_deref(),
                command.server.server.clone(),
                |target, client| async move { interrupt_command(target, client, command).await },
            )
            .await
        }
        Command::Name(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                cli.connect_auth_token_env.as_deref(),
                cli.connect_auth_token.as_deref(),
                command.server.server.clone(),
                |target, client| async move { name_command(target, client, command).await },
            )
            .await
        }
        Command::Pin(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                cli.connect_auth_token_env.as_deref(),
                cli.connect_auth_token.as_deref(),
                command.server.server.clone(),
                |target, client| async move { pin_command(target, client, command, true).await },
            )
            .await
        }
        Command::Unpin(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                cli.connect_auth_token_env.as_deref(),
                cli.connect_auth_token.as_deref(),
                command.server.server.clone(),
                |target, client| async move { pin_command(target, client, command, false).await },
            )
            .await
        }
        Command::Archive(command) => with_client(
            &config,
            cli.connect.as_deref(),
            cli.connect_auth_token_env.as_deref(),
            cli.connect_auth_token.as_deref(),
            command.server.server.clone(),
            |target, client| async move { archive_command(target, client, command, true).await },
        )
        .await,
        Command::Unarchive(command) => with_client(
            &config,
            cli.connect.as_deref(),
            cli.connect_auth_token_env.as_deref(),
            cli.connect_auth_token.as_deref(),
            command.server.server.clone(),
            |target, client| async move { archive_command(target, client, command, false).await },
        )
        .await,
        Command::Models(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                cli.connect_auth_token_env.as_deref(),
                cli.connect_auth_token.as_deref(),
                command.server.server.clone(),
                |target, client| async move { models_command(target, client, command).await },
            )
            .await
        }
        Command::Usage(command) => {
            let rate_limit_reset_allowed_servers = config
                .servers
                .iter()
                .filter(|(_, server)| server.allow_rate_limit_reset)
                .map(|(alias, _)| alias.clone())
                .collect::<std::collections::BTreeSet<_>>();
            let target = resolve_target_for_command(
                &config,
                cli.connect.as_deref(),
                cli.connect_auth_token_env.as_deref(),
                cli.connect_auth_token.as_deref(),
                command.server.clone(),
            )?;
            let rate_limit_reset_allowed =
                rate_limit_reset_allowed_servers.contains(&target.server);
            if matches!(command.action, Some(UsageSubcommand::Redeem)) && !rate_limit_reset_allowed
            {
                return Err(usage_error("rate-limit reset redemption is not permitted"));
            }
            let client = RpcClient::connect(&target.endpoint).await?;
            usage_command(target, client, command, rate_limit_reset_allowed).await
        }
        Command::Goal(command) => match command.command {
            GoalSubcommand::Get(command) => {
                with_client(
                    &config,
                    cli.connect.as_deref(),
                    cli.connect_auth_token_env.as_deref(),
                    cli.connect_auth_token.as_deref(),
                    command.server.server.clone(),
                    |target, client| async move { goal_get_command(target, client, command).await },
                )
                .await
            }
            GoalSubcommand::Set(command) => {
                with_client(
                    &config,
                    cli.connect.as_deref(),
                    cli.connect_auth_token_env.as_deref(),
                    cli.connect_auth_token.as_deref(),
                    command.server.server.clone(),
                    |target, client| async move { goal_set_command(target, client, command).await },
                )
                .await
            }
            GoalSubcommand::Clear(command) => with_client(
                &config,
                cli.connect.as_deref(),
                cli.connect_auth_token_env.as_deref(),
                cli.connect_auth_token.as_deref(),
                command.server.server.clone(),
                |target, client| async move { goal_clear_command(target, client, command).await },
            )
            .await,
        },
        Command::Annotate(command) => match command.command {
            AnnotateSubcommand::Set(command) => {
                let target = resolve_target_for_command(
                    &config,
                    cli.connect.as_deref(),
                    cli.connect_auth_token_env.as_deref(),
                    cli.connect_auth_token.as_deref(),
                    command.server.server.clone(),
                )?;
                annotate_set_command(target, command).await
            }
            AnnotateSubcommand::Get(command) => {
                let target = resolve_target_for_command(
                    &config,
                    cli.connect.as_deref(),
                    cli.connect_auth_token_env.as_deref(),
                    cli.connect_auth_token.as_deref(),
                    command.server.server.clone(),
                )?;
                annotate_get_command(target, command).await
            }
            AnnotateSubcommand::Clear(command) => {
                let target = resolve_target_for_command(
                    &config,
                    cli.connect.as_deref(),
                    cli.connect_auth_token_env.as_deref(),
                    cli.connect_auth_token.as_deref(),
                    command.server.server.clone(),
                )?;
                annotate_clear_command(target, command).await
            }
            AnnotateSubcommand::List(command) => {
                let target = resolve_target_for_command(
                    &config,
                    cli.connect.as_deref(),
                    cli.connect_auth_token_env.as_deref(),
                    cli.connect_auth_token.as_deref(),
                    command.server.server.clone(),
                )?;
                annotate_list_command(target, command).await
            }
            AnnotateSubcommand::Search(command) => {
                let target = resolve_target_for_command(
                    &config,
                    cli.connect.as_deref(),
                    cli.connect_auth_token_env.as_deref(),
                    cli.connect_auth_token.as_deref(),
                    command.server.server.clone(),
                )?;
                annotate_search_command(target, command).await
            }
            AnnotateSubcommand::Prune(command) => {
                with_client(
                    &config,
                    cli.connect.as_deref(),
                    cli.connect_auth_token_env.as_deref(),
                    cli.connect_auth_token.as_deref(),
                    command.server.server.clone(),
                    |target, client| async move {
                        annotate_prune_command(target, client, command).await
                    },
                )
                .await
            }
        },
        Command::Completion(_) | Command::Complete(_) => unreachable!(),
    }
}

fn resolve_target_for_command(
    config: &AppConfig,
    connect: Option<&str>,
    connect_auth_token_env: Option<&str>,
    connect_auth_token: Option<&str>,
    server: Option<String>,
) -> Result<Target> {
    if let Some(endpoint) = connect {
        if server.is_some() || std::env::var("CODEX_TAMER_SERVER").is_ok() {
            return Err(usage_error(
                "--connect is mutually exclusive with --server and CODEX_TAMER_SERVER",
            ));
        }
        return resolve_direct_target(endpoint, connect_auth_token_env, connect_auth_token);
    }

    if connect_auth_token_env.is_some() || connect_auth_token.is_some() {
        return Err(usage_error(
            "--connect-auth-token and --connect-auth-token-env require --connect",
        ));
    }
    resolve_target(config, server.as_deref())
}

async fn with_client<F, Fut>(
    config: &AppConfig,
    connect: Option<&str>,
    connect_auth_token_env: Option<&str>,
    connect_auth_token: Option<&str>,
    server: Option<String>,
    f: F,
) -> Result<i32>
where
    F: FnOnce(Target, RpcClient) -> Fut,
    Fut: std::future::Future<Output = Result<i32>>,
{
    let target = resolve_target_for_command(
        config,
        connect,
        connect_auth_token_env,
        connect_auth_token,
        server,
    )?;
    let client = RpcClient::connect(&target.endpoint).await?;
    f(target, client).await
}

async fn servers_command(
    config_path: &std::path::Path,
    connect: Option<&str>,
    connect_auth_token_env: Option<&str>,
    connect_auth_token: Option<&str>,
    command: &ServersCommand,
) -> Result<i32> {
    let config = if connect.is_some() {
        AppConfig::default()
    } else {
        let config = load_config(config_path)?;
        print_legacy_warnings(&config);
        config
    };
    match &command.command {
        None => {
            if connect_auth_token_env.is_some() || connect_auth_token.is_some() {
                return Err(usage_error(
                    "--connect-auth-token and --connect-auth-token-env are not valid for servers listing",
                ));
            }
            let rows: Vec<_> = config
                .servers
                .iter()
                .map(|(alias, server)| {
                    let endpoint = server.endpoint_display(alias)?;
                    Ok(json!({"alias": alias, "endpoint": endpoint}))
                })
                .collect::<Result<Vec<_>>>()?;
            if command.json {
                print_json(&json!({ "servers": rows }))?;
            } else {
                print_table(
                    &["ALIAS", "ENDPOINT"],
                    rows.iter()
                        .map(|row| {
                            vec![
                                table_cell(row["alias"].as_str().unwrap_or("")),
                                table_cell(row["endpoint"].as_str().unwrap_or("")),
                            ]
                        })
                        .collect(),
                );
            }
            Ok(0)
        }
        Some(ServersSubcommand::Ping(ping)) => {
            if connect.is_some() && ping.all {
                return Err(usage_error(
                    "--connect cannot be combined with servers ping --all",
                ));
            }
            if connect.is_some()
                && (ping.server.is_some() || std::env::var("CODEX_TAMER_SERVER").is_ok())
            {
                return Err(usage_error(
                    "--connect is mutually exclusive with --server and CODEX_TAMER_SERVER",
                ));
            }
            if ping.all {
                let mut results = Vec::new();
                for (server, cfg) in &config.servers {
                    let ok = match Target::configured(server, cfg, &config) {
                        Ok(target) => RpcClient::connect(&target.endpoint).await.is_ok(),
                        Err(_) => false,
                    };
                    results.push(json!({"server": server, "ok": ok}));
                }
                return render_server_ping_results(results, ping.json);
            }

            let targets = {
                let target = if let Some(endpoint) = connect {
                    resolve_direct_target(endpoint, connect_auth_token_env, connect_auth_token)?
                } else {
                    if connect_auth_token_env.is_some() || connect_auth_token.is_some() {
                        return Err(usage_error(
                            "--connect-auth-token and --connect-auth-token-env require --connect",
                        ));
                    }
                    resolve_target(&config, ping.server.as_deref())?
                };
                vec![target]
            };
            let mut results = Vec::new();
            for target in targets {
                let ok = RpcClient::connect(&target.endpoint).await.is_ok();
                results.push(json!({"server": target.server, "ok": ok}));
            }
            render_server_ping_results(results, ping.json)
        }
    }
}

fn render_server_ping_results(results: Vec<Value>, json_output: bool) -> Result<i32> {
    if json_output {
        print_json(&json!({"servers": results}))?;
    } else {
        print_table(
            &["SERVER", "STATUS"],
            results
                .iter()
                .map(|row| {
                    vec![
                        table_cell(row["server"].as_str().unwrap_or("")),
                        table_cell(if row["ok"].as_bool() == Some(true) {
                            "ok"
                        } else {
                            "error"
                        }),
                    ]
                })
                .collect(),
        );
    }
    Ok(if results.iter().all(|r| r["ok"].as_bool() == Some(true)) {
        0
    } else {
        3
    })
}

async fn list_command(target: Target, mut client: RpcClient, command: ListCommand) -> Result<i32> {
    let since = command.since.as_deref().map(parse_since).transpose()?;
    let limit = command.limit.unwrap_or(DEFAULT_LIST_LIMIT);
    let result = list_threads(
        &target,
        &mut client,
        ListThreadsRequest {
            limit,
            cursor: command.cursor,
            since,
            cwd: command.cwd,
            archived: command.archived,
            is_pinned: command
                .pinned
                .then_some(true)
                .or(command.unpinned.then_some(false)),
            model_providers: command.model_providers,
            source_kinds: command.source_kinds,
            parent_thread_id: command.parent_thread,
            ancestor_thread_id: command.ancestor_thread,
            sort: command.sort,
            asc: command.asc,
            desc: command.desc,
        },
    )
    .await?;
    emit_threads_result(&target, command.json, result, ThreadProjection::Direct)
}

async fn search_threads_command(
    target: Target,
    mut client: RpcClient,
    command: SearchThreadsCommand,
) -> Result<i32> {
    let since = command.since.as_deref().map(parse_since).transpose()?;
    let limit = command.limit.unwrap_or(DEFAULT_LIST_LIMIT);
    let result = search_threads(
        &target,
        &mut client,
        SearchThreadsRequest {
            query: command.query,
            limit,
            cursor: command.cursor,
            since,
            archived: command.archived,
            source_kinds: Vec::new(),
        },
    )
    .await?;
    emit_threads_result(
        &target,
        command.json,
        result,
        ThreadProjection::SearchResult,
    )
}

async fn show_command(target: Target, mut client: RpcClient, command: ShowCommand) -> Result<i32> {
    let result = read_thread_detail(
        &target,
        &mut client,
        ShowThreadRequest {
            thread_id: command.thread_id,
            last: command.last.unwrap_or(DEFAULT_SHOW_LAST),
            cursor: command.cursor,
            asc: command.asc,
            desc: command.desc,
            items: command.items,
        },
    )
    .await?;
    if command.json {
        print_json(&result)?;
    } else {
        print_thread_detail(&result);
    }
    Ok(0)
}

async fn messages_command(
    target: Target,
    mut client: RpcClient,
    command: MessagesCommand,
) -> Result<i32> {
    let since = command.since.as_deref().map(parse_since).transpose()?;
    let result = load_messages(
        &target,
        &mut client,
        MessagesRequest {
            thread_id: command.thread_id,
            last: command.last,
            since,
            role: command.role,
            max_turns: command.max_turns,
        },
    )
    .await?;
    let output = result.output;
    let filtered_role = result.filtered_role.map(message_role_name);
    if command.json {
        print_json(&output)?;
    } else {
        print_messages(
            output["messages"].as_array().unwrap_or(&Vec::new()),
            filtered_role,
        );
        if output["truncated"].as_bool() == Some(true) {
            eprintln!("warning: message scan truncated; increase --max-turns for a wider scan");
        }
    }
    Ok(0)
}

async fn new_command(
    target: Target,
    mut client: RpcClient,
    command: NewCommand,
    yolo: bool,
) -> Result<i32> {
    if command.prompt.is_none() && (command.no_wait || command.stream) {
        return Err(usage_error(
            "new without PROMPT cannot use --no-wait or --stream",
        ));
    }
    let thread_model = command.model.clone().or_else(|| target.model.clone());
    let thread_effort = command
        .effort
        .clone()
        .or_else(|| target.model_reasoning_effort.clone());
    if let Some(effort) = thread_effort.as_deref() {
        validate_effort(effort)?;
    }
    let start = start_thread(
        &mut client,
        &command.cwd,
        ThreadStartOptions {
            model: thread_model,
            effort: thread_effort,
            service_tier: command.service_tier.clone(),
            yolo,
        },
    )
    .await?;
    let thread_id = thread_id_from_start(&start)?;
    if let Some(name) = &command.name {
        set_thread_name(&mut client, &thread_id, name).await?;
    }
    if let Some(prompt) = command.prompt {
        let turn = TurnOptions {
            model: command.model,
            effort: command.effort,
            service_tier: command.service_tier,
            json: command.json,
            stream: command.stream,
            no_wait: command.no_wait,
            yolo,
        };
        return start_turn(target, client, thread_id, prompt, turn).await;
    }
    let output = json!({"server": target.server, "threadId": thread_id, "thread": start["thread"], "model": start["model"], "effort": start["reasoningEffort"], "serviceTier": start["serviceTier"]});
    if command.json {
        print_json(&output)?;
    } else {
        print_key_values(&[
            ("server", target.server.as_str()),
            ("threadId", output["threadId"].as_str().unwrap_or("")),
        ]);
    }
    Ok(0)
}

async fn fork_command(
    target: Target,
    mut client: RpcClient,
    command: ForkCommand,
    yolo: bool,
) -> Result<i32> {
    let thread_model = command.model.clone();
    let thread_effort = command.effort.clone();
    if let Some(effort) = thread_effort.as_deref() {
        validate_effort(effort)?;
    }
    let fork = fork_thread(
        &mut client,
        &command.thread_id,
        ThreadForkOptions {
            last_turn_id: command.last_turn.clone(),
            model: thread_model,
            effort: thread_effort,
            service_tier: command.service_tier.clone(),
            yolo,
        },
    )
    .await?;
    let thread_id = thread_id_from_fork(&fork)?;
    if let Some(name) = &command.name {
        set_thread_name(&mut client, &thread_id, name).await?;
    }
    let output = json!({
        "server": target.server,
        "threadId": thread_id,
        "forkedFromThreadId": command.thread_id,
        "lastTurnId": command.last_turn,
        "thread": fork["thread"],
        "model": fork["model"],
        "effort": fork["reasoningEffort"],
        "serviceTier": fork["serviceTier"]
    });
    if command.json {
        print_json(&output)?;
    } else {
        print_key_values(&[
            ("server", output["server"].as_str().unwrap_or("")),
            ("threadId", output["threadId"].as_str().unwrap_or("")),
            (
                "forkedFromThreadId",
                output["forkedFromThreadId"].as_str().unwrap_or(""),
            ),
        ]);
    }
    Ok(0)
}

async fn send_command(
    target: Target,
    client: RpcClient,
    command: SendCommand,
    yolo: bool,
) -> Result<i32> {
    start_turn(
        target,
        client,
        command.thread_id,
        command.prompt,
        TurnOptions {
            model: command.model,
            effort: command.effort,
            service_tier: command.service_tier,
            json: command.json,
            stream: command.stream,
            no_wait: command.no_wait,
            yolo,
        },
    )
    .await
}

struct TurnOptions {
    model: Option<String>,
    effort: Option<String>,
    service_tier: Option<String>,
    json: bool,
    stream: bool,
    no_wait: bool,
    yolo: bool,
}

async fn start_turn(
    target: Target,
    mut client: RpcClient,
    thread_id: String,
    prompt: String,
    options: TurnOptions,
) -> Result<i32> {
    if let Some(effort) = options.effort.as_deref() {
        validate_effort(effort)?;
    }
    let json_out = options.json;
    let stream = options.stream;
    let no_wait = options.no_wait;

    let started = start_turn_request(
        &target,
        &mut client,
        thread_id,
        prompt,
        TurnStartOptions {
            model: options.model,
            effort: options.effort,
            service_tier: options.service_tier,
            yolo: options.yolo,
        },
    )
    .await?;
    if json_out && stream {
        write_json_line(&started.acceptance)?;
    } else if json_out && no_wait {
        print_json(&started.acceptance)?;
    } else if !json_out {
        print_key_values(&[
            ("server", target.server.as_str()),
            ("threadId", started.thread_id.as_str()),
            ("turnId", started.turn_id.as_str()),
            ("status", "accepted"),
        ]);
    }
    if no_wait {
        return Ok(0);
    }

    let outcome = wait_for_turn(
        &target,
        &mut client,
        started,
        TURN_SCAN_LIMIT,
        Duration::from_secs(TURN_WAIT_TIMEOUT_SECS),
        !(json_out && stream),
        |event| {
            if json_out && stream {
                write_json_line(event)?;
            } else if !json_out {
                print_human_event(event);
            }
            Ok(())
        },
    )
    .await?;
    match outcome {
        TurnWaitOutcome::Terminal(terminal) => {
            emit_turn_terminal_output(json_out, stream, &terminal, target.server.as_str())
        }
        TurnWaitOutcome::LocalInterrupt { thread_id, turn_id } => Ok(emit_local_interrupt(
            target.server.as_str(),
            thread_id.as_str(),
            turn_id.as_str(),
        )),
    }
}

fn emit_local_interrupt(server: &str, thread_id: &str, turn_id: &str) -> i32 {
    eprintln!("interrupted locally; turn is still running");
    eprint!(
        "{}",
        key_values_text(&[
            ("server", server),
            ("threadId", thread_id),
            ("turnId", turn_id),
        ])
    );
    130
}

fn emit_turn_terminal_output(
    json_out: bool,
    stream: bool,
    terminal: &TurnTerminal,
    server: &str,
) -> Result<i32> {
    if json_out && !stream {
        print_json(&terminal.output)?;
    } else if !json_out {
        if terminal
            .output
            .get("progress")
            .and_then(Value::as_array)
            .is_some_and(|events| events.iter().any(|event| event.get("delta").is_some()))
        {
            println!();
        }
        print_key_values(&[
            ("status", terminal.output["status"].as_str().unwrap_or("")),
            ("server", server),
            (
                "threadId",
                terminal.output["threadId"].as_str().unwrap_or(""),
            ),
            ("turnId", terminal.output["turnId"].as_str().unwrap_or("")),
        ]);
    }
    Ok(terminal.exit_code)
}

async fn wait_command(
    target: Target,
    mut client: RpcClient,
    command: WaitCommand,
    yolo: bool,
) -> Result<i32> {
    if command.timeout == 0 {
        return Err(usage_error("--timeout must be greater than zero"));
    }
    let outcome = attach_turn(
        &target,
        &mut client,
        AttachTurnOptions {
            thread_id: command.thread_id,
            turn_id: command.turn_id,
            yolo,
            poll_limit: TURN_SCAN_LIMIT,
            timeout: Duration::from_secs(command.timeout),
            retain_progress: true,
        },
        |_| Ok(()),
    )
    .await?;
    let terminal = match outcome {
        TurnWaitOutcome::Terminal(terminal) => terminal,
        TurnWaitOutcome::LocalInterrupt { thread_id, turn_id } => {
            return Ok(emit_local_interrupt(
                target.server.as_str(),
                thread_id.as_str(),
                turn_id.as_str(),
            ));
        }
    };
    if command.json {
        print_json(&terminal.output)?;
    } else {
        print_key_values(&[
            ("server", target.server.as_str()),
            (
                "threadId",
                terminal.output["threadId"].as_str().unwrap_or(""),
            ),
            ("turnId", terminal.output["turnId"].as_str().unwrap_or("")),
            ("status", terminal.output["status"].as_str().unwrap_or("")),
            (
                "finalAssistantText",
                terminal.output["finalAssistantText"].as_str().unwrap_or(""),
            ),
        ]);
    }
    Ok(terminal.exit_code)
}

async fn result_command(
    target: Target,
    mut client: RpcClient,
    command: ResultCommand,
) -> Result<i32> {
    if command.max_turns == 0 {
        return Err(usage_error("--max-turns must be greater than zero"));
    }
    let output = read_turn_result(
        &target,
        &mut client,
        &command.thread_id,
        &command.turn_id,
        command.max_turns,
    )
    .await?;
    if command.json {
        print_json(&output)?;
    } else {
        print_key_values(&[
            ("server", target.server.as_str()),
            ("threadId", output["threadId"].as_str().unwrap_or("")),
            ("turnId", output["turnId"].as_str().unwrap_or("")),
            ("status", output["status"].as_str().unwrap_or("")),
            (
                "finalAssistantText",
                output["finalAssistantText"].as_str().unwrap_or(""),
            ),
        ]);
    }
    Ok(match output["status"].as_str() {
        Some("failed" | "interrupted") => 1,
        _ => 0,
    })
}

async fn events_follow_command(
    target: Target,
    mut client: RpcClient,
    command: EventsFollowCommand,
    yolo: bool,
) -> Result<i32> {
    if command.timeout == 0 {
        return Err(usage_error("--timeout must be greater than zero"));
    }
    let outcome = attach_turn(
        &target,
        &mut client,
        AttachTurnOptions {
            thread_id: command.thread_id,
            turn_id: command.turn_id,
            yolo,
            poll_limit: TURN_SCAN_LIMIT,
            timeout: Duration::from_secs(command.timeout),
            retain_progress: false,
        },
        write_json_line,
    )
    .await?;
    match outcome {
        TurnWaitOutcome::Terminal(terminal) => Ok(terminal.exit_code),
        TurnWaitOutcome::LocalInterrupt { thread_id, turn_id } => Ok(emit_local_interrupt(
            target.server.as_str(),
            thread_id.as_str(),
            turn_id.as_str(),
        )),
    }
}

async fn inject_command(
    target: Target,
    mut client: RpcClient,
    command: InjectCommand,
    yolo: bool,
) -> Result<i32> {
    let items = load_injected_items(command.items_json.as_deref(), command.items_file.as_deref())?;
    let item_count = items
        .as_array()
        .map(Vec::len)
        .expect("validated injected items");
    let thread_id = command.thread_id.clone();
    let result = request_with_resume_retry(
        &mut client,
        "thread/inject_items",
        json!({"threadId": thread_id, "items": items}),
        &command.thread_id,
        yolo,
        || {},
        |_| {},
    )
    .await?;
    if !result.is_object() {
        return Err(app_server_error(
            "thread/inject_items response must be an object",
        ));
    }
    let output = json!({
        "server": target.server,
        "threadId": command.thread_id,
        "status": "accepted",
        "itemCount": item_count
    });
    emit_json_or_status(command.json, &output)
}

fn load_injected_items(
    items_json: Option<&str>,
    items_file: Option<&std::path::Path>,
) -> Result<Value> {
    let raw = if let Some(raw) = items_json {
        reject_oversized_injected_items(raw)?;
        raw.to_string()
    } else if let Some(path) = items_file {
        if path.as_os_str() == "-" {
            read_injected_items(io::stdin(), "stdin")?
        } else {
            let file = fs::File::open(path).map_err(|err| {
                usage_error(format!(
                    "failed to read injected items from `{}`: {err}",
                    path.display()
                ))
            })?;
            read_injected_items(file, &format!("`{}`", path.display()))?
        }
    } else {
        return Err(usage_error("provide --items-json or --items-file"));
    };
    let items: Value = serde_json::from_str(&raw)
        .map_err(|err| usage_error(format!("injected items are not valid JSON: {err}")))?;
    let Some(items_array) = items.as_array() else {
        return Err(usage_error("injected items must be a non-empty JSON array"));
    };
    if items_array.is_empty() {
        return Err(usage_error("injected items must be a non-empty JSON array"));
    }
    if items_array.iter().any(|item| !item.is_object()) {
        return Err(usage_error("every injected item must be a JSON object"));
    }
    Ok(items)
}

fn read_injected_items(reader: impl Read, source: &str) -> Result<String> {
    let mut raw = String::new();
    reader
        .take((MAX_INJECT_JSON_BYTES + 1) as u64)
        .read_to_string(&mut raw)
        .map_err(|err| {
            usage_error(format!(
                "failed to read injected items from {source}: {err}"
            ))
        })?;
    reject_oversized_injected_items(&raw)?;
    Ok(raw)
}

fn reject_oversized_injected_items(raw: &str) -> Result<()> {
    if raw.len() > MAX_INJECT_JSON_BYTES {
        return Err(usage_error(format!(
            "injected items exceed the {MAX_INJECT_JSON_BYTES}-byte limit"
        )));
    }
    Ok(())
}

fn print_legacy_warnings(config: &AppConfig) {
    for warning in legacy_server_warnings(config) {
        eprintln!("warning: {warning}");
    }
}

async fn settings_show_command(
    target: Target,
    mut client: RpcClient,
    command: SettingsShowCommand,
) -> Result<i32> {
    let result = resume_thread_for_inspection(&mut client, &command.thread_id).await?;
    let output = json!({
        "server": target.server,
        "threadId": command.thread_id,
        "model": result["model"].clone(),
        "effort": result["reasoningEffort"].clone(),
        "serviceTier": result["serviceTier"].clone(),
        "cwd": result["cwd"].clone()
    });
    if command.json {
        print_json(&output)?;
    } else {
        print_key_values(&[
            ("model", output["model"].as_str().unwrap_or("")),
            ("effort", output["effort"].as_str().unwrap_or("")),
            ("serviceTier", output["serviceTier"].as_str().unwrap_or("")),
            ("cwd", output["cwd"].as_str().unwrap_or("")),
        ]);
    }
    Ok(0)
}

async fn settings_set_command(
    target: Target,
    mut client: RpcClient,
    command: SettingsSetCommand,
    yolo: bool,
) -> Result<i32> {
    if command.model.is_none()
        && command.effort.is_none()
        && command.service_tier.is_none()
        && !command.clear_service_tier
    {
        return Err(usage_error(
            "settings set requires at least one setting flag",
        ));
    }
    let mut params = Map::new();
    params.insert("threadId".to_string(), json!(command.thread_id.clone()));
    insert_opt(&mut params, "model", command.model.clone());
    if let Some(effort) = command.effort.as_deref() {
        validate_effort(effort)?;
        params.insert("effort".to_string(), json!(effort));
    }
    if command.clear_service_tier {
        params.insert("serviceTier".to_string(), Value::Null);
    } else if let Some(tier) = &command.service_tier {
        params.insert("serviceTier".to_string(), json!(tier));
    }
    let thread_id = command.thread_id.clone();
    let result = request_with_resume_retry(
        &mut client,
        "thread/settings/update",
        Value::Object(params),
        &thread_id,
        yolo,
        || {},
        |_| {},
    )
    .await?;
    validate_object_response("thread/settings/update", &result)?;
    let output = json!({"server": target.server, "threadId": command.thread_id, "status": "accepted", "requested": {"model": command.model, "effort": command.effort, "serviceTier": command.service_tier, "clearServiceTier": command.clear_service_tier}});
    emit_json_or_status(command.json, &output)
}

async fn status_command(
    target: Target,
    mut client: RpcClient,
    command: StatusCommand,
) -> Result<i32> {
    if let Some(thread_id) = command.thread_id {
        let output = thread_status(
            &target,
            &mut client,
            ThreadStatusRequest {
                thread_id: thread_id.clone(),
                load: command.load,
                turn_scan_limit: TURN_SCAN_LIMIT,
            },
        )
        .await?;
        if command.json {
            print_json(&output)?;
        } else {
            print_key_values(&[
                ("server", target.server.as_str()),
                ("threadId", thread_id.as_str()),
                (
                    "status",
                    output["thread"]["status"]["type"].as_str().unwrap_or(""),
                ),
                (
                    "activeTurnId",
                    output["activeTurnId"].as_str().unwrap_or(""),
                ),
            ]);
        }
    } else {
        let output = loaded_status(
            &target,
            &mut client,
            LoadedStatusRequest {
                limit: DEFAULT_LIST_LIMIT,
            },
        )
        .await?;
        if command.json {
            print_json(&output)?;
        } else {
            print_key_values(&[("server", target.server.as_str()), ("reachable", "true")]);
            if let Some(loaded) = output["loadedThreadIds"]
                .as_array()
                .filter(|loaded| !loaded.is_empty())
            {
                println!();
                print_table(
                    &["LOADED THREAD ID"],
                    loaded
                        .iter()
                        .map(|id| vec![table_cell(id.as_str().unwrap_or(""))])
                        .collect(),
                );
            }
        }
    }
    Ok(0)
}

async fn steer_command(
    target: Target,
    mut client: RpcClient,
    command: SteerCommand,
    yolo: bool,
) -> Result<i32> {
    let output = steer_turn_request(
        &target,
        &mut client,
        command.thread_id,
        command.turn_id,
        command.prompt,
        yolo,
    )
    .await?;
    emit_json_or_status(command.json, &output)
}

async fn interrupt_command(
    target: Target,
    mut client: RpcClient,
    command: InterruptCommand,
) -> Result<i32> {
    let output =
        interrupt_turn_request(&target, &mut client, command.thread_id, command.turn_id).await?;
    emit_json_or_status(command.json, &output)
}

async fn name_command(target: Target, mut client: RpcClient, command: NameCommand) -> Result<i32> {
    set_thread_name(&mut client, &command.thread_id, &command.name).await?;
    let output = json!({"server": target.server, "threadId": command.thread_id, "name": command.name, "status": "accepted"});
    emit_json_or_status(command.json, &output)
}

async fn set_thread_name(client: &mut RpcClient, thread_id: &str, name: &str) -> Result<()> {
    let result = client
        .request(
            "thread/name/set",
            json!({"threadId": thread_id, "name": name}),
            |_| {},
        )
        .await?;
    validate_object_response("thread/name/set", &result)?;
    Ok(())
}

async fn archive_command(
    target: Target,
    mut client: RpcClient,
    command: ThreadOnlyCommand,
    archive: bool,
) -> Result<i32> {
    let method = if archive {
        "thread/archive"
    } else {
        "thread/unarchive"
    };
    let result = client
        .request(method, json!({"threadId": command.thread_id}), |_| {})
        .await?;
    let thread = if archive {
        validate_object_response(method, &result)?;
        Value::Null
    } else {
        validate_thread_response(method, &result, &command.thread_id)?.clone()
    };
    let output = json!({
        "server": target.server,
        "threadId": command.thread_id,
        "archived": archive,
        "status": "accepted",
        "thread": thread
    });
    emit_json_or_status(command.json, &output)
}

async fn pin_command(
    target: Target,
    mut client: RpcClient,
    command: ThreadOnlyCommand,
    pinned: bool,
) -> Result<i32> {
    let result = client
        .request(
            "thread/metadata/update",
            json!({"threadId": command.thread_id, "isPinned": pinned}),
            |_| {},
        )
        .await?;
    let thread = validate_thread_response("thread/metadata/update", &result, &command.thread_id)?;
    let actual_pinned = thread
        .get("isPinned")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            app_server_error("thread/metadata/update response thread.isPinned must be a boolean")
        })?;
    if actual_pinned != pinned {
        return Err(app_server_error(format!(
            "thread/metadata/update response thread.isPinned does not match {pinned}"
        )));
    }
    let output = json!({
        "server": target.server,
        "threadId": command.thread_id,
        "pinned": pinned,
        "status": "accepted",
        "thread": thread
    });
    emit_json_or_status(command.json, &output)
}

async fn annotate_set_command(target: Target, command: AnnotateSetCommand) -> Result<i32> {
    let annotation = set_annotation(&target, &command.thread_id, &command.text)?;
    let output = json!({
        "server": target.server,
        "threadId": command.thread_id,
        "annotation": annotation,
        "status": "accepted"
    });
    if command.json {
        print_json(&output)?;
    } else {
        print_key_values(&[
            ("server", output["server"].as_str().unwrap_or("")),
            ("threadId", output["threadId"].as_str().unwrap_or("")),
            ("status", output["status"].as_str().unwrap_or("accepted")),
        ]);
    }
    Ok(0)
}

async fn annotate_get_command(target: Target, command: AnnotateGetCommand) -> Result<i32> {
    let Some(annotation) = load_annotation(&target, &command.thread_id)? else {
        return Err(ExitError {
            code: 2,
            message: format!("annotation not found for thread `{}`", command.thread_id),
        }
        .into());
    };
    let output = json!({
        "server": target.server,
        "threadId": command.thread_id,
        "annotation": annotation
    });
    if command.json {
        print_json(&output)?;
    } else {
        print_annotation_detail(&output);
    }
    Ok(0)
}

async fn annotate_clear_command(target: Target, command: AnnotateClearCommand) -> Result<i32> {
    let cleared = clear_annotation(&target, &command.thread_id)?;
    let output = json!({
        "server": target.server,
        "threadId": command.thread_id,
        "cleared": cleared,
        "status": "accepted"
    });
    if command.json {
        print_json(&output)?;
    } else {
        print_key_values(&[
            ("server", output["server"].as_str().unwrap_or("")),
            ("threadId", output["threadId"].as_str().unwrap_or("")),
            ("cleared", if cleared { "true" } else { "false" }),
            ("status", output["status"].as_str().unwrap_or("accepted")),
        ]);
    }
    Ok(0)
}

async fn annotate_list_command(target: Target, command: AnnotateListCommand) -> Result<i32> {
    emit_annotation_list(
        list_annotations(&target, command.query.as_deref())?,
        command.json,
    )
}

async fn annotate_search_command(target: Target, command: AnnotateSearchCommand) -> Result<i32> {
    emit_annotation_list(
        list_annotations(&target, Some(&command.query))?,
        command.json,
    )
}

async fn annotate_prune_command(
    target: Target,
    mut client: RpcClient,
    command: AnnotatePruneCommand,
) -> Result<i32> {
    let annotations = list_annotations(&target, None)?;
    let mut stale = Vec::new();
    for item in &annotations {
        match client
            .request(
                "thread/read",
                json!({"threadId": item.thread_id, "includeTurns": false}),
                |_| {},
            )
            .await
        {
            Ok(_) => {}
            Err(err) if is_thread_not_found_error(&err, "thread/read", &item.thread_id) => {
                stale.push(item.thread_id.clone());
            }
            Err(err) => return Err(err),
        }
    }
    let removed = if command.dry_run || stale.is_empty() {
        0
    } else {
        clear_annotations(&target, &stale)?
    };
    let output = json!({
        "server": target.server,
        "checked": annotations.len(),
        "stale": stale,
        "removed": removed,
        "dryRun": command.dry_run
    });
    if command.json {
        print_json(&output)?;
    } else {
        print_key_values(&[
            ("server", output["server"].as_str().unwrap_or("")),
            ("checked", &output["checked"].to_string()),
            (
                "stale",
                &output["stale"]
                    .as_array()
                    .unwrap_or(&Vec::new())
                    .len()
                    .to_string(),
            ),
            ("removed", &output["removed"].to_string()),
            ("dryRun", if command.dry_run { "true" } else { "false" }),
        ]);
    }
    Ok(0)
}

async fn models_command(
    target: Target,
    mut client: RpcClient,
    command: ModelsCommand,
) -> Result<i32> {
    let result = client.request("model/list", json!({}), |_| {}).await?;
    let output = json!({"server": target.server, "models": result["data"], "nextCursor": result["nextCursor"], "backwardsCursor": result["backwardsCursor"]});
    if command.json {
        print_json(&output)?;
    } else {
        print_table(
            &["MODEL", "NAME"],
            output["models"]
                .as_array()
                .unwrap_or(&Vec::new())
                .iter()
                .map(|model| {
                    vec![
                        table_cell(model["id"].as_str().unwrap_or("")),
                        table_cell(
                            model["displayName"]
                                .as_str()
                                .or_else(|| model["name"].as_str())
                                .or_else(|| model["model"].as_str())
                                .unwrap_or(""),
                        ),
                    ]
                })
                .collect(),
        );
    }
    Ok(0)
}

async fn usage_command(
    target: Target,
    mut client: RpcClient,
    command: UsageCommand,
    rate_limit_reset_allowed: bool,
) -> Result<i32> {
    match command.action {
        Some(UsageSubcommand::Redeem) => {
            usage_redeem_command(target, client, command.json, rate_limit_reset_allowed).await
        }
        None => usage_read_command(target, &mut client, command.json).await,
    }
}

async fn usage_read_command(
    target: Target,
    client: &mut RpcClient,
    json_output: bool,
) -> Result<i32> {
    let output = usage_output(
        &target,
        client
            .request("account/rateLimits/read", json!({}), |_| {})
            .await?,
    );
    if json_output {
        print_json(&output)?;
    } else {
        print_usage(&output);
    }
    Ok(0)
}

async fn usage_redeem_command(
    target: Target,
    mut client: RpcClient,
    json_output: bool,
    rate_limit_reset_allowed: bool,
) -> Result<i32> {
    if !rate_limit_reset_allowed {
        return Err(usage_error("rate-limit reset redemption is not permitted"));
    }

    let result = client
        .request("account/rateLimits/read", json!({}), |_| {})
        .await?;
    let credit = select_best_rate_limit_reset_credit(&usage_output(&target, result))?;
    let outcome = client
        .request(
            "account/rateLimitResetCredit/consume",
            json!({
                "idempotencyKey": create_rate_limit_reset_idempotency_key(),
                "creditId": credit.id,
            }),
            |_| {},
        )
        .await?;
    let outcome = validate_rate_limit_reset_outcome(&outcome)?.to_string();
    let (refreshed, refresh_error) = match client
        .request("account/rateLimits/read", json!({}), |_| {})
        .await
    {
        Ok(result) => (Some(usage_output(&target, result)), None),
        Err(err) => (None, Some(err.to_string())),
    };
    let output = json!({
        "server": target.server,
        "outcome": outcome,
        "credit": {
            "id": credit.id,
            "title": credit.title,
            "description": credit.description,
            "grantedAt": credit.granted_at,
            "expiresAt": credit.expires_at,
        },
        "rateLimits": refreshed.as_ref().map(|usage| usage["rateLimits"].clone()).unwrap_or(Value::Null),
        "rateLimitsByLimitId": refreshed.as_ref().map(|usage| usage["rateLimitsByLimitId"].clone()).unwrap_or(Value::Null),
        "rateLimitResetCredits": refreshed.as_ref().map(|usage| usage["rateLimitResetCredits"].clone()).unwrap_or(Value::Null),
        "refreshError": refresh_error,
    });
    if json_output {
        print_json(&output)?;
    } else {
        let title = output["credit"]["title"]
            .as_str()
            .unwrap_or("rate-limit reset");
        println!(
            "Reset request for {title}: {}",
            output["outcome"].as_str().unwrap_or("unknown")
        );
        if output["refreshError"].is_null() {
            print_usage(&output);
        } else {
            eprintln!(
                "Usage refresh unavailable: {}",
                output["refreshError"].as_str().unwrap_or("unknown error")
            );
        }
    }
    Ok(0)
}

fn usage_output(target: &Target, result: Value) -> Value {
    json!({
        "server": target.server,
        "rateLimits": result["rateLimits"],
        "rateLimitsByLimitId": result["rateLimitsByLimitId"],
        "rateLimitResetCredits": result["rateLimitResetCredits"],
    })
}

fn create_rate_limit_reset_idempotency_key() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    format!("codex-tamer-{millis}-{}", std::process::id())
}

async fn goal_get_command(
    target: Target,
    mut client: RpcClient,
    command: GoalGetCommand,
) -> Result<i32> {
    let result = client
        .request(
            "thread/goal/get",
            json!({"threadId": command.thread_id}),
            |_| {},
        )
        .await?;
    let output =
        json!({"server": target.server, "threadId": command.thread_id, "goal": result["goal"]});
    if command.json {
        print_json(&output)?;
    } else {
        let goal = output["goal"].to_string();
        print_key_values(&[("threadId", command.thread_id.as_str()), ("goal", &goal)]);
    }
    Ok(0)
}

async fn goal_set_command(
    target: Target,
    mut client: RpcClient,
    command: GoalSetCommand,
) -> Result<i32> {
    if command.objective.is_none() && command.status.is_none() && command.token_budget.is_none() {
        return Err(usage_error(
            "goal set requires --objective, --status, or --token-budget",
        ));
    }
    let mut params = Map::new();
    params.insert("threadId".to_string(), json!(command.thread_id));
    insert_opt(&mut params, "objective", command.objective);
    if let Some(status) = command.status {
        params.insert("status".to_string(), json!(goal_status(&status)?));
    }
    if let Some(budget) = command.token_budget {
        params.insert("tokenBudget".to_string(), json!(budget));
    }
    let result = client
        .request("thread/goal/set", Value::Object(params.clone()), |_| {})
        .await?;
    let goal = validate_goal_set_response(&result, &command.thread_id, &params)?;
    let output = json!({"server": target.server, "threadId": command.thread_id, "goal": goal, "status": "accepted"});
    emit_json_or_status(command.json, &output)
}

async fn goal_clear_command(
    target: Target,
    mut client: RpcClient,
    command: GoalClearCommand,
) -> Result<i32> {
    let result = client
        .request(
            "thread/goal/clear",
            json!({"threadId": command.thread_id}),
            |_| {},
        )
        .await?;
    let cleared = validate_boolean_response_field("thread/goal/clear", &result, "cleared")?;
    let output = json!({"server": target.server, "threadId": command.thread_id, "cleared": cleared, "status": "accepted"});
    emit_json_or_status(command.json, &output)
}

fn validate_object_response<'a>(method: &str, result: &'a Value) -> Result<&'a Map<String, Value>> {
    result
        .as_object()
        .ok_or_else(|| app_server_error(format!("{method} response must be an object")))
}

fn validate_thread_response<'a>(
    method: &str,
    result: &'a Value,
    expected_thread_id: &str,
) -> Result<&'a Value> {
    let response = validate_object_response(method, result)?;
    let thread = response
        .get("thread")
        .filter(|thread| thread.is_object())
        .ok_or_else(|| app_server_error(format!("{method} response missing thread object")))?;
    let thread_id = thread
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| app_server_error(format!("{method} response thread.id must be a string")))?;
    if thread_id != expected_thread_id {
        return Err(app_server_error(format!(
            "{method} response thread.id `{thread_id}` does not match `{expected_thread_id}`"
        )));
    }
    Ok(thread)
}

fn validate_goal_set_response<'a>(
    result: &'a Value,
    expected_thread_id: &str,
    requested: &Map<String, Value>,
) -> Result<&'a Value> {
    const METHOD: &str = "thread/goal/set";
    let response = validate_object_response(METHOD, result)?;
    let goal = response
        .get("goal")
        .filter(|goal| goal.is_object())
        .ok_or_else(|| app_server_error(format!("{METHOD} response missing goal object")))?;
    let thread_id = goal
        .get("threadId")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            app_server_error(format!("{METHOD} response goal.threadId must be a string"))
        })?;
    if thread_id != expected_thread_id {
        return Err(app_server_error(format!(
            "{METHOD} response goal.threadId `{thread_id}` does not match `{expected_thread_id}`"
        )));
    }
    validate_goal_fields(goal)?;
    for field in ["objective", "status", "tokenBudget"] {
        if let Some(expected) = requested.get(field)
            && goal.get(field) != Some(expected)
        {
            return Err(app_server_error(format!(
                "{METHOD} response goal.{field} does not match the requested value"
            )));
        }
    }
    Ok(goal)
}

fn validate_goal_fields(goal: &Value) -> Result<()> {
    const METHOD: &str = "thread/goal/set";
    if !goal.get("objective").is_some_and(Value::is_string) {
        return Err(app_server_error(format!(
            "{METHOD} response goal.objective must be a string"
        )));
    }
    let status = goal.get("status").and_then(Value::as_str).ok_or_else(|| {
        app_server_error(format!("{METHOD} response goal.status must be a string"))
    })?;
    if !matches!(
        status,
        "active" | "paused" | "blocked" | "usageLimited" | "budgetLimited" | "complete"
    ) {
        return Err(app_server_error(format!(
            "{METHOD} response has unknown goal.status `{status}`"
        )));
    }
    if !goal
        .get("tokenBudget")
        .is_some_and(|value| value.is_null() || value.as_i64().is_some())
    {
        return Err(app_server_error(format!(
            "{METHOD} response goal.tokenBudget must be an integer or null"
        )));
    }
    for field in ["tokensUsed", "timeUsedSeconds", "createdAt", "updatedAt"] {
        if goal.get(field).is_none_or(|value| value.as_i64().is_none()) {
            return Err(app_server_error(format!(
                "{METHOD} response goal.{field} must be an integer"
            )));
        }
    }
    Ok(())
}

fn validate_boolean_response_field(method: &str, result: &Value, field: &str) -> Result<bool> {
    let response = validate_object_response(method, result)?;
    response
        .get(field)
        .and_then(Value::as_bool)
        .ok_or_else(|| app_server_error(format!("{method} response {field} must be a boolean")))
}

fn validate_rate_limit_reset_outcome(result: &Value) -> Result<&str> {
    const METHOD: &str = "account/rateLimitResetCredit/consume";
    let response = validate_object_response(METHOD, result)?;
    let outcome = response
        .get("outcome")
        .and_then(Value::as_str)
        .ok_or_else(|| app_server_error(format!("{METHOD} response outcome must be a string")))?;
    if !matches!(
        outcome,
        "reset" | "nothingToReset" | "noCredit" | "alreadyRedeemed"
    ) {
        return Err(app_server_error(format!(
            "{METHOD} response has unknown outcome `{outcome}`"
        )));
    }
    Ok(outcome)
}

fn print_human_event(event: &Value) {
    if let Some(delta) = event["delta"].as_str() {
        print!("{delta}");
        let _ = io::stdout().flush();
    } else if let Some(text) = event["text"].as_str()
        && !text.is_empty()
    {
        println!("{text}");
    }
}

fn print_messages(messages: &[Value], filtered_role: Option<&str>) {
    for (index, message) in messages.iter().enumerate() {
        if index > 0 {
            println!();
        }
        let timestamp = message["turnStartedAt"]
            .as_i64()
            .or_else(|| message["turnCompletedAt"].as_i64());
        if filtered_role.is_some() {
            println!("{}", format_timestamp(timestamp));
        } else {
            println!(
                "{} {}",
                format_timestamp(timestamp),
                message["role"].as_str().unwrap_or("")
            );
        }
        println!("{}", message["text"].as_str().unwrap_or(""));
    }
}

fn format_timestamp(timestamp: Option<i64>) -> String {
    let Some(timestamp) = timestamp else {
        return "unknown-time".to_string();
    };
    chrono::DateTime::from_timestamp(timestamp, 0)
        .map(|value| {
            value
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| timestamp.to_string())
}

fn message_role_name(role: MessageRole) -> &'static str {
    match role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
    }
}

fn print_thread_detail(result: &Value) {
    let thread = &result["thread"];
    let mut rows = vec![
        ("server", result["server"].as_str().unwrap_or("")),
        ("id", thread["id"].as_str().unwrap_or("")),
        ("name", thread["name"].as_str().unwrap_or("")),
        ("cwd", thread["cwd"].as_str().unwrap_or("")),
        ("status", thread["status"]["type"].as_str().unwrap_or("")),
    ];
    let annotation = thread["annotation"]["text"].as_str();
    if let Some(annotation) = annotation.filter(|text| !text.contains('\n')) {
        rows.push(("annotation", annotation));
    }
    print_key_values(&rows);
    if let Some(annotation) = annotation.filter(|text| text.contains('\n')) {
        println!("annotation");
        for line in annotation.lines() {
            println!("  {line}");
        }
    }
    if let Some(turns) = result["turns"]["data"]
        .as_array()
        .filter(|turns| !turns.is_empty())
    {
        println!();
        print_table(
            &["TURN ID", "STATUS"],
            turns
                .iter()
                .map(|turn| {
                    vec![
                        table_cell(turn["id"].as_str().unwrap_or("")),
                        table_cell(turn_status(turn)),
                    ]
                })
                .collect(),
        );
    }
}

fn print_annotation_detail(result: &Value) {
    print_key_values(&[
        ("server", result["server"].as_str().unwrap_or("")),
        ("threadId", result["threadId"].as_str().unwrap_or("")),
        (
            "annotation",
            result["annotation"]["text"].as_str().unwrap_or(""),
        ),
        (
            "updated",
            &format_timestamp(result["annotation"]["updatedAt"].as_i64()),
        ),
    ]);
}

fn emit_annotation_list(items: Vec<AnnotationListItem>, json_out: bool) -> Result<i32> {
    if json_out {
        let annotations = items
            .iter()
            .map(|item| {
                json!({
                    "server": item.server,
                    "endpoint": item.endpoint,
                    "threadId": item.thread_id,
                    "annotation": item.annotation
                })
            })
            .collect::<Vec<_>>();
        print_json(&json!({"annotations": annotations}))?;
    } else {
        print_table(
            &["UPDATED", "SERVER", "THREAD ID", "ANNOTATION"],
            items
                .iter()
                .map(|item| {
                    vec![
                        table_cell(format_timestamp(Some(item.annotation.updated_at))),
                        table_cell(&item.server),
                        table_cell(&item.thread_id),
                        capped_cell(&item.annotation.text, ANNOTATION_WIDTH),
                    ]
                })
                .collect(),
        );
    }
    Ok(0)
}

fn print_usage(result: &Value) {
    let snapshots = usage_snapshots(result);
    let summary = usage_summary_snapshot(result, &snapshots);
    let plan = summary
        .and_then(|snapshot| snapshot["planType"].as_str())
        .unwrap_or("unknown");
    let reached = summary
        .and_then(|snapshot| snapshot["rateLimitReachedType"].as_str())
        .unwrap_or("none");
    let credits = summary
        .and_then(|snapshot| snapshot.get("credits"))
        .map(format_credits)
        .unwrap_or_else(|| "unknown".to_string());
    let reset_credits = format_rate_limit_reset_credits(result);
    let key_values = [
        ("server", result["server"].as_str().unwrap_or("")),
        ("plan", plan),
        ("credits", credits.as_str()),
        ("resetCredits", reset_credits.as_str()),
        ("limitReached", reached),
    ];
    print_key_values(&key_values);

    if snapshots.is_empty() {
        return;
    }

    println!();
    print_table(
        &["LIMIT", "WINDOW", "USED", "REACHED", "RESETS", "DURATION"],
        snapshots
            .iter()
            .flat_map(|(limit_key, snapshot)| usage_window_rows(limit_key, snapshot))
            .collect(),
    );
}

fn usage_summary_snapshot<'a>(
    result: &'a Value,
    snapshots: &'a [(String, &'a Value)],
) -> Option<&'a Value> {
    if !result["rateLimits"].is_null() {
        Some(&result["rateLimits"])
    } else {
        snapshots.first().map(|(_, snapshot)| *snapshot)
    }
}

fn usage_snapshots(result: &Value) -> Vec<(String, &Value)> {
    let mut snapshots = result["rateLimitsByLimitId"]
        .as_object()
        .map(|by_id| {
            by_id
                .iter()
                .map(|(limit_id, snapshot)| (limit_id.clone(), snapshot))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    snapshots.sort_by(|left, right| left.0.cmp(&right.0));
    if snapshots.is_empty() && !result["rateLimits"].is_null() {
        let fallback_id = result["rateLimits"]["limitId"]
            .as_str()
            .unwrap_or("codex")
            .to_string();
        snapshots.push((fallback_id, &result["rateLimits"]));
    }
    snapshots
}

fn usage_window_rows(limit_key: &str, snapshot: &Value) -> Vec<Vec<TableCell>> {
    let limit = usage_limit_label(limit_key, snapshot);
    let reached = snapshot["rateLimitReachedType"]
        .as_str()
        .unwrap_or("none")
        .to_string();
    ["primary", "secondary"]
        .into_iter()
        .filter_map(|window_name| {
            let window = snapshot.get(window_name)?;
            if window.is_null() {
                return None;
            }
            Some(vec![
                table_cell(limit.clone()),
                table_cell(window_name),
                table_cell(format_used_percent(&window["usedPercent"])),
                table_cell(reached.clone()),
                table_cell(format_timestamp(window["resetsAt"].as_i64())),
                table_cell(format_duration_mins(window["windowDurationMins"].as_i64())),
            ])
        })
        .collect()
}

fn usage_limit_label(limit_key: &str, snapshot: &Value) -> String {
    let limit_id = snapshot["limitId"].as_str().unwrap_or(limit_key);
    match snapshot["limitName"].as_str() {
        Some(name) if name != limit_id => format!("{name} ({limit_id})"),
        Some(name) => name.to_string(),
        None => limit_id.to_string(),
    }
}

fn format_credits(credits: &Value) -> String {
    if credits["unlimited"].as_bool().unwrap_or(false) {
        return "unlimited".to_string();
    }
    match (
        credits["hasCredits"].as_bool(),
        credits["balance"]
            .as_str()
            .filter(|balance| !balance.is_empty()),
    ) {
        (Some(true), Some(balance)) => balance.to_string(),
        (Some(true), None) => "available".to_string(),
        (Some(false), Some(balance)) => format!("depleted ({balance})"),
        (Some(false), None) => "depleted".to_string(),
        (None, Some(balance)) => balance.to_string(),
        (None, None) => "unknown".to_string(),
    }
}

fn format_rate_limit_reset_credits(result: &Value) -> String {
    match result["rateLimitResetCredits"]["availableCount"].as_i64() {
        Some(count) => count.to_string(),
        None => "unknown".to_string(),
    }
}

fn format_used_percent(value: &Value) -> String {
    if let Some(percent) = value.as_i64() {
        return format!("{percent}%");
    }
    if let Some(percent) = value.as_f64() {
        return format!("{percent:.0}%");
    }
    "unknown".to_string()
}

fn format_duration_mins(minutes: Option<i64>) -> String {
    match minutes {
        Some(1) => "1 min".to_string(),
        Some(minutes) => format!("{minutes} mins"),
        None => "unknown".to_string(),
    }
}

fn emit_threads_result(
    target: &Target,
    json_out: bool,
    result: Value,
    projection: ThreadProjection,
) -> Result<i32> {
    let label = match projection {
        ThreadProjection::Direct => "threads",
        ThreadProjection::SearchResult => "results",
    };
    let output = json!({"server": target.server, label: result["data"], "nextCursor": result["nextCursor"], "backwardsCursor": result["backwardsCursor"]});
    if json_out {
        print_json(&output)?;
    } else {
        let empty_items = Vec::new();
        let items = output[label].as_array().unwrap_or(&empty_items);
        let show_annotations = items.iter().any(|item| {
            item.get("thread")
                .unwrap_or(item)
                .get("annotation")
                .is_some()
        });
        let show_parent_threads = items.iter().any(|item| {
            item.get("thread")
                .unwrap_or(item)
                .get("parentThreadId")
                .is_some()
        });
        let show_pinned = items.iter().any(|item| {
            item.get("thread")
                .unwrap_or(item)
                .get("isPinned")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        });
        let mut headers = match projection {
            ThreadProjection::Direct => vec!["UPDATED", "STATUS", "TITLE/PREVIEW"],
            ThreadProjection::SearchResult => {
                vec!["UPDATED", "STATUS", "TITLE/PREVIEW", "SNIPPET"]
            }
        };
        if show_annotations {
            headers.push("ANNOTATION");
        }
        if show_pinned {
            headers.push("PINNED");
        }
        if show_parent_threads {
            headers.push("PARENT ID");
        }
        headers.push("THREAD ID");
        let rows = items
            .iter()
            .map(|item| {
                let thread = item.get("thread").unwrap_or(item);
                let mut row = vec![
                    table_cell(format_timestamp(thread["updatedAt"].as_i64())),
                    table_cell(thread["status"]["type"].as_str().unwrap_or("")),
                    capped_cell(thread_label(thread), THREAD_LABEL_WIDTH),
                ];
                if matches!(projection, ThreadProjection::SearchResult) {
                    row.push(capped_cell(
                        item["snippet"].as_str().unwrap_or(""),
                        SEARCH_SNIPPET_WIDTH,
                    ));
                }
                if show_annotations {
                    row.push(capped_cell(
                        thread["annotation"]["text"].as_str().unwrap_or(""),
                        ANNOTATION_WIDTH,
                    ));
                }
                if show_pinned {
                    row.push(table_cell(if thread["isPinned"].as_bool() == Some(true) {
                        "yes"
                    } else {
                        ""
                    }));
                }
                if show_parent_threads {
                    row.push(table_cell(thread["parentThreadId"].as_str().unwrap_or("")));
                }
                row.push(table_cell(thread["id"].as_str().unwrap_or("")));
                row
            })
            .collect();
        print_table(&headers, rows);
    }
    Ok(0)
}

fn emit_json_or_status(json_out: bool, output: &Value) -> Result<i32> {
    if json_out {
        print_json(output)?;
    } else {
        let mut rows = Vec::new();
        if let Some(server) = output["server"].as_str() {
            rows.push(("server", server));
        }
        if let Some(thread_id) = output["threadId"].as_str() {
            rows.push(("threadId", thread_id));
        }
        if let Some(turn_id) = output["turnId"].as_str() {
            rows.push(("turnId", turn_id));
        }
        rows.push(("status", output["status"].as_str().unwrap_or("accepted")));
        print_key_values(&rows);
    }
    Ok(0)
}

#[derive(Clone)]
struct TableCell {
    text: String,
    max_width: Option<usize>,
}

fn table_cell(text: impl Into<String>) -> TableCell {
    TableCell {
        text: text.into(),
        max_width: None,
    }
}

fn capped_cell(text: impl Into<String>, max_width: usize) -> TableCell {
    TableCell {
        text: text.into(),
        max_width: Some(max_width),
    }
}

fn print_table(headers: &[&str], rows: Vec<Vec<TableCell>>) {
    let rendered_rows = rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(render_table_cell)
                .collect::<Vec<String>>()
        })
        .collect::<Vec<_>>();
    let mut widths = headers
        .iter()
        .map(|header| header.chars().count())
        .collect::<Vec<_>>();
    for row in &rendered_rows {
        for (index, value) in row.iter().enumerate() {
            if index >= widths.len() {
                widths.push(0);
            }
            widths[index] = widths[index].max(value.chars().count());
        }
    }
    print_table_row(
        &headers
            .iter()
            .map(|header| (*header).to_string())
            .collect::<Vec<_>>(),
        &widths,
    );
    for row in rendered_rows {
        print_table_row(&row, &widths);
    }
}

fn print_table_row(row: &[String], widths: &[usize]) {
    for (index, width) in widths.iter().enumerate() {
        if index > 0 {
            print!("  ");
        }
        let value = row.get(index).map(String::as_str).unwrap_or("");
        if index + 1 == widths.len() {
            print!("{value}");
        } else {
            print!("{value:<width$}");
        }
    }
    println!();
}

fn print_key_values(rows: &[(&str, &str)]) {
    print!("{}", key_values_text(rows));
}

fn key_values_text(rows: &[(&str, &str)]) -> String {
    let width = rows
        .iter()
        .map(|(key, _)| key.chars().count())
        .max()
        .unwrap_or_default();
    rows.iter()
        .map(|(key, value)| {
            format!(
                "{key:<width$}  {}",
                sanitize_table_text(value),
                width = width
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn render_table_cell(cell: TableCell) -> String {
    let text = sanitize_table_text(&cell.text);
    match cell.max_width {
        Some(max_width) => truncate_text(&text, max_width),
        None => text,
    }
}

fn sanitize_table_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_text(text: &str, max_width: usize) -> String {
    if text.chars().count() <= max_width {
        return text.to_string();
    }
    if max_width <= 3 {
        return text.chars().take(max_width).collect();
    }
    let mut value = text.chars().take(max_width - 3).collect::<String>();
    value.push_str("...");
    value
}

fn thread_label(thread: &Value) -> &str {
    thread["name"]
        .as_str()
        .or_else(|| thread["preview"].as_str())
        .unwrap_or("")
}

fn print_json(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn write_json_line(value: &Value) -> Result<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    write_json_line_to(&mut stdout, value)
}

fn write_json_line_to(writer: &mut impl Write, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn insert_opt(map: &mut Map<String, Value>, key: &str, value: Option<String>) {
    if let Some(value) = value {
        map.insert(key.to_string(), json!(value));
    }
}

fn turn_status(turn: &Value) -> &'static str {
    match turn["status"].as_str().unwrap_or("inProgress") {
        "completed" => "completed",
        "interrupted" => "interrupted",
        "failed" => "failed",
        _ => "inProgress",
    }
}

fn validate_effort(effort: &str) -> Result<()> {
    if is_valid_reasoning_effort(effort) {
        Ok(())
    } else {
        Err(usage_error(format!("invalid effort `{effort}`")))
    }
}

fn goal_status(status: &str) -> Result<&'static str> {
    match status {
        "active" => Ok("active"),
        "paused" => Ok("paused"),
        "blocked" => Ok("blocked"),
        "usage-limited" => Ok("usageLimited"),
        "budget-limited" => Ok("budgetLimited"),
        "complete" => Ok("complete"),
        _ => Err(usage_error(format!("invalid goal status `{status}`"))),
    }
}

fn classify_error(err: &anyhow::Error) -> i32 {
    if let Some(error) = err.downcast_ref::<ExitError>() {
        return error.code;
    }
    let text = err.to_string();
    if text.contains("auth token requires")
        || text.contains("cannot set both `auth_token`")
        || text.contains("endpoint must")
    {
        return 2;
    }
    if text.contains("requires experimentalApi")
        || text.contains("app-server")
        || text.contains("UDS")
        || text.contains("websocket")
    {
        3
    } else {
        2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct BrokenPipeWriter;

    impl Write for BrokenPipeWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed pipe"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn ndjson_writer_returns_broken_pipe_instead_of_panicking() {
        assert!(write_json_line_to(&mut BrokenPipeWriter, &json!({"type": "accepted"})).is_err());
    }
}
