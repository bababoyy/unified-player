//! In-process `QuickJS` solver for `YouTube` player challenges.
//!
//! The synchronous engine lives on one native thread, so SWC and `QuickJS`
//! work never blocks Tokio while `QuickJS`'s interrupt handler can still
//! observe cancellation.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
};

use sha2::{Digest as _, Sha256};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::{timeout, Duration};
use tokio_util::sync::CancellationToken;

mod engine;

pub(crate) use engine::Solutions;
use engine::{challenge_bytes, prepare_player, PreparedSolver};

use super::{MAX_CHALLENGE_BYTES, MAX_PLAYER_SCRIPT_BYTES};

const MAX_PREPARED_PLAYERS: usize = 2;
const INTERRUPT_GRACE: Duration = Duration::from_millis(500);

/// Failures are deliberately coarse: they never carry player code,
/// challenge values, or engine messages.
#[derive(Debug, Error)]
pub(crate) enum Error {
    #[error("YouTube JavaScript solving was cancelled")]
    Cancelled,
    #[error("YouTube JavaScript solver timed out")]
    Timeout,
    #[error("YouTube JavaScript solver is unavailable")]
    Unavailable,
    #[error("YouTube JavaScript solver returned an invalid response")]
    InvalidResponse,
    #[error("YouTube JavaScript input exceeded the local size limit")]
    InputTooLarge,
    #[error("YouTube JavaScript output exceeded the local size limit")]
    OutputTooLarge,
}

enum Command {
    Solve {
        player: Option<String>,
        signature_challenges: Vec<String>,
        n_challenges: Vec<String>,
        reply: oneshot::Sender<Result<Solutions, Error>>,
    },
}

struct PreparedEntry {
    key: [u8; 32],
    solver: PreparedSolver,
}

/// A serialized `QuickJS` runtime with a bounded prepared-player LRU.
///
/// Execution is interruptible through a shared flag, but a native thread
/// cannot be killed: if the engine ever ignored its interrupt, the runtime
/// marks itself unavailable instead of waiting forever.
pub(crate) struct InProcessRuntime {
    sender: mpsc::Sender<Command>,
    interrupt: Arc<AtomicBool>,
    poisoned: AtomicBool,
    gate: Mutex<()>,
}

impl InProcessRuntime {
    /// Start the dedicated native `QuickJS` worker thread.
    ///
    /// # Panics
    ///
    /// Panics if the worker thread cannot be started.
    pub(crate) fn new() -> Self {
        let (sender, receiver) = mpsc::channel(1);
        let interrupt = Arc::new(AtomicBool::new(false));
        let worker_interrupt = interrupt.clone();
        thread::Builder::new()
            .name("youtube-quickjs".to_owned())
            .spawn(move || run_worker(receiver, &worker_interrupt))
            .expect("start in-process YouTube QuickJS worker");
        Self {
            sender,
            interrupt,
            poisoned: AtomicBool::new(false),
            gate: Mutex::new(()),
        }
    }

    /// Solve one bounded batch of challenges. `player` may be `None` to reuse
    /// the most recently prepared player.
    pub(crate) async fn solve(
        &self,
        player: Option<&str>,
        signature_challenges: &[String],
        n_challenges: &[String],
        cancellation: &CancellationToken,
    ) -> Result<Solutions, Error> {
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if player.is_some_and(|value| value.len() > MAX_PLAYER_SCRIPT_BYTES)
            || challenge_bytes(signature_challenges, n_challenges) > MAX_CHALLENGE_BYTES
        {
            return Err(Error::InputTooLarge);
        }
        let _gate = self.gate.lock().await;
        if self.poisoned.load(Ordering::Acquire) {
            return Err(Error::Unavailable);
        }
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }

        self.interrupt.store(false, Ordering::Relaxed);
        let (reply, response) = oneshot::channel();
        let command = Command::Solve {
            player: player.map(str::to_owned),
            signature_challenges: signature_challenges.to_vec(),
            n_challenges: n_challenges.to_vec(),
            reply,
        };
        tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                self.interrupt.store(true, Ordering::Relaxed);
                Err(Error::Cancelled)
            }
            sent = self.sender.send(command) => {
                sent.map_err(|_| Error::Unavailable)?;
                tokio::pin!(response);
                tokio::select! {
                    biased;
                    () = cancellation.cancelled() => {
                        self.interrupt.store(true, Ordering::Relaxed);
                        match timeout(INTERRUPT_GRACE, &mut response).await {
                            Ok(Ok(_)) => Err(Error::Cancelled),
                            Ok(Err(_)) => Err(Error::Unavailable),
                            Err(_) => {
                                self.poisoned.store(true, Ordering::Release);
                                Err(Error::Timeout)
                            }
                        }
                    }
                    result = &mut response => result.map_err(|_| Error::Unavailable)?
                }
            }
        }
    }
}

impl Drop for InProcessRuntime {
    fn drop(&mut self) {
        self.interrupt.store(true, Ordering::Relaxed);
    }
}

fn run_worker(mut receiver: mpsc::Receiver<Command>, interrupt: &Arc<AtomicBool>) {
    let mut prepared = VecDeque::new();
    while let Some(command) = receiver.blocking_recv() {
        match command {
            Command::Solve {
                player,
                signature_challenges,
                n_challenges,
                reply,
            } => {
                let result = run_with_panic_boundary(&mut prepared, |prepared| {
                    solve_request(
                        prepared,
                        interrupt,
                        player.as_deref(),
                        &signature_challenges,
                        &n_challenges,
                    )
                });
                let _ = reply.send(result);
            }
        }
    }
}

