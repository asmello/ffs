use crossterm::event::Event;
use ratatui::{
    backend::Backend,
    crossterm::{
        self,
        event::{DisableMouseCapture, EnableMouseCapture, EventStream},
        terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
    },
    layout::Alignment,
    style::{Color, Style},
    widgets::{Block, BorderType, Paragraph},
    Terminal,
};
use std::io;
use tokio::sync::broadcast;
use tokio_stream::{
    wrappers::{errors::BroadcastStreamRecvError, BroadcastStream},
    Stream, StreamExt,
};
use tokio_util::sync::CancellationToken;

pub struct Tui<B: Backend> {
    terminal: Terminal<B>,
    cancel: CancellationToken,
    events_task: Option<tokio::task::JoinHandle<()>>,
}

impl<B: Backend> Tui<B> {
    pub fn new(terminal: Terminal<B>) -> Self {
        Self {
            terminal,
            cancel: CancellationToken::new(),
            events_task: None,
        }
    }

    pub fn init(&mut self) -> eyre::Result<()> {
        terminal::enable_raw_mode()?;
        crossterm::execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
        let cancel = self.cancel.clone();

        let panic_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic| {
            cancel.cancel();
            let _ = Self::reset();
            panic_hook(panic);
        }));

        self.terminal.hide_cursor()?;
        self.terminal.clear()?;

        Ok(())
    }

    pub fn draw(&mut self) -> eyre::Result<()> {
        self.terminal.draw(|frame| {
            frame.render_widget(
                Paragraph::new(
                        "This is a tui template.\n\
                            Press `Esc`, `Ctrl-C` or `q` to stop running.\n\
                            Press left and right to increment and decrement the counter respectively.",
                )
                .block(
                    Block::bordered()
                        .title("Template")
                        .title_alignment(Alignment::Center)
                        .border_type(BorderType::Rounded),
                )
                .style(Style::default().fg(Color::Cyan).bg(Color::Black))
                .centered(),
                frame.area(),
            )
        })?;
        Ok(())
    }

    pub fn events(&mut self) -> impl Stream<Item = Event> {
        // this is actually a rare case where we want to drop events if they
        // accumulate too fast and the receiver cannot keep up, hence the use of
        // a broadcast channel
        let (tx, rx) = broadcast::channel(32);
        let cancel = self.cancel.clone();

        self.events_task = tokio::spawn(async move {
            let mut events = EventStream::new();
            loop {
                tokio::select! {
                    r = events.next() => {
                        let Some(res) = r else {
                            break;
                        };
                        match res {
                            Ok(event) => {
                                if tx.send(event).is_err() {
                                    // channel closed, we must be shutting down
                                    break;
                                }
                            }
                            Err(err) => {
                                tracing::error!(?err, "event loop error");
                                break;
                            }
                        }
                    }
                    _ = cancel.cancelled() => {
                        break;
                    }
                }
            }
        })
        .into();

        BroadcastStream::new(rx).map_while(|res| match res {
            Ok(event) => Some(event),
            Err(BroadcastStreamRecvError::Lagged(lag)) => {
                tracing::warn!("dropped {lag} events!");
                None
            }
        })
    }

    fn reset() -> eyre::Result<()> {
        terminal::disable_raw_mode()?;
        crossterm::execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture)?;
        Ok(())
    }

    pub async fn exit(&mut self) -> eyre::Result<()> {
        Self::reset()?;
        self.terminal.show_cursor()?;
        if let Some(events_task) = self.events_task.take() {
            self.cancel.cancel();
            tracing::trace!("waiting for events task to finish");
            events_task.await?;
            tracing::trace!("events task finished successfully");
        }
        Ok(())
    }
}
