use ai::auth::{
    AuthEvent, AuthFuture, AuthInteraction, AuthPrompt, AuthSelectOption, OAuthCredential,
};
use ai::providers::all::builtin_providers;
use ai::utils::abort::AbortController;
use indexmap::IndexMap;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;

const AUTH_FILE: &str = "auth.json";

#[derive(Clone)]
struct CliInteraction {
    signal: Arc<dyn ai::types::AbortSignal>,
}

impl CliInteraction {
    fn read_answer(question: &str) -> Result<String, ai::auth::AuthError> {
        print!("{question}");
        io::stdout()
            .flush()
            .map_err(|error| ai::auth::AuthError::new(error.to_string()))?;
        let mut answer = String::new();
        io::stdin()
            .read_line(&mut answer)
            .map_err(|error| ai::auth::AuthError::new(error.to_string()))?;
        while answer.ends_with(['\n', '\r']) {
            answer.pop();
        }
        Ok(answer)
    }

    fn select_answer(
        message: &str,
        options: &[AuthSelectOption],
    ) -> Result<String, ai::auth::AuthError> {
        println!("\n{message}");
        for (index, option) in options.iter().enumerate() {
            println!("  {}. {}", index + 1, option.label);
        }
        let answer = Self::read_answer(&format!("Enter number (1-{}): ", options.len()))?;
        let index = parse_selection(&answer);
        index
            .and_then(|index| options.get(index))
            .map(|option| option.id.clone())
            .ok_or_else(|| ai::auth::AuthError::new("Invalid selection"))
    }
}

fn parse_selection(answer: &str) -> Option<usize> {
    let answer = answer
        .trim_start_matches(|character: char| character.is_whitespace() || character == '\u{feff}');
    let (negative, digits) = match answer.as_bytes().first() {
        Some(b'+') => (false, &answer[1..]),
        Some(b'-') => (true, &answer[1..]),
        _ => (false, answer),
    };
    let digit_count = digits.bytes().take_while(u8::is_ascii_digit).count();
    if negative || digit_count == 0 {
        return None;
    }
    digits[..digit_count]
        .parse::<usize>()
        .ok()
        .and_then(|value| value.checked_sub(1))
}

impl AuthInteraction for CliInteraction {
    fn signal(&self) -> Option<Arc<dyn ai::types::AbortSignal>> {
        Some(self.signal.clone())
    }

    fn prompt(&self, prompt: AuthPrompt) -> AuthFuture<String> {
        Box::pin(async move {
            match prompt {
                AuthPrompt::Select {
                    message, options, ..
                } => Self::select_answer(&message, &options),
                AuthPrompt::Text {
                    message,
                    placeholder,
                    ..
                }
                | AuthPrompt::Secret {
                    message,
                    placeholder,
                    ..
                }
                | AuthPrompt::ManualCode {
                    message,
                    placeholder,
                    ..
                } => {
                    let placeholder = placeholder
                        .map(|placeholder| format!(" ({placeholder})"))
                        .unwrap_or_default();
                    Self::read_answer(&format!("{message}{placeholder}: "))
                }
            }
        })
    }

    fn notify(&self, event: AuthEvent) {
        match event {
            AuthEvent::AuthUrl { url, instructions } => {
                println!("\nOpen this URL in your browser:\n{url}");
                if let Some(instructions) = instructions {
                    println!("{instructions}");
                }
            }
            AuthEvent::DeviceCode {
                user_code,
                verification_uri,
                ..
            } => {
                println!("\nOpen this URL in your browser:\n{verification_uri}");
                println!("Enter code: {user_code}");
            }
            AuthEvent::Info { message, .. } | AuthEvent::Progress { message } => {
                println!("{message}");
            }
        }
    }
}

fn load_auth_from(path: &Path) -> IndexMap<String, OAuthCredential> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}

fn save_auth_to(path: &Path, auth: &IndexMap<String, OAuthCredential>) -> Result<(), String> {
    let contents = serde_json::to_string_pretty(auth).map_err(|error| error.to_string())?;
    std::fs::write(path, contents).map_err(|error| error.to_string())
}

fn oauth_providers() -> Vec<ai::models::ProviderRef> {
    builtin_providers()
        .into_iter()
        .filter(|provider| provider.auth().oauth.is_some())
        .collect()
}