fn run_with_panic_boundary<F>(
    prepared: &mut VecDeque<PreparedEntry>,
    solve: F,
) -> Result<Solutions, Error>
where
    F: FnOnce(&mut VecDeque<PreparedEntry>) -> Result<Solutions, Error>,
{
    catch_unwind(AssertUnwindSafe(|| solve(prepared))).unwrap_or_else(|_| {
        prepared.clear();
        Err(Error::Unavailable)
    })
}

fn solve_request(
    prepared: &mut VecDeque<PreparedEntry>,
    interrupt: &Arc<AtomicBool>,
    player: Option<&str>,
    signature_challenges: &[String],
    n_challenges: &[String],
) -> Result<Solutions, Error> {
    if interrupt.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }

    if let Some(player) = player {
        let key: [u8; 32] = Sha256::digest(player.as_bytes()).into();
        if let Some(index) = prepared.iter().position(|entry| entry.key == key) {
            if let Some(entry) = prepared.remove(index) {
                prepared.push_back(entry);
            }
        } else {
            let prepared_player =
                prepare_player(player).map_err(|error| map_engine_error(&error))?;
            let solver = PreparedSolver::quickjs_with_interrupt(prepared_player, interrupt.clone())
                .map_err(|error| map_engine_error(&error))?;
            prepared.push_back(PreparedEntry { key, solver });
            while prepared.len() > MAX_PREPARED_PLAYERS {
                prepared.pop_front();
            }
        }
    }

    let Some(entry) = prepared.back_mut() else {
        return Err(Error::Unavailable);
    };
    if interrupt.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    entry.solver.clear_interrupt();
    let result = entry
        .solver
        .solve(signature_challenges, n_challenges)
        .map_err(|error| map_engine_error(&error));
    if interrupt.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    result
}

fn map_engine_error(error: &engine::Error) -> Error {
    match error {
        engine::Error::PlayerTooLarge | engine::Error::ChallengesTooLarge => Error::InputTooLarge,
        engine::Error::OutputTooLarge => Error::OutputTooLarge,
        engine::Error::Backend(_) => Error::InvalidResponse,
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, time::Duration};

    use tokio_util::sync::CancellationToken;

    use super::{engine, run_with_panic_boundary, InProcessRuntime, PreparedEntry};

    const PLAYER: &str = include_str!("../ejs/fixtures/synthetic_reverse.json");

    #[derive(serde::Deserialize)]
    struct Fixture {
        player: String,
    }

    const SLOW_PLAYER: &str = r#"(function(){
function R(){this.v=new Map();}
R.prototype.set=function(k,v){this.v.set(k,v);};
R.prototype.get=function(k){return this.v.get(k);};
R.prototype.clone=function(){return this;};
R.prototype.transform=function(){while(true){}}
var M={mark:function(a,b){return b;}};
var H=function(a,b,c){M.mark("alr","yes");var r=new R();if(c!==undefined)r.set(b,c);return r;};
}).call(this);"#;

    fn fixture_player() -> String {
        serde_json::from_str::<Fixture>(PLAYER)
            .expect("valid fixture")
            .player
    }

    #[tokio::test]
    async fn solves_without_a_child_process() {
        let runtime = InProcessRuntime::new();
        let solutions = runtime
            .solve(
                Some(&fixture_player()),
                &["uvwxyz".to_owned()],
                &["abcdef".to_owned()],
                &CancellationToken::new(),
            )
            .await
            .expect("in-process QuickJS should solve");
        assert_eq!(solutions.signatures["uvwxyz"], "zyxwvu");
        assert_eq!(solutions.n_values["abcdef"], "fedcba");
    }

    #[tokio::test]
    async fn runtime_remains_usable_after_cancellation() {
        let runtime = InProcessRuntime::new();
        let cancellation = CancellationToken::new();
        let cancel = cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel.cancel();
        });
        let error = runtime
            .solve(
                Some(SLOW_PLAYER),
                &["slow-signature".to_owned()],
                &[],
                &cancellation,
            )
            .await
            .expect_err("cancelled solve");
        assert!(matches!(error, super::Error::Cancelled));

        let solutions = runtime
            .solve(
                Some(&fixture_player()),
                &["uvwxyz".to_owned()],
                &["abcdef".to_owned()],
                &CancellationToken::new(),
            )
            .await
            .expect("runtime should recover after cooperative cancellation");
        assert_eq!(solutions.signatures["uvwxyz"], "zyxwvu");
        assert_eq!(solutions.n_values["abcdef"], "fedcba");
    }

    #[tokio::test]
    async fn oversized_input_is_rejected_before_reaching_the_worker() {
        let runtime = InProcessRuntime::new();
        let player = "x".repeat(super::MAX_PLAYER_SCRIPT_BYTES + 1);
        let error = runtime
            .solve(Some(&player), &[], &[], &CancellationToken::new())
            .await
            .expect_err("oversized player");
        assert!(matches!(error, super::Error::InputTooLarge));
    }

    #[test]
    fn panic_boundary_clears_prepared_cache_and_fails_closed() {
        let prepared_player =
            engine::prepare_player(&fixture_player()).expect("fixture should preprocess");
        let solver = engine::PreparedSolver::quickjs_with_interrupt(
            prepared_player,
            std::sync::Arc::default(),
        )
        .expect("QuickJS should initialize");
        let mut prepared = VecDeque::from([PreparedEntry {
            key: [7; 32],
            solver,
        }]);

        let result = run_with_panic_boundary(&mut prepared, |_| panic!("synthetic worker panic"));

        assert!(matches!(result, Err(super::Error::Unavailable)));
        assert!(
            prepared.is_empty(),
            "panic must clear prepared QuickJS state"
        );
    }
}
