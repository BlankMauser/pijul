use anyhow::bail;
use clap::Parser;
use std::path::Path;

/// Mint an HTTP bearer token for a Nest by proving possession of an SSH key.
///
/// Prints the token on stdout so it can be wired into a remote's Authorization
/// header, e.g. `Authorization: Bearer $(pijul http-token <remote>)`.
#[derive(Parser, Debug)]
pub struct HttpToken {
    /// The HTTP remote (URL or configured name). Defaults to the default remote.
    remote: Option<String>,
    /// Identity to sign with (defaults to the configured identity)
    #[clap(long = "identity")]
    identity: Option<String>,
}

impl HttpToken {
    pub fn repository_path(&mut self) -> Option<&Path> {
        None
    }

    pub async fn run(mut self, config: &pijul_config::Config) -> Result<(), anyhow::Error> {
        let identity_name = self
            .identity
            .take()
            .unwrap_or(pijul_identity::choose_identity_name(config).await?);
        let complete = pijul_identity::Complete::load(&identity_name)?;
        let secret = complete.skey();

        let remote_name = if let Some(ref r) = self.remote {
            r.clone()
        } else if let Some(ref def) = config.default_remote {
            def.clone()
        } else {
            bail!("Missing remote: pass a remote URL/name or set a default remote");
        };

        let mut remote = pijul_remote::repository(
            config,
            None,
            Some(&complete.config.author.username),
            &remote_name,
            pijul_core::DEFAULT_CHANNEL,
            false,
            true,
        )
        .await?;

        let (token, _expires_in) = remote.http_login(&secret).await?;
        println!("{}", token);
        Ok(())
    }
}