async fn login(provider_id: &str) -> Result<(), String> {
    let providers = oauth_providers();
    let provider = providers
        .iter()
        .find(|provider| provider.id() == provider_id)
        .ok_or_else(|| format!("Unknown provider: {provider_id}"))?;
    let oauth = provider
        .auth()
        .oauth
        .ok_or_else(|| format!("Unknown provider: {provider_id}"))?;
    let controller = AbortController::new();
    let interaction: Arc<dyn AuthInteraction> = Arc::new(CliInteraction {
        signal: controller.signal(),
    });
    let credential = (oauth.login)(ai::auth::normalize_interaction(interaction))
        .await
        .map_err(|error| error.message)?;
    let mut auth = load_auth_from(Path::new(AUTH_FILE));
    auth.insert(provider_id.to_owned(), credential);
    save_auth_to(Path::new(AUTH_FILE), &auth)?;
    println!("\nCredentials saved to {AUTH_FILE}");
    Ok(())
}

fn print_help(providers: &[ai::models::ProviderRef]) {
    let provider_list = providers
        .iter()
        .map(|provider| format!("  {:<20} {}", provider.id(), provider.name()))
        .collect::<Vec<_>>()
        .join("\n");
    println!(
        "Usage: npx @earendil-works/pi-ai <command> [provider]\n\nCommands:\n  login [provider]  Login to an OAuth provider\n  list              List available providers\n\nProviders:\n{provider_list}"
    );
}

async fn run(args: &[String]) -> Result<(), String> {
    let providers = oauth_providers();
    let command = args.first().map(String::as_str);
    if command.is_none() || matches!(command, Some("help" | "--help" | "-h")) {
        print_help(&providers);
        return Ok(());
    }
    if command == Some("list") {
        for provider in providers {
            println!("{:<20} {}", provider.id(), provider.name());
        }
        return Ok(());
    }
    if command == Some("login") {
        let provider_id = if let Some(provider_id) = args.get(1) {
            provider_id.clone()
        } else {
            for (index, provider) in providers.iter().enumerate() {
                println!("  {}. {}", index + 1, provider.name());
            }
            let answer =
                CliInteraction::read_answer(&format!("Enter number (1-{}): ", providers.len()))
                    .map_err(|error| error.message)?;
            parse_selection(&answer)
                .and_then(|index| providers.get(index))
                .map(|provider| provider.id().to_owned())
                .unwrap_or_default()
        };
        if !providers
            .iter()
            .any(|provider| provider.id() == provider_id)
        {
            return Err(format!("Unknown provider: {provider_id}"));
        }
        return login(&provider_id).await;
    }
    Err(format!(
        "Unknown command: {}",
        command.expect("known to be present")
    ))
}

#[tokio::main]
async fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if let Err(error) = run(&args).await {
        eprintln!("Error: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai::auth::OAuthCredentialType;
    use serde_json::Map;

    /// Pins pi `src/cli.ts:18-29`'s absent/malformed fallback and pretty JSON persistence.
    #[test]
    fn auth_file_round_trip_and_malformed_fallback() {
        let directory =
            std::env::temp_dir().join(format!("agentprism-ai-cli-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("test directory");
        let path = directory.join("auth.json");
        assert!(load_auth_from(&path).is_empty());
        std::fs::write(&path, "not json").expect("malformed fixture");
        assert!(load_auth_from(&path).is_empty());
        let credential = OAuthCredential {
            kind: OAuthCredentialType::OAuth,
            refresh: "refresh".to_owned(),
            access: "access".to_owned(),
            expires: 123.0,
            extra: Map::new(),
        };
        let auth = IndexMap::from([("provider".to_owned(), credential.clone())]);
        save_auth_to(&path, &auth).expect("save");
        assert_eq!(load_auth_from(&path)["provider"], credential);
        std::fs::remove_file(path).expect("remove auth fixture");
        std::fs::remove_dir(directory).expect("remove test directory");
    }

    /// Pins pi `src/cli.ts:37,101`'s `Number.parseInt(..., 10)` prefix parsing.
    #[test]
    fn selection_parsing_matches_javascript_parse_int() {
        assert_eq!(parse_selection("  +2trailing"), Some(1));
        assert_eq!(parse_selection("\u{feff}1"), Some(0));
        assert_eq!(parse_selection("-1"), None);
        assert_eq!(parse_selection("invalid"), None);
    }

    /// Ports pi `src/cli.ts:8-12,79-92` without invoking an OAuth network flow.
    #[tokio::test]
    async fn help_list_and_unknown_command_paths_are_complete() {
        let providers = oauth_providers();
        assert!(!providers.is_empty());
        assert!(
            providers
                .iter()
                .all(|provider| provider.auth().oauth.is_some())
        );
        assert!(run(&["help".to_owned()]).await.is_ok());
        assert!(run(&["list".to_owned()]).await.is_ok());
        assert_eq!(
            run(&["unknown".to_owned()]).await,
            Err("Unknown command: unknown".to_owned())
        );
    }
}
