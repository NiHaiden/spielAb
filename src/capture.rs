//! Wayland screen sharing through the desktop portal and its PipeWire remote.
//! The portal session and owned FD stay alive together until capture stops.
use crate::settings::CaptureMode;
use anyhow::{Context, Result, ensure};
use ashpd::desktop::{
    PersistMode, Session,
    screencast::{CursorMode, Screencast, SourceType},
};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

pub struct ScreenCapture {
    session: PortalSession,
    remote: OwnedFd,
    pub node_id: u32,
    pub size: Option<(i32, i32)>,
    pub mode: CaptureMode,
}

// Cancellation can drop select() while the compositor dialog is open.
// Retain a guard from the moment CreateSession succeeds so this also closes it.
struct PortalSession(Option<Session<'static, Screencast<'static>>>);

impl PortalSession {
    async fn close(&mut self) -> Result<()> {
        if let Some(session) = self.0.take() {
            session.close().await?;
        }
        Ok(())
    }
}

impl Drop for PortalSession {
    fn drop(&mut self) {
        if let Some(session) = self.0.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move {
                let _ = session.close().await;
            });
        }
    }
}

impl ScreenCapture {
    pub async fn select() -> Result<Self> {
        Self::select_mode(CaptureMode::Mirror).await
    }

    pub async fn supports_virtual() -> Result<bool> {
        Ok(Screencast::new()
            .await?
            .available_source_types()
            .await?
            .contains(SourceType::Virtual))
    }

    pub async fn select_mode(mode: CaptureMode) -> Result<Self> {
        let portal = Screencast::new()
            .await
            .context("ScreenCast portal unavailable")?;
        let available = portal.available_source_types().await?;
        let requested = match mode {
            CaptureMode::Mirror => (SourceType::Monitor | SourceType::Window) & available,
            CaptureMode::Extend => {
                ensure!(
                    available.contains(SourceType::Virtual),
                    "This desktop does not support virtual monitors through its screen-sharing portal"
                );
                SourceType::Virtual.into()
            }
        };
        ensure!(
            !requested.is_empty(),
            "No supported screen sources are available"
        );
        let mut guard = PortalSession(Some(portal.create_session().await?));
        let session = guard.0.as_ref().expect("New portal session");
        let result = async {
            portal
                .select_sources(
                    session,
                    CursorMode::Embedded,
                    requested,
                    // KDE 6.7's single-source card accepts synchronously from
                    // PipeWireLayout.qml::onClicked. Starting the stream can
                    // destroy that delegate inside its own signal handler and
                    // abort the portal. Checkbox selection + the Share button
                    // avoids that reentrant acceptance path. Still require one
                    // stream below; never silently choose among multiple.
                    true,
                    None,
                    PersistMode::DoNot,
                )
                .await?
                .response()?;
            let selection = portal
                .start(session, None)
                .await
                .context("The desktop sharing dialog closed unexpectedly. Try again: single-click one source, then press Share")?
                .response()?;
            ensure!(
                selection.streams().len() == 1,
                "Select exactly one screen or window, then press Share"
            );
            let stream = selection
                .streams()
                .first()
                .context("No screen was selected")?;
            let node_id = stream.pipe_wire_node_id();
            ensure!(
                mode != CaptureMode::Extend || stream.source_type() == Some(SourceType::Virtual),
                "The desktop did not return a virtual monitor; extended display was not created"
            );
            let size = stream.size();
            let remote = portal.open_pipe_wire_remote(session).await?;
            Ok::<_, anyhow::Error>((remote, node_id, size))
        }
        .await;
        match result {
            Ok((remote, node_id, size)) => Ok(Self {
                session: guard,
                remote,
                node_id,
                size,
                mode,
            }),
            Err(error) => {
                let _ = guard.close().await;
                Err(error)
            }
        }
    }

    pub fn remote_fd(&self) -> RawFd {
        self.remote.as_raw_fd()
    }

    pub async fn close(mut self) -> Result<()> {
        self.session.close().await?;
        Ok(())
    }
}
