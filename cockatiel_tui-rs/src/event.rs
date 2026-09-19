use crossterm::event::{self, Event, KeyEvent, MouseEvent};
use tokio::sync::mpsc;

pub enum AppEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize(u16, u16),
    Paste(String),
}

/// Spawn a dedicated thread that blocks on `crossterm::event::read()` and
/// forwards every input event onto the channel the UI's async loop selects on.
/// Keypresses/mouse/resizes arrive immediately — no polling, no timer latency.
pub fn spawn_event_reader(event_tx: mpsc::UnboundedSender<AppEvent>) {
    std::thread::spawn(move || loop {
        match event::read() {
            Ok(Event::Key(key)) => {
                if event_tx.send(AppEvent::Key(key)).is_err() {
                    break;
                }
            }
            Ok(Event::Mouse(mouse)) => {
                if event_tx.send(AppEvent::Mouse(mouse)).is_err() {
                    break;
                }
            }
            Ok(Event::Resize(w, h)) => {
                if event_tx.send(AppEvent::Resize(w, h)).is_err() {
                    break;
                }
            }
            Ok(Event::Paste(text)) => {
                if event_tx.send(AppEvent::Paste(text)).is_err() {
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    });
}