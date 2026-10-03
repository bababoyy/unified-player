//! Synchronous boundary around the vendored Rust EJS port.
//!
//! Nothing here touches networking, credentials, or Tokio; the parent
//! runtime owns this engine on a dedicated thread with its own cancellation.

use std::collections::BTreeMap;
use std::sync::{atomic::AtomicBool, Arc};

use thiserror::Error;
use ytdlp_ejs::{
    builtin::JsRuntimeProvider, preprocess_player, registry::RuntimeType, JsChallengeError,
    JsChallengeType,
};

use super::super::{MAX_CHALLENGE_BYTES, MAX_PLAYER_SCRIPT_BYTES, MAX_SOLVER_OUTPUT_BYTES};

#[derive(Debug, Error)]
pub(super) enum Error {
    #[error("player script exceeded the local JavaScript size limit")]
    PlayerTooLarge,
    #[error("challenge values exceeded the local size limit")]
    ChallengesTooLarge,
    #[error("solver output exceeded the local size limit")]
    OutputTooLarge,
    #[error("the QuickJS compatibility backend failed")]
    Backend(#[source] JsChallengeError),
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Solutions {
    pub(crate) signatures: BTreeMap<String, String>,
    pub(crate) n_values: BTreeMap<String, String>,
}

/// SWC-generated player code that is safe to pass to a prepared solver.
///
/// The JavaScript is deliberately not exposed through `Debug` or a string
/// conversion so player code cannot be logged by accident.
pub(super) struct PreparedPlayer(String);

impl PreparedPlayer {
    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Preprocess a player script using the pinned EJS-compatible SWC pipeline.
pub(super) fn prepare_player(player_script: &str) -> Result<PreparedPlayer, Error> {
    if player_script.len() > MAX_PLAYER_SCRIPT_BYTES {
        return Err(Error::PlayerTooLarge);
    }
    Ok(PreparedPlayer(
        preprocess_player(player_script).map_err(Error::Backend)?,
    ))
}

/// A reusable `QuickJS` provider for one preprocessed player script.
///
/// The provider is synchronous and not `Send`; it must stay on the runtime's
/// worker thread.
pub(super) struct PreparedSolver {
    provider: JsRuntimeProvider,
    interrupted: Arc<AtomicBool>,
}

impl PreparedSolver {
    #[cfg(test)]
    fn quickjs(player: PreparedPlayer) -> Result<Self, Error> {
        Self::quickjs_with_interrupt(player, Arc::new(AtomicBool::new(false)))
    }

    #[allow(clippy::needless_pass_by_value)]
    pub(super) fn quickjs_with_interrupt(
        player: PreparedPlayer,
        interrupted: Arc<AtomicBool>,
    ) -> Result<Self, Error> {
        let provider = RuntimeType::QuickJS
            .create_quickjs_provider(&player.0, interrupted.clone())
            .map_err(Error::Backend)?;
        Ok(Self {
            provider,
            interrupted,
        })
    }

    /// Clear a previous interrupt request before starting a new solve.
    pub(super) fn clear_interrupt(&self) {
        self.interrupted
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.provider.clear_interrupt();
    }

    pub(super) fn solve(
        &mut self,
        signature_challenges: &[String],
        n_challenges: &[String],
    ) -> Result<Solutions, Error> {
        if challenge_bytes(signature_challenges, n_challenges) > MAX_CHALLENGE_BYTES {
            return Err(Error::ChallengesTooLarge);
        }
        let n_values = self
            .provider
            .solve_challenges(&JsChallengeType::N, n_challenges)
            .map_err(Error::Backend)?;
        let signatures = self
            .provider
            .solve_challenges(&JsChallengeType::Sig, signature_challenges)
            .map_err(Error::Backend)?;
        let solutions = Solutions {
            signatures: signatures.into_iter().collect(),
            n_values: n_values.into_iter().collect(),
        };
        validate_output_size(&solutions)?;
        Ok(solutions)
    }
}

pub(super) fn challenge_bytes(signature_challenges: &[String], n_challenges: &[String]) -> usize {
    signature_challenges
        .iter()
        .chain(n_challenges)
        .map(String::len)
        .sum()
}

fn validate_output_size(solutions: &Solutions) -> Result<(), Error> {
    let bytes = solutions
        .signatures
        .iter()
        .chain(solutions.n_values.iter())
        .try_fold(0usize, |total, (key, value)| {
            total
                .checked_add(key.len())
                .and_then(|total| total.checked_add(value.len()))
        })
        .ok_or(Error::OutputTooLarge)?;
    if bytes > MAX_SOLVER_OUTPUT_BYTES {
        return Err(Error::OutputTooLarge);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        thread,
        time::Duration,
    };

    use super::{prepare_player, validate_output_size, PreparedSolver, Solutions};

    const PLAYER: &str = r#"(function(){
function R(){this.v=new Map();}
R.prototype.set=function(k,v){this.v.set(k,v);};
R.prototype.get=function(k){return this.v.get(k);};
R.prototype.clone=function(){return this;};
R.prototype.transform=function(){var s=this.v.get("s");if(s)this.v.set("s",s.split("").reverse().join(""));var n=this.v.get("n");if(n)this.v.set("n",n.split("").reverse().join(""));};
var M={mark:function(a,b){return b;}};
var H=function(a,b,c){M.mark("alr","yes");var r=new R();if(c!==undefined)r.set(b,c);return r;};
}).call(this);"#;

    const SLOW_PLAYER: &str = r#"(function(){
function R(){this.v=new Map();}
R.prototype.set=function(k,v){this.v.set(k,v);};
R.prototype.get=function(k){return this.v.get(k);};
R.prototype.clone=function(){return this;};
R.prototype.transform=function(){while(true){}}
var M={mark:function(a,b){return b;}};
var H=function(a,b,c){M.mark("alr","yes");var r=new R();if(c!==undefined)r.set(b,c);return r;};
}).call(this);"#;

    #[test]
    fn quickjs_solver_reuses_preprocessed_player() {
        let prepared = prepare_player(PLAYER).expect("player should preprocess");
        assert!(!prepared.is_empty());
        let mut solver = PreparedSolver::quickjs(prepared).expect("QuickJS should initialize");
        let solutions = solver
            .solve(&["uvwxyz".to_owned()], &["abcdef".to_owned()])
            .expect("challenges should solve");
        assert_eq!(
            solutions.signatures.get("uvwxyz"),
            Some(&"zyxwvu".to_owned())
        );
        assert_eq!(solutions.n_values.get("abcdef"), Some(&"fedcba".to_owned()));
    }

    #[test]
    fn quickjs_interrupt_flag_stops_script_execution() {
        let prepared = prepare_player(SLOW_PLAYER).expect("slow player should preprocess");
        let interrupted = Arc::new(AtomicBool::new(false));
        let mut solver = PreparedSolver::quickjs_with_interrupt(prepared, interrupted.clone())
            .expect("QuickJS should initialize");
        solver.clear_interrupt();
        let signal = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            interrupted.store(true, Ordering::Relaxed);
        });

        let result = solver.solve(&["slow-signature".to_owned()], &[]);
        signal.join().expect("interrupt signal should finish");
        assert!(result.is_err(), "interrupted script must not complete");
    }

    #[test]
    fn solver_output_is_bounded_before_crossing_the_runtime_boundary() {
        let solutions = Solutions {
            signatures: [("signature".to_owned(), "x".repeat(256 * 1024))]
                .into_iter()
                .collect(),
            n_values: std::collections::BTreeMap::default(),
        };
        assert!(matches!(
            validate_output_size(&solutions),
            Err(super::Error::OutputTooLarge)
        ));
    }
}
