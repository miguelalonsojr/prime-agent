//! The slash-command ladder (`handle_slash`), the builtin client-command dispatch tail, the
//! command-catalog refresh/fold, and the shared connection-state read.
use super::{
    create_session, effort_picker, info_commands, terminal_columns, AgentView, AuthSelectorKind,
    ChatEntry, CommandCatalogUpdate, DaemonCommand, DockFold, Duration, InfoContent, Map,
    PendingConfirm, RebuildKind, Result, SessionUi, SlashCommandExecution, SlashCommandRegistry,
    StatusKind, SubmitBehavior, Value, UI_REQUEST_TIMEOUT_MS,
};

impl SessionUi {
    /// Slash-command dispatch: builtin client commands without a UI report unavailability, session
    /// commands forward to the session, unknown commands get the suggestion error or pass through
    /// as a prompt; `behavior` passes through, so a slash-prefixed follow-up keeps its lane.
    pub(super) async fn handle_slash(
        &mut self,
        text: &str,
        behavior: SubmitBehavior,
        view: &mut AgentView,
    ) -> Result<()> {
        let registry = SlashCommandRegistry::builtin();
        let (name, args) = pa_types::slash_commands::parse_slash_command(text)
            .unwrap_or_else(|| (String::new(), String::new()));

        // A bare `/skill:<name>` never sends: the daemon's admission seam would hand the model the
        // expanded protocol as its only user message with no task attached.
        if name.starts_with("skill:") && args.trim().is_empty() {
            // The draft restores into the argument position: the next keystrokes
            // become the request.
            view.editor.set_text(&format!("{text} "));
            self.note(
                "add your request after the skill, e.g. /skill:prime-agent-release make a release of PR #2731",
                view,
            );
            return Ok(());
        }

        // Client-local commands this build implements (not TS builtins).
        match name.as_str() {
            "help" => {
                self.note(
                    "/help           this list\n/list           live sessions\n/switch <n|id>  switch to a session from /list\n/new            start a new session\n/clm <action>   accepted model context (on|off|status|view|restore)\n/exit           detach and exit",
                    view,
                );
                return Ok(());
            }
            "list" => {
                self.refresh_list(view).await?;
                return Ok(());
            }
            "switch" => {
                if args.is_empty() {
                    self.note("usage: /switch <n|id> (run /list first)", view);
                } else {
                    self.switch_to(&args, view).await?;
                }
                return Ok(());
            }
            "exit" => {
                self.exit_requested = true;
                return Ok(());
            }
            _ => {}
        }

        let Some(resolved) = registry.parse(text) else {
            // Oversized names are prompts; close typos get the suggestion error,
            // everything else passes through to the model.
            if name.chars().count() > 64 {
                return self.send_prompt(text, behavior, view);
            }
            let candidates = registry.suggestion_candidates();
            return match pa_types::slash_commands::find_slash_command_suggestion(&name, &candidates)
            {
                Some(suggestion) => {
                    self.note(
                        &format!("Unknown command: /{name}. Did you mean /{suggestion}?"),
                        view,
                    );
                    Ok(())
                }
                None => self.send_prompt(text, behavior, view),
            };
        };

        let command = registry
            .get(resolved.name)
            .expect("resolved name is builtin");
        // `agent command used` (TS `captureAgentCommandUsed`): one report
        // per submitted builtin, client and session commands alike, by its
        // canonical name, before the command runs.
        self.track_command_used(resolved.name);
        match command.execution {
            SlashCommandExecution::Session => self.send_prompt(text, behavior, view),
            SlashCommandExecution::Client => {
                self.dispatch_client_command(&resolved, text, view).await
            }
        }
    }

    /// CLM requests remain optional. An unsupported daemon refuses before socket write.
    async fn handle_clm_command(&mut self, args: &str, view: &mut AgentView) {
        let command = match args {
            "status" => DaemonCommand::GetClmStatus {
                id: None,
                active_session_id: self.active_session_id.clone(),
                rest: Map::default(),
            },
            "view" => DaemonCommand::GetClmModelSnapshot {
                id: None,
                active_session_id: self.active_session_id.clone(),
                rest: Map::default(),
            },
            "on" | "off" => DaemonCommand::SetClmMode {
                id: None,
                active_session_id: self.active_session_id.clone(),
                enabled: args == "on",
                rest: Map::default(),
            },
            _ => {
                let Some((epoch, revision)) = parse_clm_restore(args) else {
                    self.error_row(
                        "Usage: /clm on|off|status|view|restore <epoch:revision>",
                        view,
                    );
                    return;
                };
                DaemonCommand::RestoreClmRevision {
                    id: None,
                    active_session_id: self.active_session_id.clone(),
                    epoch_id: epoch.into(),
                    revision,
                    rest: Map::default(),
                }
            }
        };
        match self
            .bounded_request(Duration::from_millis(UI_REQUEST_TIMEOUT_MS), command)
            .await
        {
            Ok(data) => {
                let text = if args == "view" {
                    format_clm_view(&data)
                } else {
                    format_clm_status(&data)
                };
                let rows = text
                    .lines()
                    .map(|line| {
                        vec![info_commands::ClientSpan {
                            text: line.to_string(),
                            color: None,
                        }]
                    })
                    .collect();
                self.open_info_panel(
                    view,
                    Some("CLM accepted context".into()),
                    InfoContent::Rows(rows),
                );
                if args == "on" {
                    self.note("Older Prime Agent binaries can ignore CLM edits and replay the audit. Do not downgrade this session.", view);
                }
            }
            Err(error) => self.error_row(&format!("CLM unavailable: {error:#}"), view),
        }
    }

    /// Create a fresh session and rebind the view to it (the `/new` flow,
    /// shared with the `app.session.new` action).
    pub(super) async fn start_new_session(&mut self, view: &mut AgentView) -> Result<()> {
        let id = create_session(&self.client, &self.create_options(), None).await?;
        self.attach_session(&id, DockFold::Fresh).await?;
        // The tray's context usage rode the attach snapshot: the
        // rebuild copies it into the chrome without a stats round-trip.
        self.rebuild_view(view, &RebuildKind::Rebind);
        // A new session starts with no draft and no prompt history (the submitted
        // `/new` drains the draft already).
        view.editor.clear_history();
        view.editor.set_text("");
        self.note(&format!("started session {id}"), view);
        self.track_feature_outcome("new", "completed", None);
        Ok(())
    }

