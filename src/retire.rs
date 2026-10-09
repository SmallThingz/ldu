//! Transfer owned values to a single background thread for destruction.

use crossbeam_channel::{Sender, TrySendError, bounded};
use std::{io, thread};

pub(crate) struct Retirer<T: Send + 'static> {
    sender: Sender<T>,
}

impl<T: Send + 'static> Retirer<T> {
    pub(crate) fn new() -> io::Result<Self> {
        let (sender, receiver) = bounded::<T>(1);
        let _ = thread::Builder::new()
            .name("ldu-retire".into())
            .spawn(move || {
                for item in receiver {
                    drop(item);
                }
            })?;
        Ok(Self { sender })
    }

    /// Returns ownership immediately if cleanup is busy or its worker has stopped.
    pub(crate) fn try_retire(&self, item: T) -> Result<(), T> {
        match self.sender.try_send(item) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(item) | TrySendError::Disconnected(item)) => Err(item),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::Receiver;
    use std::time::Duration;

    struct Tracked {
        tag: u8,
        dropped: Sender<(u8, thread::ThreadId)>,
        gate: Option<Receiver<()>>,
    }

    impl Drop for Tracked {
        fn drop(&mut self) {
            let _ = self.dropped.send((self.tag, thread::current().id()));
            if let Some(gate) = self.gate.take() {
                let _ = gate.recv();
            }
        }
    }

    #[test]
    fn destruction_runs_off_thread_and_full_queue_returns_ownership() {
        let retirer = Retirer::new().unwrap();
        let (dropped, notices) = bounded(3);
        let (release, gate) = bounded(0);
        let caller = thread::current().id();
        assert!(
            retirer
                .try_retire(Tracked {
                    tag: 1,
                    dropped: dropped.clone(),
                    gate: Some(gate),
                })
                .is_ok()
        );
        let first = notices.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(first.0, 1);
        assert_ne!(first.1, caller);

        // The worker is held inside the first destructor, so one queued value fills capacity.
        assert!(
            retirer
                .try_retire(Tracked {
                    tag: 2,
                    dropped: dropped.clone(),
                    gate: None,
                })
                .is_ok()
        );
        let (response, result) = bounded(1);
        let _ = thread::spawn(move || {
            let outcome = retirer.try_retire(Tracked {
                tag: 3,
                dropped,
                gate: None,
            });
            let _ = response.send((retirer, outcome));
        });
        let (retirer, outcome) = result
            .recv_timeout(Duration::from_secs(5))
            .expect("retirement blocked behind an active destructor");
        let returned = match outcome {
            Err(item) => item,
            Ok(()) => panic!("a full cleanup queue accepted another value"),
        };
        assert_eq!(returned.tag, 3);
        assert!(
            notices.try_recv().is_err(),
            "returned ownership was dropped"
        );

        release.send(()).unwrap();
        let second = notices.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(second.0, 2);
        assert_eq!(second.1, first.1);
        assert!(retirer.try_retire(returned).is_ok());
        drop(retirer);
        let third = notices.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(third.0, 3);
        assert_eq!(third.1, first.1);
    }
}
