//! The only provider-page warm-up path for the thread-affine Obscura worker.
//!
//! Keep the existing cookie/import/navigation/localStorage ordering here so
//! startup, refresh and login-complete do not drift apart.
use crate::{providers::Provider, session};
use anyhow::Result;

pub(super) struct PageSession;

impl PageSession {
    /// Restore a provider snapshot using the single canonical warm-up order:
    /// cookies → navigation → localStorage → application reload.
    /// With no snapshot, this only navigates to the provider's login/chat entrypoint.
    pub(super) async fn open(
        browser: &obscura_core::Browser,
        page: &mut obscura_core::Page,
        provider: Provider,
        session_file: Option<&session::SessionFile>,
    ) -> Result<()> {
        let cookie_restore = session_file
            .map(|file| session::restore_cookies(browser, file, provider.name()))
            .transpose();
        // Navigate even when cookie import failed so the caller's fail-closed
        // cleanup can clear origin-scoped localStorage on the provider origin.
        page.goto(provider.login_url()).await?;
        cookie_restore?;
        if let Some(file) = session_file {
            session::restore_local_storage(page, file)?;
            page.goto(provider.login_url()).await?;
        }
        Ok(())
    }
}
