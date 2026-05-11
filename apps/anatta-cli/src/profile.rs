//! `anatta profile <…>` subcommands.

use clap::{Subcommand, ValueEnum};
use dialoguer::theme::ColorfulTheme;

use anatta_runtime::profile::{
    AnyProfileId, ClaudeProfile, ClaudeProfileId, CodexProfile, CodexProfileId,
};
use anatta_store::profile::{AuthMethod, BackendKind, NewProfile};

use crate::auth;
use crate::config::Config;

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum ProfileCommand {
    /// Create a new profile (interactive by default; --non-interactive uses flags only).
    Create {
        #[arg(long, value_enum)]
        backend: Option<BackendArg>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, value_enum)]
        auth: Option<AuthArg>,
        /// Provide the API key inline (only with --auth api-key).
        #[arg(long, env = "ANATTA_PROFILE_API_KEY")]
        api_key: Option<String>,
        /// Fail if any required input is missing instead of prompting.
        #[arg(long)]
        non_interactive: bool,

        /// Provider id (e.g. `anthropic`, `deepseek`, `kimi`, `minimax`,
        /// `zai`, `custom`). Defaults to `anthropic` for claude / `openai`
        /// for codex when omitted.
        #[arg(long)]
        provider: Option<String>,
        /// Override `ANTHROPIC_BASE_URL` (required for `--provider custom`).
        #[arg(long)]
        base_url: Option<String>,
        /// Override `ANTHROPIC_MODEL`.
        #[arg(long)]
        model: Option<String>,
        /// Override `ANTHROPIC_SMALL_FAST_MODEL`.
        #[arg(long)]
        small_fast_model: Option<String>,
        /// Override `ANTHROPIC_DEFAULT_OPUS_MODEL`.
        #[arg(long)]
        opus_model: Option<String>,
        /// Override `ANTHROPIC_DEFAULT_SONNET_MODEL`.
        #[arg(long)]
        sonnet_model: Option<String>,
        /// Override `ANTHROPIC_DEFAULT_HAIKU_MODEL`.
        #[arg(long)]
        haiku_model: Option<String>,
        /// Override `CLAUDE_CODE_SUBAGENT_MODEL`.
        #[arg(long)]
        subagent_model: Option<String>,
    },
    /// List all configured profiles.
    List,
    /// Show details about a specific profile.
    Show { id: String },
    /// Delete a profile (removes dir, keyring entry, and DB row).
    Delete {
        id: String,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum BackendArg {
    Claude,
    Codex,
}
impl From<BackendArg> for BackendKind {
    fn from(b: BackendArg) -> Self {
        match b {
            BackendArg::Claude => BackendKind::Claude,
            BackendArg::Codex => BackendKind::Codex,
        }
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum AuthArg {
    Login,
    ApiKey,
}
impl From<AuthArg> for AuthMethod {
    fn from(a: AuthArg) -> Self {
        match a {
            AuthArg::Login => AuthMethod::Login,
            AuthArg::ApiKey => AuthMethod::ApiKey,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProfileCmdError {
    #[error("interactive prompt: {0}")]
    Prompt(#[from] dialoguer::Error),
    #[error("input required: {0} (pass --{0} or omit --non-interactive)")]
    InputRequired(&'static str),
    #[error("backend binary `{0}` not found on PATH; install it before running login")]
    BinaryNotFound(&'static str),
    #[error("profile id has invalid format: {0}")]
    BadId(String),
    #[error("profile not found: {0}")]
    ProfileNotFound(String),
    #[error("profile creation rolled back: {source}")]
    RolledBack {
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("runtime profile: {0}")]
    Profile(#[from] anatta_runtime::profile::ProfileError),
    #[error("store: {0}")]
    Store(#[from] anatta_store::StoreError),
    #[error("auth: {0}")]
    Auth(#[from] auth::AuthError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("unknown provider: {0}")]
    UnknownProvider(String),
    #[error("provider {provider} targets backend {got}, but profile uses backend {expected}")]
    ProviderBackendMismatch {
        provider: String,
        expected: &'static str,
        got: &'static str,
    },
    #[error("provider {provider} does not support auth method {auth}")]
    AuthNotSupportedByProvider {
        provider: String,
        auth: &'static str,
    },
}

pub async fn run(cmd: ProfileCommand, cfg: &Config) -> Result<(), ProfileCmdError> {
    match cmd {
        ProfileCommand::Create {
            backend,
            name,
            auth,
            api_key,
            non_interactive,
            provider,
            base_url,
            model,
            small_fast_model,
            opus_model,
            sonnet_model,
            haiku_model,
            subagent_model,
        } => {
            create(
                cfg, backend, name, auth, api_key, non_interactive,
                provider, base_url, model, small_fast_model,
                opus_model, sonnet_model, haiku_model, subagent_model,
            )
            .await
        }
        ProfileCommand::List => list(cfg).await,
        ProfileCommand::Show { id } => show(cfg, &id).await,
        ProfileCommand::Delete { id, yes } => delete(cfg, &id, yes).await,
    }
}

// ────────────────────────────────────────────────────────────────────────────
// create
// ────────────────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn create(
    cfg: &Config,
    backend_flag: Option<BackendArg>,
    name_flag: Option<String>,
    auth_flag: Option<AuthArg>,
    api_key_flag: Option<String>,
    non_interactive: bool,
    provider_flag: Option<String>,
    base_url_flag: Option<String>,
    model_flag: Option<String>,
    small_fast_model_flag: Option<String>,
    opus_model_flag: Option<String>,
    sonnet_model_flag: Option<String>,
    haiku_model_flag: Option<String>,
    subagent_model_flag: Option<String>,
) -> Result<(), ProfileCmdError> {
    let theme = ColorfulTheme::default();

    // 1. backend
    let backend: BackendKind = match backend_flag {
        Some(b) => b.into(),
        None => {
            if non_interactive {
                return Err(ProfileCmdError::InputRequired("backend"));
            }
            let items = ["claude", "codex"];
            let pick = dialoguer::Select::with_theme(&theme)
                .with_prompt("Backend")
                .default(0)
                .items(&items)
                .interact()?;
            if pick == 0 { BackendKind::Claude } else { BackendKind::Codex }
        }
    };
    let backend_str = backend.as_str();

    // 2. provider (default depends on backend)
    let provider_id: String = match provider_flag {
        Some(p) => p,
        None => {
            if non_interactive {
                // Non-interactive default: anthropic / openai.
                match backend {
                    BackendKind::Claude => "anthropic".to_owned(),
                    BackendKind::Codex => "openai".to_owned(),
                }
            } else {
                let candidates: Vec<&'static anatta_runtime::profile::ProviderSpec> =
                    anatta_runtime::profile::providers::iter_for_backend(backend_str).collect();
                let labels: Vec<String> = candidates
                    .iter()
                    .map(|s| format!("{}  ({:?}, {})", s.display_name, s.tier,
                                     s.supported_auth.join("+")))
                    .collect();
                let pick = dialoguer::Select::with_theme(&theme)
                    .with_prompt("Provider")
                    .default(0)
                    .items(&labels)
                    .interact()?;
                candidates[pick].id.to_owned()
            }
        }
    };
    let spec = anatta_runtime::profile::providers::lookup(&provider_id)
        .ok_or_else(|| ProfileCmdError::UnknownProvider(provider_id.clone()))?;
    if spec.backend != backend_str {
        return Err(ProfileCmdError::ProviderBackendMismatch {
            provider: provider_id.clone(),
            expected: backend_str,
            got: spec.backend,
        });
    }

    // 3. name
    let name: String = match name_flag {
        Some(n) => n,
        None => {
            if non_interactive {
                return Err(ProfileCmdError::InputRequired("name"));
            }
            dialoguer::Input::<String>::with_theme(&theme)
                .with_prompt("Name (label, e.g. work / personal)")
                .interact_text()?
        }
    };

    // 4. auth method (constrained by spec.supported_auth)
    let auth_method: AuthMethod = match auth_flag {
        Some(a) => {
            let am: AuthMethod = a.into();
            if !spec.supported_auth.contains(&am.as_str()) {
                return Err(ProfileCmdError::AuthNotSupportedByProvider {
                    provider: provider_id.clone(),
                    auth: am.as_str(),
                });
            }
            am
        }
        None => {
            // Auto-pick when only one option; prompt otherwise.
            if spec.supported_auth == ["api_key"] {
                AuthMethod::ApiKey
            } else if non_interactive {
                return Err(ProfileCmdError::InputRequired("auth"));
            } else {
                let items: Vec<&str> = spec.supported_auth.to_vec();
                let pick = dialoguer::Select::with_theme(&theme)
                    .with_prompt("Auth method")
                    .default(0)
                    .items(&items)
                    .interact()?;
                AuthMethod::parse(items[pick])
                    .map_err(|_| ProfileCmdError::InputRequired("auth"))?
            }
        }
    };

    // 5. (if api-key) gather the key
    let api_key: Option<String> = if matches!(auth_method, AuthMethod::ApiKey) {
        match api_key_flag {
            Some(k) => Some(k),
            None => {
                if non_interactive {
                    return Err(ProfileCmdError::InputRequired("api-key"));
                }
                Some(
                    dialoguer::Password::with_theme(&theme)
                        .with_prompt("API key")
                        .interact()?,
                )
            }
        }
    } else {
        None
    };

    // 6. (custom provider) require a base_url
    if provider_id == "custom" && base_url_flag.is_none() {
        return Err(ProfileCmdError::InputRequired("base-url"));
    }

    // 7. mint id, create on-disk profile
    let (profile_path, id_string): (std::path::PathBuf, String) = match backend {
        BackendKind::Claude => {
            let id = ClaudeProfileId::new();
            let p = ClaudeProfile::create(id.clone(), &cfg.anatta_home)?;
            (p.path, id.as_str().to_owned())
        }
        BackendKind::Codex => {
            let id = CodexProfileId::new();
            let p = CodexProfile::create(id.clone(), &cfg.anatta_home)?;
            (p.path, id.as_str().to_owned())
        }
    };

    println!(
        "-> Generated id: {}\n-> Provider: {}\n-> Profile dir: {}",
        id_string,
        provider_id,
        profile_path.display()
    );

    // 8. run auth (with rollback on failure)
    let outcome = run_auth(
        backend,
        &profile_path,
        &id_string,
        auth_method,
        api_key.as_deref(),
        cfg,
    )
    .await;
    if let Err(e) = outcome {
        let _ = std::fs::remove_dir_all(&profile_path);
        let _ = auth::delete_api_key(&cfg.anatta_home, &id_string);
        return Err(ProfileCmdError::RolledBack {
            source: Box::new(e),
        });
    }

    // 9. commit DB row
    if let Err(e) = cfg
        .store
        .insert_profile(NewProfile {
            id: &id_string,
            backend,
            name: &name,
            auth_method,
            provider: &provider_id,
            base_url_override: base_url_flag.as_deref(),
            model_override: model_flag.as_deref(),
            small_fast_model_override: small_fast_model_flag.as_deref(),
            default_opus_model_override: opus_model_flag.as_deref(),
            default_sonnet_model_override: sonnet_model_flag.as_deref(),
            default_haiku_model_override: haiku_model_flag.as_deref(),
            subagent_model_override: subagent_model_flag.as_deref(),
        })
        .await
    {
        let _ = std::fs::remove_dir_all(&profile_path);
        let _ = auth::delete_api_key(&cfg.anatta_home, &id_string);
        return Err(e.into());
    }

    println!("✓ {id_string} (\"{name}\") created.");
    Ok(())
}

async fn run_auth(
    backend: BackendKind,
    profile_path: &std::path::Path,
    profile_id: &str,
    method: AuthMethod,
    api_key: Option<&str>,
    cfg: &Config,
) -> Result<(), ProfileCmdError> {
    match method {
        AuthMethod::Login => {
            let bin_name = match backend {
                BackendKind::Claude => "claude",
                BackendKind::Codex => "codex",
            };
            let binary = auth::locate_binary(bin_name)
                .ok_or(ProfileCmdError::BinaryNotFound(bin_name))?;
            println!("-> Launching `{bin_name}` auth flow...");
            auth::run_login(backend, profile_path, &binary).await?;
            Ok(())
        }
        AuthMethod::ApiKey => {
            let key = api_key.expect("api_key path always supplies the key");
            auth::store_api_key(&cfg.anatta_home, profile_id, key)?;
            Ok(())
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// list / show / delete
// ────────────────────────────────────────────────────────────────────────────

async fn list(cfg: &Config) -> Result<(), ProfileCmdError> {
    let rows = cfg.store.list_profiles().await?;
    if rows.is_empty() {
        println!("(no profiles yet — `anatta profile create` to add one)");
        return Ok(());
    }
    println!(
        "{:<24}  {:<8}  {:<10}  {:<16}  {:<8}  LAST USED",
        "ID", "BACKEND", "PROVIDER", "NAME", "AUTH"
    );
    for r in rows {
        let last = r
            .last_used_at
            .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| "never".into());
        println!(
            "{:<24}  {:<8}  {:<10}  {:<16}  {:<8}  {}",
            r.id,
            r.backend.as_str(),
            r.provider,
            r.name,
            r.auth_method.as_str(),
            last
        );
    }
    Ok(())
}

async fn show(cfg: &Config, id: &str) -> Result<(), ProfileCmdError> {
    let r = cfg
        .store
        .get_profile(id)
        .await?
        .ok_or_else(|| ProfileCmdError::ProfileNotFound(id.to_owned()))?;
    println!("{:<22} {}", "id:", r.id);
    println!("{:<22} {}", "backend:", r.backend.as_str());
    println!("{:<22} {}", "name:", r.name);
    println!("{:<22} {}", "provider:", r.provider);
    println!("{:<22} {}", "auth_method:", r.auth_method.as_str());
    println!("{:<22} {}", "created_at:", r.created_at);
    println!(
        "{:<22} {}",
        "last_used_at:",
        r.last_used_at
            .map(|t| t.to_rfc3339())
            .unwrap_or_else(|| "(never)".into())
    );
    let any = AnyProfileId::parse(&r.id)
        .map_err(|e| ProfileCmdError::BadId(format!("{e}")))?;
    let dir = cfg.anatta_home.join("profiles").join(any.as_str());
    println!("{:<22} {}", "path:", dir.display());

    // Overrides — only print non-None ones.
    let overrides: &[(&str, Option<&str>)] = &[
        ("base_url:",       r.base_url_override.as_deref()),
        ("model:",          r.model_override.as_deref()),
        ("small_fast_model:", r.small_fast_model_override.as_deref()),
        ("opus_model:",     r.default_opus_model_override.as_deref()),
        ("sonnet_model:",   r.default_sonnet_model_override.as_deref()),
        ("haiku_model:",    r.default_haiku_model_override.as_deref()),
        ("subagent_model:", r.subagent_model_override.as_deref()),
    ];
    let any_override = overrides.iter().any(|(_, v)| v.is_some());
    if any_override {
        println!("{:<22}", "overrides:");
        for (label, val) in overrides {
            if let Some(v) = val {
                println!("  {:<20} {}", label, v);
            }
        }
    }

    if matches!(r.auth_method, AuthMethod::ApiKey) {
        let has = auth::read_api_key(&cfg.anatta_home, &r.id)?.is_some();
        println!(
            "{:<22} {}",
            "api_key:",
            if has { "(stored)" } else { "(missing)" }
        );
    }
    Ok(())
}

async fn delete(cfg: &Config, id: &str, yes: bool) -> Result<(), ProfileCmdError> {
    let row = cfg
        .store
        .get_profile(id)
        .await?
        .ok_or_else(|| ProfileCmdError::ProfileNotFound(id.to_owned()))?;

    if !yes {
        let confirm = dialoguer::Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt(format!(
                "Delete profile {} ({} / \"{}\")? Profile dir, credentials file, and DB row are removed; shared session files are kept.",
                row.id,
                row.backend.as_str(),
                row.name
            ))
            .default(false)
            .interact()?;
        if !confirm {
            println!("aborted.");
            return Ok(());
        }
    }

    // Removing the profile dir wipes the credentials file alongside
    // claude/codex's own state — single recursive rm covers both.
    let dir = cfg.anatta_home.join("profiles").join(&row.id);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(ProfileCmdError::Io)?;
    }
    cfg.store.delete_profile(&row.id).await?;

    println!("✓ {} deleted.", row.id);
    Ok(())
}
