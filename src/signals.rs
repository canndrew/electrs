use futures::StreamExt;
use tokio::{
    signal::unix::{signal, SignalKind},
    sync::mpsc::{unbounded_channel, UnboundedReceiver},
};

pub(crate) enum Signal {
    Exit,
    Trigger,
}

pub(crate) fn register() -> UnboundedReceiver<Signal> {
    let (tx, rx) = unbounded_channel();
    let mut sigterm = signal(SignalKind::terminate())
        .expect("failed to register signal hook for SIGTERM")
        .fuse();
    let mut sigint = signal(SignalKind::interrupt())
        .expect("failed to register signal hook for SIGINT")
        .fuse();
    let mut sigusr1 = signal(SignalKind::user_defined1())
        .expect("failed to register signal hook for SIGUSR1")
        .fuse();
    tokio::spawn(async move {
        loop {
            futures::select! {
                () = sigterm.select_next_some() => {
                    info!("notified via SIGTERM");
                    if tx.send(Signal::Exit).is_err() {
                        break;
                    }
                },
                () = sigint.select_next_some() => {
                    info!("notified via SIGINT");
                    if tx.send(Signal::Exit).is_err() {
                        break;
                    }
                },
                () = sigusr1.select_next_some() => {
                    info!("notified via SIGUSR1");
                    if tx.send(Signal::Trigger).is_err() {
                        break;
                    }
                },
                complete => break,
            }
        }
    });
    rx
}