    /// A builtin client command: only the implemented subset runs locally;
    /// the rest report unavailability.
    async fn dispatch_client_command(
        &mut self,
        resolved: &pa_types::slash_commands::ResolvedSlashCommand,
        text: &str,
        view: &mut AgentView,
    ) -> Result<()> {
        match resolved.name {
            // `/clear` stays the no-argument alias of `/new`.
            "new" if resolved.original_name == "clear" && !resolved.args.is_empty() => {
                self.note("Usage: /clear", view);
            }
            "new" => {
                // The raw suffix after the typed command name (TS parses
                // `text.slice(4)`): the leading separator and the verbatim
                // prompt survive, where the trimmed `resolved.args` would
                // lose them.
                let options = match crate::new_session_command::parse_new_session_command(
                    &text[1 + resolved.original_name.len()..],
                ) {
                    Ok(options) => options,
                    Err(message) => {
                        // The draft the submit drained restores next to the
                        // error row (TS `editor.setText(text)` + `showError`).
                        view.editor.set_text(text);
                        self.error_row(&message, view);
                        return Ok(());
                    }
                };
                self.start_new_session(view).await?;
                if let Some(name) = options.name {
                    if !self.set_session_name(&name, view).await {
                        // TS parity (`handleClearCommand`'s catch): the
                        // rename failed after the session exists — the
                        // prompt restores into the editor and never submits.
                        if let Some(prompt) = options.prompt.as_deref() {
                            view.editor.set_text(prompt);
                        }
                        return Ok(());
                    }
                }
                if let Some(prompt) = options.prompt {
                    view.editor.add_to_history(&prompt);
                    self.send_prompt(&prompt, SubmitBehavior::Steer, view)?;
                }
            }
            // `/quit` detaches and exits (the session keeps running in the daemon).
            "quit" => {
                self.exit_requested = true;
            }
            "resume" => {
                if resolved.args.is_empty() {
                    self.open_agents_view = true;
                    self.exit_requested = true;
                } else {
                    match self.resolve_resume_selector(&resolved.args) {
                        Some(selection) => {
                            self.pending_selection = Some(selection);
                            self.exit_requested = true;
                        }
                        None => {
                            self.note(
                                &format!("could not resolve session \"{}\"", resolved.args),
                                view,
                            );
                        }
                    }
                }
            }
            // `/model` opens the model picker (menu-only: the TS inline-arg form is deliberately
            // removed — a partial + Tab opens the picker filtered instead).
            "model" => {
                if !resolved.args.trim().is_empty() {
                    view.editor
                        .set_text(&format!("/{} {}", resolved.original_name, resolved.args));
                    self.error_row("Usage: /model (Tab filters the picker)", view);
                    return Ok(());
                }
                self.open_model_picker(view, "").await?;
                self.track_menu_opened("model", "command");
                self.track_feature_outcome("model", "initiated", None);
            }
            "effort" => {
                let Some(state) = self.connection_state(view).await else {
                    return Ok(());
                };
                let levels: Vec<String> = state
                    .get("availableThinkingLevels")
                    .and_then(Value::as_array)
                    .map(|levels| {
                        levels
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                // An "off"-only list is no thinking surface.
                let levels: Vec<String> = if levels.len() == 1 && levels[0] == "off" {
                    Vec::new()
                } else {
                    levels
                };
                let current = state
                    .get("thinkingLevel")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                match effort_picker::effort_command(&levels, current.as_deref(), &resolved.args) {
                    effort_picker::EffortCommandOutcome::Open(picker) => {
                        view.choice_picker = Some(picker);
                        self.track_feature_outcome("effort", "initiated", None);
                    }
                    effort_picker::EffortCommandOutcome::Unsupported => {
                        self.note("Current model does not support thinking", view);
                    }
                    effort_picker::EffortCommandOutcome::Unknown { requested, levels } => {
                        // The ⚠ Error row, not the muted note.
                        view.push_entry(ChatEntry::Status {
                            text: format!(
                                "\u{26a0} Error: Unknown thinking level '{requested}'. Available: {}",
                                levels.join(", ")
                            ),
                            kind: StatusKind::Error,
                        });
                        self.dirty = true;
                    }
                    effort_picker::EffortCommandOutcome::Apply { level } => {
                        self.apply_thinking_level(&level, view).await;
                    }
                }
            }
            "tree" => {
                if resolved.args.is_empty() {
                    self.track_feature_outcome("tree", "initiated", None);
                    self.open_tree_selector(view, None).await?;
                } else {
                    self.note("Usage: /tree", view);
                }
            }
            "fork" => {
                if resolved.args.is_empty() {
                    self.track_feature_outcome("fork", "initiated", None);
                    self.open_fork_selector(view).await?;
                } else {
                    self.note("Usage: /fork", view);
                }
            }
            "clone" => {
                if resolved.args.is_empty() {
                    self.track_feature_outcome("clone", "initiated", None);
                    self.handle_clone_command(view).await?;
                } else {
                    self.note("Usage: /clone", view);
                }
            }
            "copy" => {
                if resolved.args.is_empty() {
                    self.handle_copy_command(view).await?;
                } else {
                    view.editor
                        .set_text(&format!("/{} {}", resolved.original_name, resolved.args));
                    self.error_row("Usage: /copy", view);
                }
            }
            // `/login` opens the providers selector (the full configuration menu
            // stays unported).
            "login" => {
                if resolved.args.is_empty() {
                    self.open_provider_auth(AuthSelectorKind::Login, view)
                        .await?;
                    self.track_feature_outcome("login", "initiated", None);
                } else {
                    view.editor
                        .set_text(&format!("/{} {}", resolved.original_name, resolved.args));
                    self.error_row("Usage: /login", view);
                }
            }
            "logout" => {
                if resolved.args.is_empty() {
                    self.open_provider_auth(AuthSelectorKind::Logout, view)
                        .await?;
                    self.track_feature_outcome("logout", "initiated", None);
                } else {
                    view.editor
                        .set_text(&format!("/{} {}", resolved.original_name, resolved.args));
                    self.error_row("Usage: /logout", view);
                }
            }
            "import" => {
                let command_text = if resolved.args.is_empty() {
                    "/import".to_string()
                } else {
                    format!("/import {}", resolved.args)
                };
                self.open_import_confirm(&command_text, view);
            }
            "traces" => {
                self.handle_traces_command(resolved, view).await?;
            }
            // `/nightly [on|off|status]` (TS `interactive-mode.ts`
            // 5455-5484): status resolves the effective channel; on (or
            // bare) and off/stable save the `updateChannel` setting that
            // `/update` and `prime-agent update` follow.
            "nightly" => {
                let arg = resolved.args.trim().to_lowercase();
                if arg == "status" {
                    // The channel resolves through the client-settings seam; a surface
                    // without it never claims a channel.
                    let Some(settings) = &self.client_settings else {
                        self.note("/nightly is not available in this client yet", view);
                        return Ok(());
                    };
                    let preferred = settings.update_channel();
                    let channel = settings.effective_update_channel(&view.chrome.version);
                    let source = if preferred.is_some() {
                        "set in settings"
                    } else {
                        "inferred from the running version"
                    };
                    self.note(
                        &format!(
                            "Updates follow the {channel} channel ({source}). v{} installed.",
                            view.chrome.version
                        ),
                        view,
                    );
                    return Ok(());
                }
                if arg == "off" || arg == "stable" {
                    // The pin persists through the client-settings seam; a surface without
                    // it never claims the pin.
                    let Some(settings) = &self.client_settings else {
                        self.note("/nightly is not available in this client yet", view);
                        return Ok(());
                    };
                    if let Err(error) = settings.set_update_channel("stable") {
                        self.error_row(&format!("{error:#}"), view);
                        return Ok(());
                    }
                    self.note(
                        "Updates now follow the stable channel. Run /update to install the latest build.",
                        view,
                    );
                    return Ok(());
                }
                if !arg.is_empty() && arg != "on" {
                    self.error_row("Usage: /nightly [on|off|status]", view);
                    return Ok(());
                }
                let Some(settings) = &self.client_settings else {
                    self.note("/nightly is not available in this client yet", view);
                    return Ok(());
                };
                if let Err(error) = settings.set_update_channel("nightly") {
                    self.error_row(&format!("{error:#}"), view);
                    return Ok(());
                }
                self.note(
                    "Updates now follow the nightly channel. Run /update to install the latest nightly build.",
                    view,
                );
            }
            // `/factory [on|off|status]`: the factory's opt-in gate (the
            // operator's directive: the factory is disabled until the
            // user turns it on). The toggle persists `factory.enabled` —
            // the shared settings key the daemon's `factory_activity`
            // advertisement and the kernel's factory gate read — so the
            // factory surfaces on the next client start (the running
            // connection keeps the advertisement its hello was built
            // with).
            "factory" => {
                let arg = resolved.args.trim().to_lowercase();
                match arg.as_str() {
                    "on" => {
                        let Some(settings) = &self.client_settings else {
                            self.note("/factory is not available in this client yet", view);
                            return Ok(());
                        };
                        if let Err(error) = settings.set_factory_enabled(true) {
                            self.error_row(&format!("{error:#}"), view);
                            return Ok(());
                        }
                        self.note(
                            "The factory is enabled. Restart the client to surface the factory group (the factory page and the agent-side factory API follow the same gate).",
                            view,
                        );
                    }
                    "off" => {
                        // The lifecycle guard (the review round's finding):
                        // the kernel's control loop is not gated by the
                        // setting, so `off` while runs are live would keep
                        // admitting and collecting children while every
                        // factory surface — the namespace, the activity
                        // lane, and the page after the next client start —
                        // refuses: a running factory loses its stop and
                        // visibility path until the gate is enabled again.
                        // The write refuses while the session's kernel
                        // reports live runs, naming the count the dock
                        // shows; the runs stop first (the page's stop
                        // action or `rlm.factory.stop`). An unreadable
                        // count fails closed on the lane-advertised client
                        // (the only state where the guard matters): the
                        // count's own failure classes — a timed-out or
                        // malformed lane reply — cannot prove zero live
                        // runs, and an unknown liveness must not open the
                        // gate; the client retries once the lane answers.
                        // The kernel-not-running refusal never reaches the
                        // unreadable arm: the lane never builds a kernel
                        // and the kernel owns its run registry in memory,
                        // so that class reads as a definitive zero
                        // (`live_factory_runs`), not an unknown liveness —
                        // the off proceeds for a session with no kernel.
                        // A client whose hello never advertised the lane
                        // keeps the fail-open read: an older daemon has no
                        // executor to protect, and a client started before
                        // `/factory on` already sees the frozen hello the
                        // settled surfaces rule describes.
                        let lane_advertised = self.factory_activity_supported();
                        match self.live_factory_runs().await {
                            Some(live) if live > 0 => {
                                let runs = if live == 1 { "run is" } else { "runs are" };
                                let them = if live == 1 { "it" } else { "them" };
                                self.error_row(
                                    &format!(
                                        "Cannot disable the factory while {live} {runs} still live — stop {them} first (the factory page's stop action or rlm.factory.stop), then /factory off."
                                    ),
                                    view,
                                );
                                return Ok(());
                            }
                            None if lane_advertised => {
                                self.error_row(
                                    "Cannot disable the factory: the live-run count could not be read from the factory lane — try /factory off again once it answers.",
                                    view,
                                );
                                return Ok(());
                            }
                            Some(_) | None => {}
                        }
                        let Some(settings) = &self.client_settings else {
                            self.note("/factory is not available in this client yet", view);
                            return Ok(());
                        };
                        if let Err(error) = settings.set_factory_enabled(false) {
                            self.error_row(&format!("{error:#}"), view);
                            return Ok(());
                        }
                        self.note(
                            "The factory is disabled. The factory group and page disappear on the next client start.",
                            view,
                        );
                    }
                    "" | "status" => {
                        let Some(settings) = &self.client_settings else {
                            self.note("/factory is not available in this client yet", view);
                            return Ok(());
                        };
                        if settings.factory_enabled() {
                            self.note(
                                "The factory is enabled. Run /factory off to disable it.",
                                view,
                            );
                        } else {
                            self.note(
                                "The factory is disabled (off by default). Run /factory on to enable it (takes effect on the next client start).",
                                view,
                            );
                        }
                    }
                    _ => self.error_row("Usage: /factory [on|off|status]", view),
                }
            }
            // `/telemetry [status|on|off]`: the report (on/off and why, the
            // endpoint, the installation id), or the persisted switch the
            // running telemetry clients re-read at their next send.
            "telemetry" => {
                let Some(settings) = &self.client_settings else {
                    self.note("/telemetry is not available in this client yet", view);
                    return Ok(());
                };
                let report = match resolved.args.trim().to_lowercase().as_str() {
                    "" | "status" => Ok(settings.telemetry_status()),
                    "on" => settings.set_telemetry_enabled(true),
                    "off" => settings.set_telemetry_enabled(false),
                    _ => {
                        self.error_row("Usage: /telemetry [status|on|off]", view);
                        return Ok(());
                    }
                };
                match report {
                    Ok(report) => self.note(&report, view),
                    Err(error) => self.error_row(&format!("{error:#}"), view),
                }
            }
            // `/update` (the TS->Rust migration path): the confirm, then
            // the download+install runs OUT-OF-BAND (a background task —
            // the TUI stays mounted, the daemon keeps running, and the
            // update replaces only the on-disk binary, so the new build
            // takes effect on restart and nothing here blocks or tears
            // down; there is no busy guard to keep). The confirm carries
            // the preserve invariant: the update uninstalls the
            // TypeScript version and installs the latest Rust build;
            // sessions and configuration (~/.prime/agent) are never
            // touched.
            "update" => {
                if !resolved.args.trim().is_empty() {
                    view.editor.set_text(text);
                    self.error_row("Usage: /update", view);
                    return Ok(());
                }
                if self.update_in_flight {
                    self.note_as(
                        "An update is already running; its outcome lands here when it finishes.",
                        StatusKind::Warning,
                        view,
                    );
                    return Ok(());
                }
                view.editor.set_text("");
                // The message is hand-wrapped to fit the panel rows (the
                // confirm renders each line truncated, never wrapped).
                view.confirm = Some(crate::confirm::ConfirmPanel::yes_no(
                    "Update Prime Agent",
                    "Update uninstalls the TypeScript version and installs the latest Rust build;\nyour sessions and configuration (~/.prime/agent) are never touched.\n\nThe update runs in the background — restart prime-agent after it\nfinishes to run the new build.",
                ));
                self.pending_confirm = Some(PendingConfirm::Update);
                self.dirty = true;
            }
            // The auth flows run in the client process; other management subcommands
            // surface through the `mcp` CLI command instead of the TUI.
            "mcp" => self.handle_mcp_command(resolved, view).await?,
            // `/plugins [search]` opens the `/mcp` view (this client folds the
            // catalog into it); an argument prefills its search field.
            "plugins" => {
                self.open_mcp_view("/plugins", view, "").await?;
                let search = resolved.args.trim();
                if !search.is_empty() {
                    if let Some(mcp) = view.mcp_view.as_mut() {
                        mcp.paste(search);
                    }
                }
            }
            // An explicit `.jsonl` path exports the current branch; anything else
            // exports HTML.
            "export" => {
                self.handle_export_command(resolved, view).await?;
            }
            // The session exports to a temp file and uploads as a secret gist.
            "share" => {
                if resolved.args.is_empty() {
                    self.handle_share_command(view).await?;
                } else {
                    view.editor
                        .set_text(&format!("/{} {}", resolved.original_name, resolved.args));
                    self.error_row("Usage: /share", view);
                }
            }
            // `/hotkeys`: the full keyboard-shortcut reference renders from the EFFECTIVE bindings
            // so user `keybindings.json` overrides show their keys (client-side rows only).
            "hotkeys" => {
                if !resolved.args.is_empty() {
                    view.editor
                        .set_text(&format!("/{} {}", resolved.original_name, resolved.args));
                    self.error_row("Usage: /hotkeys", view);
                    return Ok(());
                }
                // The operator's 2026-09-26 directive: the guide renders as the read-only
                // info panel instead of the transcript flood.
                self.open_info_panel(
                    view,
                    Some("Hotkeys".to_string()),
                    InfoContent::Markdown(crate::hotkeys::hotkeys_guide(view.editor.keybindings())),
                );
                self.track_menu_opened("hotkeys", "command");
            }

            "session" => {
                if !resolved.args.is_empty() {
                    view.editor.set_text(text);
                    self.error_row("Usage: /session", view);
                    return Ok(());
                }
                let stats = self
                    .bounded_request(
                        Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
                        DaemonCommand::GetSessionStats {
                            id: None,
                            active_session_id: self.active_session_id.clone(),
                            rest: Map::default(),
                        },
                    )
                    .await;
                match stats {
                    Ok(stats) => {
                        let name = self.session_name.clone();
                        self.open_info_panel(
                            view,
                            None,
                            InfoContent::Rows(info_commands::session_info_rows(
                                &stats,
                                name.as_deref(),
                            )),
                        );
                        self.track_menu_opened("session", "command");
                    }
                    Err(error) => {
                        self.error_row(&format!("{error:#}"), view);
                    }
                }
            }
            // `/context` (and its `/usage` alias): the agent tree with token/cost columns and
            // context utilization. The optional `all` argument is a deliberate TS delta (TS takes
            // none): past the row budget the default view collapses to the highest-usage agents;
            // `all` renders the whole tree.
            "clm" => {
                self.handle_clm_command(&resolved.args, view).await;
            }
            "context" => {
                let scope = match resolved.args.as_str() {
                    "" => info_commands::ContextTreeScope::Collapsed,
                    "all" => info_commands::ContextTreeScope::EveryAgent,
                    _ => {
                        view.editor.set_text(text);
                        self.error_row("Usage: /context [all]", view);
                        return Ok(());
                    }
                };
                let tree = self
                    .bounded_request(
                        Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
                        DaemonCommand::GetContextTree {
                            id: None,
                            active_session_id: self.active_session_id.clone(),
                            rest: Map::default(),
                        },
                    )
                    .await;
                match tree {
                    Ok(tree) => {
                        // TS render width: clamp(columns - 2, 60, 120).
                        let width = terminal_columns().saturating_sub(2).clamp(60, 120);
                        self.open_info_panel(
                            view,
                            None,
                            InfoContent::Rows(info_commands::context_tree_rows(
                                &tree, width, scope,
                            )),
                        );
                        self.track_menu_opened("context", "command");
                    }
                    Err(error) => {
                        self.error_row(&format!("{error:#}"), view);
                    }
                }
            }
            // The exact assembled prompt is unbounded, so it renders in the
            // scrollable info panel instead of flooding the transcript.
            "system-prompt" => {
                if !resolved.args.is_empty() {
                    view.editor.set_text(text);
                    self.error_row("Usage: /system-prompt", view);
                    return Ok(());
                }
                let prompt = self
                    .bounded_request(
                        Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
                        DaemonCommand::GetSystemPrompt {
                            id: None,
                            active_session_id: self.active_session_id.clone(),
                            rest: Map::default(),
                        },
                    )
                    .await;
                match prompt {
                    Ok(data) => {
                        let prompt = data
                            .get("systemPrompt")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let mut rows = info_commands::system_prompt_header_rows(prompt);
                        rows.push(Vec::new());
                        rows.extend(info_commands::system_prompt_body_rows(prompt));
                        self.open_info_panel(view, None, InfoContent::Rows(rows));
                        self.track_menu_opened("system-prompt", "command");
                    }
                    Err(error) => {
                        self.error_row(&format!("{error:#}"), view);
                    }
                }
            }
            // A client-side read of the logs directory (the daemon writes it, this
            // client lists it).
            "logs" => {
                if !resolved.args.is_empty() {
                    view.editor.set_text(text);
                    self.error_row("Usage: /logs", view);
                    return Ok(());
                }
                let Some(agent_dir) = pa_types::platform::agent_dir() else {
                    self.error_row(
                        "home directory not found: set HOME (or USERPROFILE on Windows)",
                        view,
                    );
                    return Ok(());
                };
                self.open_info_panel(
                    view,
                    None,
                    InfoContent::Rows(info_commands::logs_rows(&agent_dir.join("logs"))),
                );
                self.track_menu_opened("logs", "command");
            }
            // The shipped CHANGELOG.md entries, newest first; the TS accent `What's
            // New` title is the panel's title.
            "changelog" => {
                if !resolved.args.is_empty() {
                    view.editor.set_text(text);
                    self.error_row("Usage: /changelog", view);
                    return Ok(());
                }
                self.open_info_panel(
                    view,
                    Some("What's New".to_string()),
                    InfoContent::Markdown(info_commands::changelog_markdown(
                        &Self::changelog_path(),
                    )),
                );
                self.track_menu_opened("changelog", "command");
            }
            "settings" => {
                if !resolved.args.is_empty() {
                    view.editor
                        .set_text(&format!("/{} {}", resolved.original_name, resolved.args));
                    self.error_row("Usage: /settings", view);
                    return Ok(());
                }
                self.open_settings_menu(view).await;
            }
            // Start a side question without touching the transcript; the pane stays
            // open for follow-ups until esc returns to the main thread.
            "btw" => {
                if resolved.args.is_empty() {
                    self.note_as("Usage: /btw <question>", StatusKind::Warning, view);
                    return Ok(());
                }
                self.start_side_question(&resolved.args, view).await?;
            }
            "name" => {
                let name = resolved.args.trim();
                if name.is_empty() {
                    match &self.session_name {
                        Some(current) => self.note(&format!("Session name: {current}"), view),
                        None => self.note_as("Usage: /name <name>", StatusKind::Warning, view),
                    }
                    return Ok(());
                }
                self.set_session_name(name, view).await;
            }
            "fast" => {
                if !resolved.args.is_empty() {
                    self.error_row("Usage: /fast", view);
                    return Ok(());
                }
                self.handle_fast_command(view).await;
            }
            "tier" => {
                self.handle_tier_command(view, &resolved.args).await;
            }
            "rlm-max-depth" => {
                self.handle_rlm_max_depth_command(view, &resolved.args)
                    .await;
            }
            "term" => {
                if !resolved.args.is_empty() {
                    self.error_row("Usage: /term", view);
                    return Ok(());
                }
                if cfg!(windows) {
                    self.note_as(
                        "/term is unavailable on Windows.",
                        StatusKind::Warning,
                        view,
                    );
                    return Ok(());
                }
                if !cfg!(any(target_os = "linux", target_os = "macos")) {
                    self.note_as(
                        "/term is unavailable on this platform.",
                        StatusKind::Warning,
                        view,
                    );
                    return Ok(());
                }
                if self.turn_active
                    || view.working.is_some()
                    || self.work_in_flight()
                    || view.compaction.is_some()
                {
                    self.note_as(
                        "Wait for current work to finish before /term.",
                        StatusKind::Warning,
                        view,
                    );
                    return Ok(());
                }
                let shell = self
                    .client_settings
                    .as_ref()
                    .and_then(|settings| settings.shell_path())
                    .or_else(|| std::env::var_os("SHELL").map(std::path::PathBuf::from))
                    .unwrap_or_else(|| std::path::PathBuf::from("/bin/sh"));
                let cwd = if self.kernel_cwd.is_dir() {
                    self.kernel_cwd.clone()
                } else {
                    self.cwd.clone()
                };
                self.note("/term", view);
                self.shell_request = Some((shell, cwd));
            }
            "speed" => {
                let arg = resolved.args.trim().to_lowercase();
                if !arg.is_empty() && arg != "on" && arg != "off" {
                    self.error_row("Usage: /speed [on|off]", view);
                    return Ok(());
                }
                let enable = match arg.as_str() {
                    "on" => true,
                    "off" => false,
                    _ => !self.speed_display_enabled,
                };
                self.set_speed_display(enable, view);
            }
            "reload" => {
                if !resolved.args.is_empty() {
                    view.editor
                        .set_text(&format!("/{} {}", resolved.original_name, resolved.args));
                    self.error_row("Usage: /reload", view);
                    return Ok(());
                }
                if self.turn_active || view.working.is_some() || self.work_in_flight() {
                    self.note_as(
                        "Wait for the current response to finish before reloading.",
                        StatusKind::Warning,
                        view,
                    );
                    return Ok(());
                }
                if view.compaction.is_some() {
                    self.note_as(
                        "Wait for compaction to finish before reloading.",
                        StatusKind::Warning,
                        view,
                    );
                    return Ok(());
                }
                self.handle_reload_command(view)?;
            }
            "heartbeats" => {
                if !resolved.args.is_empty() {
                    view.editor
                        .set_text(&format!("/{} {}", resolved.original_name, resolved.args));
                    self.error_row("Usage: /heartbeats", view);
                    return Ok(());
                }
                self.open_heartbeats_view(view);
            }
            other => {
                self.note(
                    &format!("/{other} is not available in this client yet"),
                    view,
                );
            }
        }
        Ok(())
    }

    /// Rename the active session through the daemon's `SetSessionName` seam —
    /// the one path the `/name` and `/new --name` arms share. On success the
    /// local name, the chrome's chat title, and the `Session name set:` row
    /// update; on failure the error row reports and the caller learns the
    /// rename did not land.
    async fn set_session_name(&mut self, name: &str, view: &mut AgentView) -> bool {
        match self
            .bounded_request(
                Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
                DaemonCommand::SetSessionName {
                    id: None,
                    active_session_id: self.active_session_id.clone(),
                    name: name.to_string(),
                    worker_token: None,
                    rest: Map::default(),
                },
            )
            .await
        {
            Ok(_) => {
                self.session_name = Some(name.to_string());
                view.chrome.chat_name = self.session_display();
                self.plain_row(&format!("Session name set: {name}"), view);
                true
            }
            Err(error) => {
                self.error_row(&format!("{error:#}"), view);
                false
            }
        }
    }

    /// Report a menu surface opening (`tui menu opened`), fire-and-forget;
    /// `menu` names the surface, `source` how it opened.
    pub(super) fn track_menu_opened(&self, menu: &'static str, source: &'static str) {
        if let Some(telemetry) = self.telemetry.clone() {
            tokio::spawn(async move {
                telemetry.menu_opened(menu, source).await;
            });
        }
    }

    /// Fetch the session's slash-command catalog in the background: the response carries the
    /// `skill:` commands; a fetch races a rebind silently — the epoch drops the stale response.
    pub(crate) fn spawn_command_catalog_refresh(&mut self) {
        self.command_refresh_epoch += 1;
        let epoch = self.command_refresh_epoch;
        // The clear rides the same FIFO channel ahead of the fetch's response, so the old rows drop
        // immediately — a rebind never offers stale cross-session commands.
        let _ = self.command_updates.send(CommandCatalogUpdate {
            epoch,
            skill_commands: Vec::new(),
        });
        let updates = self.command_updates.clone();
        let client = self.client.clone();
        let active_session_id = self.active_session_id.clone();
        tokio::spawn(async move {
            let request = DaemonCommand::GetCommands {
                id: None,
                active_session_id,
                rest: Map::default(),
            };
            let fetched = tokio::time::timeout(
                Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
                client.request_ok(request),
            )
            .await;
            let skill_commands = match fetched {
                Ok(Ok(data)) => crate::autocomplete::skill_command_entries(&data),
                // The catalog clears on failure.
                Ok(Err(_)) | Err(_) => Vec::new(),
            };
            let _ = updates.send(CommandCatalogUpdate {
                epoch,
                skill_commands,
            });
        });
    }

    /// Fold a landed command-catalog refresh into the session: the `skill:` commands replace the
    /// provider's list, gated by the `enableSkillCommands` setting; a stale epoch never applies.
    pub(crate) fn apply_command_catalog(
        &mut self,
        update: CommandCatalogUpdate,
        view: &mut AgentView,
    ) {
        if update.epoch < self.command_refresh_epoch {
            return;
        }
        // The cache keeps the raw fetch (the toggle re-applies it under the
        // setting's new value); the default applies when no settings seam exists.
        self.skill_commands_cache = update.skill_commands;
        let skills = if self
            .client_settings
            .as_ref()
            .is_none_or(|settings| settings.enable_skill_commands())
        {
            self.skill_commands_cache.clone()
        } else {
            Vec::new()
        };
        view.editor.set_autocomplete_skill_commands(skills);
        self.dirty = true;
    }

    /// The session's connection state (`get_connection_state` carries the connection fields;
    /// `get_state` serves the roster summary instead). `None` surfaces the failure as a note.
    pub(super) async fn connection_state(&mut self, view: &mut AgentView) -> Option<Value> {
        match self
            .bounded_request(
                Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
                DaemonCommand::GetConnectionState {
                    id: None,
                    active_session_id: self.active_session_id.clone(),
                    rest: Map::default(),
                },
            )
            .await
        {
            Ok(data) => {
                // The queue delivery mode rides the state and is cached here (every state
                // read is the single refresh seam).
                if let Some(mode) = data.get("steeringMode").and_then(Value::as_str) {
                    self.steering_mode = mode.to_string();
                }
                Some(data)
            }
            Err(error) => {
                self.note(&format!("{error:#}"), view);
                None
            }
        }
    }
}

fn parse_clm_restore(args: &str) -> Option<(&str, u64)> {
    let mut parts = args.split_whitespace();
    if parts.next()? != "restore" {
        return None;
    }
    let (epoch, revision_text) = parts.next()?.split_once(':')?;
    if parts.next().is_some()
        || epoch.is_empty()
        || revision_text.is_empty()
        || !revision_text.bytes().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let revision = revision_text.parse::<u64>().ok()?;
    (revision <= 9_007_199_254_740_991).then_some((epoch, revision))
}

fn safe_clm_text(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .collect()
}

fn clm_field<'a>(data: &'a Value, key: &str) -> &'a str {
    data.get(key).and_then(Value::as_str).unwrap_or("unknown")
}

