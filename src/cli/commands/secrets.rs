//! `powerqueue secrets ...` — manage stored API keys.

use anyhow::{Result, anyhow};
use comfy_table::Cell;
use owo_colors::{OwoColorize, Stream};

use crate::cli::output;
use crate::cli::{Context, SecretsCommand};
use crate::secrets::{SecretKind, SecretOrigin, Secrets, mask};

/// Map a user-typed name (`linear`, `linear_api_key`, `jev`, `jev_api_key`) to a kind.
pub fn parse_secret_name(name: &str) -> Result<SecretKind> {
    match name.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "linear" | "linear_api_key" | "linear_key" => Ok(SecretKind::LinearApiKey),
        "github" | "github_token" => Ok(SecretKind::GitHubToken),
        "jev" | "jev_api_key" | "jev_key" | "typesafe" => Ok(SecretKind::JevApiKey),
        other => Err(anyhow!("unknown secret `{other}` (expected linear | github | jev)")),
    }
}

/// One row of `secrets list`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SecretRow {
    pub name: &'static str,
    pub label: &'static str,
    pub configured: bool,
    pub origin: Option<SecretOrigin>,
    pub masked: Option<String>,
    pub env_var: &'static str,
}

/// Collect the state of every known secret (values are masked).
pub fn secret_rows(secrets: &Secrets) -> Result<Vec<SecretRow>> {
    SecretKind::ALL
        .iter()
        .map(|kind| {
            let found = secrets.get_with_origin(*kind)?;
            Ok(SecretRow {
                name: kind.name(),
                label: kind.label(),
                configured: found.is_some(),
                origin: found.as_ref().map(|(_, o)| *o),
                masked: found.as_ref().map(|(v, _)| mask(v)),
                env_var: kind.env_var(),
            })
        })
        .collect()
}

/// Handle `powerqueue secrets ...`.
pub fn run(ctx: &mut Context, cmd: SecretsCommand) -> Result<i32> {
    match cmd {
        SecretsCommand::Set { name, value } => {
            let kind = parse_secret_name(&name)?;
            let value = match value {
                Some(v) => v,
                None => dialoguer::Password::new().with_prompt(kind.label().to_string()).interact()?,
            };
            let secrets = ctx.secrets();
            secrets.set(kind, &value)?;
            println!(
                "{} {} stored in {}",
                "ok".if_supports_color(Stream::Stdout, |t| t.green()),
                kind.label(),
                secrets.backend_description()
            );
            if secrets.get_with_origin(kind)?.map(|(_, o)| o) == Some(SecretOrigin::Environment) {
                println!(
                    "{}",
                    format!("note: {} is set in the environment and takes precedence", kind.env_var())
                        .if_supports_color(Stream::Stdout, |t| t.yellow())
                );
            }
            Ok(0)
        }
        SecretsCommand::Unset { name } => {
            let kind = parse_secret_name(&name)?;
            let secrets = ctx.secrets();
            secrets.delete(kind)?;
            println!(
                "{} {} removed from {}",
                "ok".if_supports_color(Stream::Stdout, |t| t.green()),
                kind.label(),
                secrets.backend_description()
            );
            Ok(0)
        }
        SecretsCommand::List => {
            let color = ctx.color;
            let json = ctx.json;
            let secrets = ctx.secrets();
            let rows = secret_rows(secrets)?;
            let backend = secrets.backend_description();
            if json {
                println!("{}", serde_json::json!({ "backend": backend, "secrets": rows }));
                return Ok(0);
            }
            let mut table = output::table();
            if !color {
                table.force_no_tty();
            }
            table.set_header(vec!["NAME", "CONFIGURED", "ORIGIN", "VALUE"]);
            for r in &rows {
                let configured = if r.configured { "yes" } else { "no" };
                table.add_row(vec![
                    Cell::new(r.name),
                    Cell::new(configured),
                    Cell::new(r.origin.map(|o| o.to_string()).unwrap_or_else(|| "-".into())),
                    Cell::new(
                        r.masked
                            .clone()
                            .unwrap_or_else(|| format!("(set with `powerqueue secrets set {}` or {})", r.name, r.env_var)),
                    ),
                ]);
            }
            println!("{table}");
            println!("backend: {backend}");
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::FileBackend;

    #[test]
    fn names_are_lenient() {
        assert_eq!(parse_secret_name("linear").unwrap(), SecretKind::LinearApiKey);
        assert_eq!(parse_secret_name("LINEAR_API_KEY").unwrap(), SecretKind::LinearApiKey);
        assert_eq!(parse_secret_name("jev-api-key").unwrap(), SecretKind::JevApiKey);
        assert!(parse_secret_name("openai").is_err());
    }

    #[test]
    fn rows_mask_values() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = Secrets::with_backend(Box::new(FileBackend::new(dir.path().join("s.toml"))));
        secrets.set(SecretKind::LinearApiKey, "lin_api_1234567890").unwrap();
        let rows = secret_rows(&secrets).unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows[0].configured);
        assert_eq!(rows[0].masked.as_deref(), Some("lin_************90"));
        assert_eq!(rows[0].origin, Some(SecretOrigin::File));
    }
}
