use super::{addresses, create_socket, IPVersion, SocketMode};
use crate::{protocol::Message, tui::Tui};
use crossterm::event::{Event, KeyCode};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{io, net::SocketAddr, path::Path};
use tokio::task::JoinSet;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;

pub async fn send(ip_version: IPVersion, _path: &Path) {
    let cancel = CancellationToken::new();

    let mut tasks = JoinSet::new();
    tasks.spawn(tui_loop(cancel.clone()));
    tasks.spawn(network_loop(ip_version, cancel.clone()));

    while let Some(r) = tasks.join_next().await {
        if !cancel.is_cancelled() {
            // any task returning means we're shutting down, so let's stop the others
            cancel.cancel();
        }
        match r {
            Ok(Ok(())) => (),
            Ok(Err(err)) => {
                tracing::error!(?err, "task terminated unsuccesfully");
            }
            Err(join_err) => {
                if let Ok(reason) = join_err.try_into_panic() {
                    std::panic::resume_unwind(reason);
                } else {
                    // task cancelled
                }
            }
        }
    }
}

async fn network_loop(ip_version: IPVersion, cancel: CancellationToken) -> eyre::Result<()> {
    let addr = addresses(ip_version);
    let socket = create_socket(addr.send, SocketMode::Send)?;

    let discover_msg = bitcode::encode(&Message::Discover);
    socket.send_to(&discover_msg, addr.recv).await?;

    let mut buf = Vec::with_capacity(65536);
    loop {
        tokio::select! {
            r = socket.recv_buf_from(&mut buf) => {
                let (_, addr) = r?;
                let msg: Message = bitcode::decode(&buf)?;
                handle_message(addr, msg);
                buf.clear();
            }
            _ = cancel.cancelled() => {
                tracing::debug!("network loop cancelled");
                break;
            }
        }
    }

    Ok(())
}

fn handle_message(src: SocketAddr, msg: Message) {
    match msg {
        Message::Discover => {
            tracing::error!("client received discover message");
        }
        Message::Announce(name) => {
            tracing::info!(addr = %src, "discovered server: {name}");
        }
        Message::Start {
            nonce,
            size,
            hash,
            path,
        } => todo!(),
        Message::Ack { nonce, id } => todo!(),
        Message::Nack { nonce, msg } => todo!(),
        Message::Data { id, chunk, content } => todo!(),
        Message::Error { id, msg } => todo!(),
        Message::Repeat { id, chunks } => todo!(),
        Message::Done { id } => todo!(),
    }
}

async fn tui_loop(cancel: CancellationToken) -> eyre::Result<()> {
    let backend = CrosstermBackend::new(io::stdout());
    let terminal = Terminal::new(backend)?;
    let mut tui = Tui::new(terminal);
    tui.init()?;

    let mut events = tui.events();
    loop {
        tui.draw()?;
        tokio::select! {
            event = events.next() => {
                match event {
                    Some(event) => match handle_event(event) {
                        Action::Exit => break,
                        Action::None => (),
                    }
                    None => break,
                }
            }
            _ = cancel.cancelled() => {
                tracing::debug!("tui loop cancelled");
                break;
            }
        }
    }

    tui.exit().await?;
    tracing::trace!("tui terminated successfully");

    Ok(())
}

enum Action {
    None,
    Exit,
}

fn handle_event(event: Event) -> Action {
    match event {
        Event::FocusGained => Action::None,
        Event::FocusLost => Action::None,
        Event::Key(event) => {
            if matches!(event.code, KeyCode::Char('q')) {
                Action::Exit
            } else {
                Action::None
            }
        }
        Event::Mouse(_) => Action::None,
        Event::Paste(_) => Action::None,
        Event::Resize(_, _) => Action::None,
    }
}