fn clm_safe_integer(value: &Value) -> Option<u64> {
    const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
    value
        .as_u64()
        .filter(|n| *n <= MAX_SAFE_INTEGER)
        .or_else(|| {
            let n = value.as_f64()?;
            (n.is_finite() && n >= 0.0 && n <= MAX_SAFE_INTEGER as f64 && n.fract() == 0.0)
                .then_some(n as u64)
        })
}

fn format_clm_status(data: &Value) -> String {
    let revision = match (
        data.get("epochId").and_then(Value::as_str),
        data.get("revision").and_then(Value::as_u64),
    ) {
        (Some(epoch), Some(revision)) => format!("{epoch}:{revision}"),
        _ => "unknown".into(),
    };
    let recent = data
        .get("recentRevisions")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    Some(format!(
                        "{}:{}",
                        item.get("epochId")?.as_str()?,
                        item.get("revision")?.as_u64()?
                    ))
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|items| !items.is_empty())
        .unwrap_or_else(|| "none".into());
    let occupancy = data.get("estimatedOccupancy").unwrap_or(&Value::Null);
    let number = |key| {
        occupancy
            .get(key)
            .and_then(clm_safe_integer)
            .map_or_else(|| "unknown".into(), |n| n.to_string())
    };
    let usage = data.get("measuredUsage");
    let observed = usage
        .and_then(|u| {
            Some(format!(
                "{} input, {} output, {} cache read, {} cache write, ${}",
                clm_safe_integer(u.get("input")?)?,
                clm_safe_integer(u.get("output")?)?,
                clm_safe_integer(u.get("cacheRead")?)?,
                clm_safe_integer(u.get("cacheWrite")?)?,
                u.get("cost")?
                    .as_f64()
                    .filter(|v| v.is_finite() && *v >= 0.0)?
            ))
        })
        .unwrap_or_else(|| "unknown".into());
    let mut lines = vec![
        format!(
            "CLM: {} (configured default: {}; source: {})",
            clm_field(data, "mode"),
            if data.get("configuredDefault").and_then(Value::as_bool) == Some(true) {
                "on"
            } else {
                "off"
            },
            clm_field(data, "modeSource")
        ),
        format!("Accepted revision: {revision}"),
        format!("Context file: {}", clm_field(data, "path")),
        format!("Recent accepted revisions: {recent}"),
        format!(
            "Estimated occupancy: {} tokens",
            number("estimatedInputTokens")
        ),
        format!(
            "Estimated input budget: {} tokens",
            number("maxInputTokens")
        ),
        format!(
            "Estimated output reserve: {} tokens",
            number("outputReserveTokens")
        ),
        format!("Observed billed usage (current revision): {observed}"),
    ];
    for (key, label) in [("warning", "Warning"), ("error", "Recovery error")] {
        if let Some(value) = data.get(key).and_then(Value::as_str) {
            lines.push(format!("{label}: {value}"));
        }
    }
    safe_clm_text(&lines.join("\n"))
}

fn format_clm_view(data: &Value) -> String {
    let mut lines = vec![format!(
        "Accepted model context {}:{} (read-only)",
        clm_field(data, "epochId"),
        data.get("revision")
            .and_then(Value::as_u64)
            .map_or_else(|| "unknown".into(), |n| n.to_string())
    )];
    if let Some(messages) = data.get("messages").and_then(Value::as_array) {
        for message in messages {
            let content = match message.get("content") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(parts)) => parts
                    .iter()
                    .map(|part| match part.get("type").and_then(Value::as_str) {
                        Some("text") => part
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        Some("image") => format!("[image: {}]", clm_field(part, "mimeType")),
                        _ => part.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                Some(other) => other.to_string(),
                None => message.to_string(),
            };
            lines.push(format!("{}: {content}", clm_field(message, "role")));
        }
    }
    safe_clm_text(&lines.join("\n\n"))
}

#[cfg(test)]
mod clm_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn clm_restore_accepts_zero_and_rejects_invalid_targets() {
        assert_eq!(parse_clm_restore("restore epoch:0"), Some(("epoch", 0)));
        assert_eq!(parse_clm_restore("restore   epoch:000"), Some(("epoch", 0)));
        for invalid in [
            "restore :0",
            "restore epoch:-1",
            "restore epoch:1x",
            "restore epoch:9007199254740992",
            "restore epoch:1:2",
        ] {
            assert_eq!(parse_clm_restore(invalid), None, "{invalid}");
        }
    }

    #[tokio::test]
    async fn clm_renders_status_and_accepted_view_received_from_socket() {
        use crate::daemon_client::DaemonClient;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("clm.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let measured_usage = pa_types::clm::ClmMeasuredUsage {
            input: 2.0,
            output: 3.0,
            cache_read: 4.0,
            cache_write: 5.0,
            cost: 0.5,
        };
        let serialized_usage = serde_json::to_string(&measured_usage).unwrap();
        assert!(
            serialized_usage.contains("\"input\":2.0"),
            "{serialized_usage}"
        );
        let status = json!({"configuredDefault":true,"mode":"on","modeSource":"configured-default",
            "epochId":"accepted","revision":2,"path":"/tmp/synthetic/clm/context.txt",
            "recentRevisions":[{"epochId":"accepted","revision":2}],
            "estimatedOccupancy":{"estimatedInputTokens":10,"maxInputTokens":20,"outputReserveTokens":5},
            "measuredUsage":measured_usage});
        let snapshot = json!({"epochId":"accepted","revision":2,
            "messages":[{"role":"user","content":"accepted\nline\u{1b}[31m"}],"estimatedTokens":10});
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            writer.write_all(format!("{}\n", json!({"type":"daemon_hello","protocol":{"name":"prime-agent.daemon","version":7},
                "schemaRevision":32,"serverCapabilities":["clm_context_v1"]})).as_bytes()).await.unwrap();
            let mut reader = BufReader::new(reader);
            for (kind, data) in [
                ("get_clm_status", status),
                ("get_clm_model_snapshot", snapshot),
            ] {
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let envelope: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(envelope["command"]["type"], kind);
                writer
                    .write_all(
                        format!(
                            "{}\n",
                            json!({"type":"response","id":envelope["id"],
                    "command":kind,"success":true,"data":data})
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            }
        });
        let (client, _) = DaemonClient::connect(&socket).await.unwrap();
        let status = client
            .request_ok(DaemonCommand::GetClmStatus {
                id: None,
                active_session_id: "s1".into(),
                rest: Map::default(),
            })
            .await
            .unwrap();
        let rendered_status = format_clm_status(&status);
        assert!(rendered_status
            .contains("CLM: on (configured default: on; source: configured-default)"));
        assert!(rendered_status.contains("Accepted revision: accepted:2"));
        assert!(rendered_status.contains("Estimated input budget: 20 tokens"));
        assert!(rendered_status.contains("Observed billed usage (current revision): 2 input, 3 output, 4 cache read, 5 cache write, $0.5"), "{rendered_status}");
        let view = client
            .request_ok(DaemonCommand::GetClmModelSnapshot {
                id: None,
                active_session_id: "s1".into(),
                rest: Map::default(),
            })
            .await
            .unwrap();
        let rendered_view = format_clm_view(&view);
        assert!(rendered_view.contains("Accepted model context accepted:2 (read-only)"));
        assert!(rendered_view.contains("user: accepted\nline"));
        assert!(!rendered_view.contains('\u{1b}'));
        server.await.unwrap();
        client.close();
    }

    #[test]
    fn clm_status_hides_unsafe_integer_estimates_and_billed_receipts() {
        let maximum = 9_007_199_254_740_991_u64;
        let valid = json!({"estimatedOccupancy":{"estimatedInputTokens":0,"maxInputTokens":maximum,"outputReserveTokens":0},
            "measuredUsage":{"input":maximum,"output":0,"cacheRead":0,"cacheWrite":0,"cost":0.0}});
        let text = format_clm_status(&valid);
        assert!(text.contains(&format!("Estimated input budget: {maximum} tokens")));
        assert!(text.contains("Estimated occupancy: 0 tokens"));
        assert!(text.contains(&format!(
            "Observed billed usage (current revision): {maximum} input"
        )));
        for field in [
            "estimatedInputTokens",
            "maxInputTokens",
            "outputReserveTokens",
        ] {
            let mut invalid = valid.clone();
            invalid["estimatedOccupancy"][field] = json!(maximum + 1);
            assert!(
                format_clm_status(&invalid).contains(&format!(
                    "{}: unknown tokens",
                    match field {
                        "estimatedInputTokens" => "Estimated occupancy",
                        "maxInputTokens" => "Estimated input budget",
                        _ => "Estimated output reserve",
                    }
                )),
                "{field}"
            );
        }
        for field in ["input", "output", "cacheRead", "cacheWrite"] {
            let mut invalid = valid.clone();
            invalid["measuredUsage"][field] = json!(maximum + 1);
            assert!(
                format_clm_status(&invalid)
                    .contains("Observed billed usage (current revision): unknown"),
                "{field}"
            );
        }
    }

    #[test]
    fn clm_status_accepts_integral_float_numbers_but_not_fractional_or_unsafe() {
        let maximum = 9_007_199_254_740_991_u64;
        for (number, expected) in [
            (json!(0.0), "0".to_string()),
            (json!(maximum as f64), maximum.to_string()),
        ] {
            let status = json!({"estimatedOccupancy":{"estimatedInputTokens":number},
                "measuredUsage":{"input":number,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"cost":0.0}});
            let text = format_clm_status(&status);
            assert!(
                text.contains(&format!("Estimated occupancy: {expected} tokens")),
                "{text}"
            );
            assert!(
                text.contains(&format!(
                    "Observed billed usage (current revision): {expected} input"
                )),
                "{text}"
            );
        }
        for number in [json!(-1.0), json!(1.5), json!((maximum + 1) as f64)] {
            let status = json!({"estimatedOccupancy":{"estimatedInputTokens":number},
                "measuredUsage":{"input":number,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"cost":0.0}});
            let text = format_clm_status(&status);
            assert!(
                text.contains("Estimated occupancy: unknown tokens"),
                "{text}"
            );
            assert!(
                text.contains("Observed billed usage (current revision): unknown"),
                "{text}"
            );
        }
    }

    #[test]
    fn clm_status_and_view_render_accepted_context_without_terminal_controls() {
        let status = json!({"mode":"on","configuredDefault":false,"modeSource":"session-override",
            "epochId":"epoch","revision":2,"path":"/tmp/clm/context.txt",
            "recentRevisions":[{"epochId":"epoch","revision":1}],
            "estimatedOccupancy":{"estimatedInputTokens":100,"maxInputTokens":200,"outputReserveTokens":30},
            "measuredUsage":{"input":11,"output":12,"cacheRead":13,"cacheWrite":14,"cost":0.5},
            "warning":"warn\u{1b}[31m"});
        let text = format_clm_status(&status);
        assert!(text.contains("Accepted revision: epoch:2"));
        assert!(text.contains("Observed billed usage (current revision): 11 input"));
        assert!(!text.contains('\u{1b}'));
        let view = json!({"epochId":"epoch","revision":2,"messages":[{"role":"user","content":"hello\u{1b}[31m"}]});
        let text = format_clm_view(&view);
        assert!(text.contains("Accepted model context epoch:2 (read-only)"));
        assert!(text.contains("user: hello"));
        assert!(!text.contains('\u{1b}'));
    }
}
