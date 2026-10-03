use std::{
    collections::BTreeMap, path::PathBuf, process::Stdio, sync::Mutex as StdMutex, time::Duration,
};

use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};
#[cfg(feature = "youtube-quickjs")]
use sha2::{Digest as _, Sha256};
use tokio::{
    io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

#[cfg(feature = "youtube-quickjs")]
mod quickjs;

const EJS_LIBRARY: &str = include_str!("ejs/yt.solver.lib.js");
const EJS_CORE: &str = include_str!("ejs/yt.solver.core.js");
/// Build-time provenance for the embedded solver assets and vendored Rust port.
/// Keep this token in sync with `ejs/manifest.toml` when refreshing EJS.
pub(crate) const EJS_BUNDLE_PROVENANCE: &str =
    "ejs_0.8.0_4fb477f4af56880cfd324c48bd4294a2d2294e50_lib_770831df5c46474f_core_ca259e4e3ddd37d9_rust_f2a266960642c3e2";
const SOLVER_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PLAYER_SCRIPT_BYTES: usize = 8 * 1024 * 1024;
const MAX_CHALLENGE_BYTES: usize = 256 * 1024;
const MAX_SOLVER_OUTPUT_BYTES: usize = 256 * 1024;

// The JavaScript process receives only public player code and challenge values.
// The vm context deliberately has no Node globals, filesystem, or network APIs.
const NODE_RUNNER: &str = r#"
const readline = require("node:readline");
const vm = require("node:vm");
const context = vm.createContext({ input: null }, {
  codeGeneration: { strings: true, wasm: false },
});
let initialized = false;

async function run() {
  const lines = readline.createInterface({ input: process.stdin });
  for await (const line of lines) {
    if (!line.trim()) continue;
    let output;
    try {
      const envelope = JSON.parse(line);
      if (!initialized) {
        vm.runInContext(
          envelope.lib + "\nvar meriyah = lib.meriyah; var astring = lib.astring;\n" + envelope.core,
          context,
          { timeout: 5000 },
        );
        initialized = true;
      }
      context.input = envelope.input;
      output = vm.runInContext("jsc(input)", context, { timeout: 5000 });
    } catch (_) {
      output = { type: "error" };
    }
    process.stdout.write(JSON.stringify(output) + "\n");
  }
}

run().catch(() => { process.exitCode = 2; });
"#;

#[derive(Debug, Default)]
pub(crate) struct Solutions {
    pub(crate) signatures: BTreeMap<String, String>,
    pub(crate) n_values: BTreeMap<String, String>,
    #[allow(dead_code)]
    pub(crate) preprocessed_player: Option<String>,
}

#[derive(Serialize)]
struct Envelope<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    lib: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    core: Option<&'static str>,
    input: Input<'a>,
}

#[derive(Serialize)]
struct Input<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    player: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    preprocessed_player: Option<&'a str>,
    output_preprocessed: bool,
    requests: [Request<'a>; 2],
}

#[derive(Serialize)]
struct Request<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    challenges: &'a [String],
}

#[derive(Deserialize)]
struct Output {
    #[serde(rename = "type")]
    kind: String,
    preprocessed_player: Option<String>,
    responses: Option<Vec<Response>>,
}

#[derive(Deserialize)]
struct Response {
    #[serde(rename = "type")]
    kind: String,
    data: Option<BTreeMap<String, String>>,
}

pub(crate) struct Solver {
    worker: Mutex<Option<JavaScriptWorker>>,
    last_backend: StdMutex<Option<&'static str>>,
    #[cfg(feature = "youtube-quickjs")]
    quickjs: Option<QuickJsState>,
}

impl Default for Solver {
    fn default() -> Self {
        Self {
            worker: Mutex::new(None),
            last_backend: StdMutex::new(None),
            #[cfg(feature = "youtube-quickjs")]
            quickjs: None,
        }
    }
}

#[cfg(feature = "youtube-quickjs")]
struct QuickJsState {
    runtime: quickjs::InProcessRuntime,
    player_hash: Mutex<Option<[u8; 32]>>,
}

struct JavaScriptWorker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    initialized: bool,
}

impl JavaScriptWorker {
    fn spawn() -> Result<Self> {
        let node = node_executable().ok_or_else(|| {
            anyhow::anyhow!(
                "Node.js was not found; install a supported JavaScript runtime for YouTube playback"
            )
        })?;
        let mut child = Command::new(node);
        child
            .args(["--no-addons", "--input-type=commonjs", "-e", NODE_RUNNER])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = child
            .spawn()
            .context("start the bounded YouTube JavaScript runtime")?;
        let stdin = child
            .stdin
            .take()
            .context("open the YouTube JavaScript runtime input")?;
        let stdout = child
            .stdout
            .take()
            .context("open the YouTube JavaScript runtime output")?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            initialized: false,
        })
    }

    async fn solve_input(
        &mut self,
        input: Input<'_>,
        signature_challenges: &[String],
        n_challenges: &[String],
        cancellation: &CancellationToken,
    ) -> Result<Solutions> {
        let input = serde_json::to_vec(&Envelope {
            lib: (!self.initialized).then_some(EJS_LIBRARY),
            core: (!self.initialized).then_some(EJS_CORE),
            input: Input {
                player: input.player,
                preprocessed_player: input.preprocessed_player,
                kind: input.kind,
                output_preprocessed: input.output_preprocessed,
                requests: [
                    Request {
                        kind: "n",
                        challenges: n_challenges,
                    },
                    Request {
                        kind: "sig",
                        challenges: signature_challenges,
                    },
                ],
            },
        })
        .context("encode YouTube JavaScript challenge input")?;
        let mut line = input;
        line.push(b'\n');
        let write_result = async {
            self.stdin.write_all(&line).await?;
            self.stdin.flush().await
        };
        tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                bail!("YouTube JavaScript challenge solving was cancelled");
            }
            result = write_result => {
                result.context("send the YouTube JavaScript challenge input")?;
            }
        }
        self.initialized = true;

        let output = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                bail!("YouTube JavaScript challenge solving was cancelled");
            }
            result = timeout(SOLVER_TIMEOUT, read_bounded_line(&mut self.stdout)) => {
                result
                    .context("run the bounded YouTube JavaScript runtime")??
            }
        };
        let output =
            output.context("YouTube JavaScript runtime exited before returning a result")?;
        parse_output(&output)
    }

    async fn terminate(&mut self) {
        let _ = self.child.kill().await;
    }
}

async fn read_bounded_line<R>(reader: &mut R) -> Result<Option<Vec<u8>>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut line = Vec::new();
    loop {
        let buffer = reader
            .fill_buf()
            .await
            .context("read the YouTube JavaScript solver result")?;
        if buffer.is_empty() {
            return Ok((!line.is_empty()).then_some(line));
        }
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(buffer.len(), |index| index + 1);
        anyhow::ensure!(
            line.len().saturating_add(take) <= MAX_SOLVER_OUTPUT_BYTES,
            "YouTube JavaScript solver returned too much output"
        );
        line.extend_from_slice(&buffer[..take]);
        reader.consume(take);
        if newline.is_some() {
            return Ok(Some(line));
        }
    }
}

impl Solver {
    /// The default solver: `QuickJS` when compiled in, otherwise Node.
    pub(crate) fn configured() -> Self {
        #[cfg(feature = "youtube-quickjs")]
        return Self::with_quickjs();

        #[cfg(not(feature = "youtube-quickjs"))]
        Self::default()
    }

    /// Build a solver for the persisted `YouTube` JavaScript runtime setting.
    ///
    /// `QuickJS` is compile-time optional; a build without it serves a
    /// `QuickJs` selection through the Node fallback.
    pub(crate) fn configured_for(runtime: crate::config::YouTubeJavaScriptRuntime) -> Self {
        match runtime {
            crate::config::YouTubeJavaScriptRuntime::Auto
            | crate::config::YouTubeJavaScriptRuntime::QuickJs => Self::configured(),
            crate::config::YouTubeJavaScriptRuntime::Node => Self::default(),
        }
    }

    #[cfg(feature = "youtube-quickjs")]
    pub(crate) fn with_quickjs() -> Self {
        Self {
            worker: Mutex::new(None),
            last_backend: StdMutex::new(None),
            quickjs: Some(QuickJsState {
                runtime: quickjs::InProcessRuntime::new(),
                player_hash: Mutex::new(None),
            }),
        }
    }

    #[cfg(feature = "youtube-quickjs")]
    async fn solve_quickjs(
        &self,
        player_script: &str,
        signature_challenges: &[String],
        n_challenges: &[String],
        cancellation: &CancellationToken,
    ) -> Result<Solutions> {
        let state = self
            .quickjs
            .as_ref()
            .expect("QuickJS state was initialized");
        let player_hash: [u8; 32] = Sha256::digest(player_script.as_bytes()).into();
        let mut known_player = state.player_hash.lock().await;
        let player = if *known_player == Some(player_hash) {
            None
        } else {
            Some(player_script)
        };
        let result = state
            .runtime
            .solve(player, signature_challenges, n_challenges, cancellation)
            .await
            .map_err(|error| anyhow::anyhow!(error.to_string()));
        if result.is_ok() {
            *known_player = Some(player_hash);
            if !signature_challenges.is_empty() || !n_challenges.is_empty() {
                self.remember_backend("QuickJS");
            }
        } else {
            *known_player = None;
        }
        result.map(|solutions| Solutions {
            signatures: solutions.signatures,
            n_values: solutions.n_values,
            preprocessed_player: None,
        })
    }

    async fn run_input(
        &self,
        input: Input<'_>,
        signature_challenges: &[String],
        n_challenges: &[String],
        cancellation: &CancellationToken,
    ) -> Result<Solutions> {
        let mut worker = self.worker.lock().await;
        if worker.is_none() {
            *worker = Some(JavaScriptWorker::spawn()?);
        }
        let result = worker
            .as_mut()
            .expect("JavaScript worker was initialized")
            .solve_input(input, signature_challenges, n_challenges, cancellation)
            .await;
        if result.is_err() {
            if let Some(mut failed_worker) = worker.take() {
                failed_worker.terminate().await;
            }
        }
        if result.is_ok() && (!signature_challenges.is_empty() || !n_challenges.is_empty()) {
            self.remember_backend("Node");
        }
        result
    }

    fn remember_backend(&self, backend: &'static str) {
        if let Ok(mut last_backend) = self.last_backend.lock() {
            *last_backend = Some(backend);
        }
    }

    pub(crate) fn reset_last_backend(&self) {
        if let Ok(mut last_backend) = self.last_backend.lock() {
            *last_backend = None;
        }
    }

    /// Return the runtime that most recently solved a JavaScript challenge.
    /// This is intentionally a label only; it never includes process paths or
    /// account/session material.
    pub(crate) fn last_backend(&self) -> Option<&'static str> {
        self.last_backend.lock().ok().and_then(|backend| *backend)
    }

    /// Return a stable, privacy-safe label for the runtime that solved the
    /// most recent challenge. A missing label means no challenge completed in
    /// this solver instance, not that a process path should be logged.
    pub(crate) fn last_backend_diagnostic_label(&self) -> &'static str {
        match self.last_backend() {
            Some("QuickJS") => "quickjs",
            Some("Node") => "node",
            Some(_) => "unknown",
            None => "not_recorded",
        }
    }

    /// Return the configured runtime when no challenge has completed yet.
    /// The `configured_` prefix prevents a failed attempt from being mistaken
    /// for a successful backend selection in diagnostics.
    pub(crate) fn configured_backend_diagnostic_label(&self) -> &'static str {
        #[cfg(feature = "youtube-quickjs")]
        if self.quickjs.is_some() {
            return "configured_quickjs";
        }
        "configured_node"
    }

    async fn solve_node(
        &self,
        player_script: &str,
        signature_challenges: &[String],
        n_challenges: &[String],
        cancellation: &CancellationToken,
    ) -> Result<Solutions> {
        validate_input(player_script, signature_challenges, n_challenges)?;
        self.run_input(
            Input {
                kind: "player",
                player: Some(player_script),
                preprocessed_player: None,
                output_preprocessed: false,
                requests: [
                    Request {
                        kind: "n",
                        challenges: n_challenges,
                    },
                    Request {
                        kind: "sig",
                        challenges: signature_challenges,
                    },
                ],
            },
            signature_challenges,
            n_challenges,
            cancellation,
        )
        .await
    }

    pub(crate) async fn solve(
        &self,
        player_script: &str,
        signature_challenges: &[String],
        n_challenges: &[String],
        cancellation: &CancellationToken,
    ) -> Result<Solutions> {
        if cancellation.is_cancelled() {
            bail!("YouTube JavaScript challenge solving was cancelled");
        }
        #[cfg(feature = "youtube-quickjs")]
        if self.quickjs.is_some() {
            match self
                .solve_quickjs(
                    player_script,
                    signature_challenges,
                    n_challenges,
                    cancellation,
                )
                .await
            {
                Ok(solutions) => return Ok(solutions),
                Err(error) if cancellation.is_cancelled() => return Err(error),
                Err(_) => {}
            }
        }
        self.solve_node(
            player_script,
            signature_challenges,
            n_challenges,
            cancellation,
        )
        .await
    }

    /// Initialize the configured JavaScript worker and prepare the supplied
    /// player without claiming that playback used a JavaScript backend.
    pub(crate) async fn warm_up(
        &self,
        player_script: &str,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        self.solve(player_script, &[], &[], cancellation)
            .await
            .map(|_| ())
    }

    #[allow(dead_code)]
    pub(crate) async fn prepare(
        &self,
        player_script: &str,
        cancellation: &CancellationToken,
    ) -> Result<String> {
        if cancellation.is_cancelled() {
            bail!("YouTube JavaScript challenge solving was cancelled");
        }
        validate_script(player_script)?;
        let solutions = self
            .run_input(
                Input {
                    kind: "player",
                    player: Some(player_script),
                    preprocessed_player: None,
                    output_preprocessed: true,
                    requests: [
                        Request {
                            kind: "n",
                            challenges: &[],
                        },
                        Request {
                            kind: "sig",
                            challenges: &[],
                        },
                    ],
                },
                &[],
                &[],
                cancellation,
            )
            .await?;
        solutions
            .preprocessed_player
            .context("YouTube JavaScript solver returned no preprocessed player")
    }

    #[allow(dead_code)]
    pub(crate) async fn solve_preprocessed(
        &self,
        preprocessed_player: &str,
        signature_challenges: &[String],
        n_challenges: &[String],
        cancellation: &CancellationToken,
    ) -> Result<Solutions> {
        if cancellation.is_cancelled() {
            bail!("YouTube JavaScript challenge solving was cancelled");
        }
        validate_input(preprocessed_player, signature_challenges, n_challenges)?;
        self.run_input(
            Input {
                kind: "preprocessed",
                player: None,
                preprocessed_player: Some(preprocessed_player),
                output_preprocessed: false,
                requests: [
                    Request {
                        kind: "n",
                        challenges: n_challenges,
                    },
                    Request {
                        kind: "sig",
                        challenges: signature_challenges,
                    },
                ],
            },
            signature_challenges,
            n_challenges,
            cancellation,
        )
        .await
    }
}

#[cfg(test)]
pub(crate) async fn solve(
    player_script: &str,
    signature_challenges: &[String],
    n_challenges: &[String],
    cancellation: &CancellationToken,
) -> Result<Solutions> {
    Solver::default()
        .solve(
            player_script,
            signature_challenges,
            n_challenges,
            cancellation,
        )
        .await
}

fn validate_input(
    player_script: &str,
    signature_challenges: &[String],
    n_challenges: &[String],
) -> Result<()> {
    validate_script(player_script)?;
    let challenge_bytes = signature_challenges
        .iter()
        .chain(n_challenges)
        .map(String::len)
        .sum::<usize>();
    anyhow::ensure!(
        challenge_bytes <= MAX_CHALLENGE_BYTES,
        "YouTube JavaScript challenge exceeded the local size limit"
    );
    Ok(())
}

fn validate_script(player_script: &str) -> Result<()> {
    anyhow::ensure!(
        player_script.len() <= MAX_PLAYER_SCRIPT_BYTES,
        "YouTube player script exceeded the local JavaScript size limit"
    );
    Ok(())
}

fn parse_output(output: &[u8]) -> Result<Solutions> {
    let parsed: Output =
        serde_json::from_slice(output).context("parse the YouTube JavaScript solver result")?;
    anyhow::ensure!(
        parsed.kind == "result",
        "YouTube JavaScript solver did not return a result"
    );
    let responses = parsed
        .responses
        .context("YouTube JavaScript solver returned no responses")?;
    anyhow::ensure!(
        responses.len() == 2,
        "YouTube JavaScript solver response shape changed"
    );

    let n_values = response_data(&responses[0], "n")?;
    let signatures = response_data(&responses[1], "sig")?;
    Ok(Solutions {
        signatures,
        n_values,
        preprocessed_player: parsed.preprocessed_player,
    })
}

fn response_data(response: &Response, kind: &str) -> Result<BTreeMap<String, String>> {
    anyhow::ensure!(
        response.kind == "result",
        "YouTube JavaScript solver could not solve the {kind} challenge"
    );
    response
        .data
        .clone()
        .context("YouTube JavaScript solver returned no challenge values")
}

fn node_executable() -> Option<PathBuf> {
    which::which("node")
        .ok()
        .or_else(|| which::which("nodejs").ok())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    #[cfg(feature = "youtube-quickjs")]
    use std::time::Duration;

    use serde::Deserialize;
    use sha2::{Digest as _, Sha256};

    use super::{node_executable, solve, Solver, EJS_CORE, EJS_LIBRARY};
    use tokio_util::sync::CancellationToken;

    const EJS_MANIFEST: &str = include_str!("ejs/manifest.toml");
    const COMPATIBILITY_VECTOR: &str = include_str!("ejs/fixtures/synthetic_reverse.json");

    #[derive(Deserialize)]
    struct Manifest {
        schema_version: u32,
        upstream_repository: String,
        upstream_tag: String,
        upstream_commit: String,
        generated_by: String,
        protocol: String,
        assets: BTreeMap<String, Asset>,
        dependencies: Dependencies,
        rust_port: RustPort,
    }

    #[derive(Deserialize)]
    struct Asset {
        path: String,
        source: String,
        sha256: String,
    }

    #[derive(Deserialize)]
    struct Dependencies {
        meriyah: String,
        astring: String,
    }

    #[derive(Deserialize)]
    struct RustPort {
        repository: String,
        crate_version: String,
        commit: String,
        features: Vec<String>,
    }

    #[derive(Deserialize)]
    struct CompatibilityVector {
        name: String,
        player: String,
        signature_challenge: String,
        signature_expected: String,
        n_challenge: String,
        n_expected: String,
    }

    fn sha256_hex(value: &str) -> String {
        Sha256::digest(value.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect()
    }

    const PLAYER_FIXTURE: &str = r#"(function(){
function R(){this.v=new Map();}
R.prototype.set=function(k,v){this.v.set(k,v);};
R.prototype.get=function(k){return this.v.get(k);};
R.prototype.clone=function(){return this;};
R.prototype.transform=function(){var s=this.v.get("s");if(s)this.v.set("s",s.split("").reverse().join(""));var n=this.v.get("n");if(n)this.v.set("n",n.split("").reverse().join(""));};
var M={mark:function(a,b){return b;}};
var H=function(a,b,c){M.mark("alr","yes");var r=new R();if(c!==undefined)r.set(b,c);return r;};
}).call(this);"#;

    #[cfg(feature = "youtube-quickjs")]
    const SLOW_PLAYER_FIXTURE: &str = r#"(function(){
function R(){this.v=new Map();}
R.prototype.set=function(k,v){this.v.set(k,v);};
R.prototype.get=function(k){return this.v.get(k);};
R.prototype.clone=function(){return this;};
R.prototype.transform=function(){while(true){}}
var M={mark:function(a,b){return b;}};
var H=function(a,b,c){M.mark("alr","yes");var r=new R();if(c!==undefined)r.set(b,c);return r;};
}).call(this);"#;

    #[test]
    fn bundled_solver_assets_are_present() {
        assert!(super::EJS_LIBRARY.len() > 100_000);
        assert!(super::EJS_CORE.len() > 5_000);
    }

    #[test]
    fn bundled_solver_assets_match_upstream_manifest() {
        let manifest: Manifest = toml::from_str(EJS_MANIFEST).expect("valid EJS manifest");
        assert_eq!(manifest.schema_version, 1);
        assert_eq!(
            manifest.upstream_repository,
            "https://github.com/yt-dlp/ejs"
        );
        assert_eq!(manifest.upstream_tag, "0.8.0");
        assert_eq!(
            manifest.upstream_commit,
            "4fb477f4af56880cfd324c48bd4294a2d2294e50"
        );
        assert_eq!(manifest.generated_by, "yt-dlp/ejs");
        assert_eq!(manifest.protocol, "player-v1");
        assert_eq!(manifest.dependencies.meriyah, "6.1.4");
        assert_eq!(manifest.dependencies.astring, "1.9.0");
        assert_eq!(
            manifest.rust_port.repository,
            "https://github.com/ahaoboy/ytdlp-ejs"
        );
        assert_eq!(manifest.rust_port.crate_version, "0.1.1");
        assert_eq!(
            manifest.rust_port.commit,
            "f2a266960642c3e2a15a359a7e9f43d4faa800e1"
        );
        assert_eq!(manifest.rust_port.features, vec!["qjs"]);

        let lib = manifest.assets.get("lib").expect("manifest lib asset");
        assert_eq!(lib.path, "yt.solver.lib.js");
        assert!(lib.source.ends_with("/0.8.0/yt.solver.lib.js"));
        assert_eq!(lib.sha256, sha256_hex(EJS_LIBRARY));

        let core = manifest.assets.get("core").expect("manifest core asset");
        assert_eq!(core.path, "yt.solver.core.js");
        assert!(core.source.ends_with("/0.8.0/yt.solver.core.js"));
        assert_eq!(core.sha256, sha256_hex(EJS_CORE));
        assert!(super::EJS_BUNDLE_PROVENANCE.contains("ejs_0.8.0_4fb477f4"));
        assert!(super::EJS_BUNDLE_PROVENANCE.contains("lib_770831df5c46474f"));
        assert!(super::EJS_BUNDLE_PROVENANCE.contains("core_ca259e4e3ddd37d9"));
        assert!(super::EJS_BUNDLE_PROVENANCE.contains("rust_f2a266960642c3e2"));
    }

    #[test]
    fn compatibility_vector_is_well_formed() {
        let vector: CompatibilityVector =
            serde_json::from_str(COMPATIBILITY_VECTOR).expect("valid compatibility vector");
        assert_eq!(vector.name, "synthetic-reverse");
        assert_eq!(vector.player, PLAYER_FIXTURE);
        assert_eq!(vector.signature_challenge, "uvwxyz");
        assert_eq!(vector.signature_expected, "zyxwvu");
        assert_eq!(vector.n_challenge, "abcdef");
        assert_eq!(vector.n_expected, "fedcba");
    }

    #[tokio::test]
    async fn invalid_player_is_rejected_without_returning_script_text() {
        if node_executable().is_none() {
            return;
        }
        let error = solve(
            "not a YouTube player",
            &["private-signature-challenge".to_string()],
            &[],
            &CancellationToken::new(),
        )
        .await
        .expect_err("invalid player should not solve");
        let rendered = error.to_string();
        assert!(!rendered.contains("private-signature-challenge"));
        assert!(!rendered.contains("not a YouTube player"));
    }

    #[tokio::test]
    async fn ejs_solver_runs_a_player_inside_the_bounded_runtime() {
        if node_executable().is_none() {
            return;
        }
        let solutions = solve(
            PLAYER_FIXTURE,
            &["uvwxyz".to_string()],
            &["abcdef".to_string()],
            &CancellationToken::new(),
        )
        .await
        .expect("fixture player should solve");
        assert_eq!(
            solutions.signatures.get("uvwxyz"),
            Some(&"zyxwvu".to_string())
        );
        assert_eq!(
            solutions.n_values.get("abcdef"),
            Some(&"fedcba".to_string())
        );
    }

    #[tokio::test]
    async fn prepared_player_can_be_reused_without_reparsing_the_source() {
        if node_executable().is_none() {
            return;
        }
        let solver = Solver::default();
        let prepared = solver
            .prepare(PLAYER_FIXTURE, &CancellationToken::new())
            .await
            .expect("player should be preprocessed");
        assert!(!prepared.is_empty());

        let solutions = solver
            .solve_preprocessed(
                &prepared,
                &["prepared-signature".to_string()],
                &["prepared-n".to_string()],
                &CancellationToken::new(),
            )
            .await
            .expect("preprocessed player should solve");
        assert_eq!(
            solutions.signatures.get("prepared-signature"),
            Some(&"erutangis-deraperp".to_string())
        );
        assert_eq!(
            solutions.n_values.get("prepared-n"),
            Some(&"n-deraperp".to_string())
        );
    }

    #[tokio::test]
    async fn cancelled_solver_does_not_complete_a_challenge() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let error = solve(PLAYER_FIXTURE, &[], &[], &cancellation)
            .await
            .expect_err("cancelled solver should stop");
        assert!(error.to_string().contains("cancelled"));
    }

    #[tokio::test]
    async fn persistent_solver_reuses_the_loaded_ejs_runtime() {
        if node_executable().is_none() {
            return;
        }
        let solver = Solver::default();
        let first = solver
            .solve(
                PLAYER_FIXTURE,
                &["first-signature".to_string()],
                &["first-n".to_string()],
                &CancellationToken::new(),
            )
            .await
            .expect("first solver request should complete");
        let second = solver
            .solve(
                PLAYER_FIXTURE,
                &["second-signature".to_string()],
                &["second-n".to_string()],
                &CancellationToken::new(),
            )
            .await
            .expect("second solver request should reuse the worker");
        assert_eq!(
            first.signatures.get("first-signature"),
            Some(&"erutangis-tsrif".to_string())
        );
        assert_eq!(first.n_values.get("first-n"), Some(&"n-tsrif".to_string()));
        assert_eq!(
            second.signatures.get("second-signature"),
            Some(&"erutangis-dnoces".to_string())
        );
        assert_eq!(
            second.n_values.get("second-n"),
            Some(&"n-dnoces".to_string())
        );
        assert_eq!(solver.last_backend(), Some("Node"));
    }

    #[tokio::test]
    async fn warm_up_prepares_the_solver_without_claiming_a_backend() {
        if node_executable().is_none() {
            return;
        }
        let solver = Solver::default();
        solver
            .warm_up(PLAYER_FIXTURE, &CancellationToken::new())
            .await
            .expect("warm-up should prepare the player");
        assert_eq!(solver.last_backend(), None);

        solver
            .solve(
                PLAYER_FIXTURE,
                &["warm-up-follow-up".to_string()],
                &[],
                &CancellationToken::new(),
            )
            .await
            .expect("the prepared worker should solve a later challenge");
        assert_eq!(solver.last_backend(), Some("Node"));
    }

    #[tokio::test]
    #[ignore = "local performance benchmark"]
    async fn node_solver_benchmark() {
        if node_executable().is_none() {
            return;
        }
        let runs = 100;
        let mut cold = Vec::with_capacity(runs);
        for index in 0..runs {
            let solver = Solver::default();
            let started = std::time::Instant::now();
            solver
                .solve(
                    PLAYER_FIXTURE,
                    &[format!("node-cold-signature-{index}")],
                    &[format!("node-cold-n-{index}")],
                    &CancellationToken::new(),
                )
                .await
                .expect("Node cold benchmark request should solve");
            cold.push(started.elapsed());
        }

        let solver = Solver::default();
        solver
            .solve(
                PLAYER_FIXTURE,
                &["node-warm-seed-signature".to_owned()],
                &["node-warm-seed-n".to_owned()],
                &CancellationToken::new(),
            )
            .await
            .expect("Node warm benchmark seed should solve");
        let mut warm = Vec::with_capacity(runs);
        for index in 0..runs {
            let started = std::time::Instant::now();
            solver
                .solve(
                    PLAYER_FIXTURE,
                    &[format!("node-warm-signature-{index}")],
                    &[format!("node-warm-n-{index}")],
                    &CancellationToken::new(),
                )
                .await
                .expect("Node warm benchmark request should solve");
            warm.push(started.elapsed());
        }

        let first = PLAYER_FIXTURE.to_owned();
        let second = format!("{PLAYER_FIXTURE}\n// synthetic-node-reused-player");
        for (name, player) in [("first", &first), ("second", &second)] {
            solver
                .solve(
                    player,
                    &[format!("node-reuse-seed-{name}-signature")],
                    &[format!("node-reuse-seed-{name}-n")],
                    &CancellationToken::new(),
                )
                .await
                .expect("Node reuse benchmark seed should solve");
        }
        let mut reuse = Vec::with_capacity(runs);
        for index in 0..runs {
            let player = if index % 2 == 0 { &first } else { &second };
            let started = std::time::Instant::now();
            solver
                .solve(
                    player,
                    &[format!("node-reuse-signature-{index}")],
                    &[format!("node-reuse-n-{index}")],
                    &CancellationToken::new(),
                )
                .await
                .expect("Node reuse benchmark request should solve");
            reuse.push(started.elapsed());
        }

        let mut churn = Vec::with_capacity(runs);
        for index in 0..runs {
            let player = format!("{PLAYER_FIXTURE}\n// synthetic-node-player-{index}");
            let started = std::time::Instant::now();
            solver
                .solve(
                    &player,
                    &[format!("node-churn-signature-{index}")],
                    &[format!("node-churn-n-{index}")],
                    &CancellationToken::new(),
                )
                .await
                .expect("Node churn benchmark request should solve");
            churn.push(started.elapsed());
        }

        println!(
            "youtube_ejs_node_benchmark runs={runs} cold_us={:?} warm_us={:?} reuse_us={:?} churn_us={:?}",
            benchmark_summary(cold),
            benchmark_summary(warm),
            benchmark_summary(reuse),
            benchmark_summary(churn),
        );
    }

    fn benchmark_summary(mut samples: Vec<std::time::Duration>) -> (u128, u128, u128) {
        samples.sort_unstable();
        let micros = samples
            .into_iter()
            .map(|sample| sample.as_micros())
            .collect::<Vec<_>>();
        (
            micros[micros.len() / 2],
            micros[(micros.len() * 95).saturating_sub(1) / 100],
            *micros.last().expect("benchmark has samples"),
        )
    }

    #[cfg(feature = "youtube-quickjs")]
    #[tokio::test]
    async fn configured_quickjs_backend_solves_without_a_child_process() {
        let solver = Solver::configured();
        let solutions = solver
            .solve(
                PLAYER_FIXTURE,
                &["in-process-signature".to_owned()],
                &["in-process-n".to_owned()],
                &CancellationToken::new(),
            )
            .await
            .expect("QuickJS backend should solve");
        assert_eq!(
            solutions.signatures.get("in-process-signature"),
            Some(&"erutangis-ssecorp-ni".to_owned())
        );
        assert_eq!(
            solutions.n_values.get("in-process-n"),
            Some(&"n-ssecorp-ni".to_owned())
        );
        assert_eq!(solver.last_backend(), Some("QuickJS"));
        assert_eq!(solver.last_backend_diagnostic_label(), "quickjs");
    }

    #[cfg(feature = "youtube-quickjs")]
    #[tokio::test]
    async fn quickjs_cancellation_does_not_fall_back_to_node() {
        let solver = Solver::with_quickjs();
        let cancellation = CancellationToken::new();
        let cancel = cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel.cancel();
        });
        let started = std::time::Instant::now();
        let error = solver
            .solve(
                SLOW_PLAYER_FIXTURE,
                &["cancellation-signature".to_owned()],
                &["cancellation-n".to_owned()],
                &cancellation,
            )
            .await
            .expect_err("cancelled QuickJS execution should not use Node");
        assert!(cancellation.is_cancelled());
        assert!(error.to_string().contains("cancelled"));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(solver.last_backend(), None);
    }

    #[cfg(feature = "youtube-quickjs")]
    #[tokio::test]
    async fn quickjs_and_node_backends_match_the_compatibility_vector() {
        if node_executable().is_none() {
            return;
        }
        let cancellation = CancellationToken::new();
        let quickjs_solver = Solver::with_quickjs();
        let node_solver = Solver::default();
        let quickjs = quickjs_solver
            .solve(
                PLAYER_FIXTURE,
                &["uvwxyz".to_owned()],
                &["abcdef".to_owned()],
                &cancellation,
            )
            .await
            .expect("QuickJS compatibility vector should solve");
        let node = node_solver
            .solve(
                PLAYER_FIXTURE,
                &["uvwxyz".to_owned()],
                &["abcdef".to_owned()],
                &cancellation,
            )
            .await
            .expect("Node compatibility vector should solve");
        assert_eq!(quickjs.signatures, node.signatures);
        assert_eq!(quickjs.n_values, node.n_values);
        assert_eq!(quickjs_solver.last_backend(), Some("QuickJS"));
        assert_eq!(node_solver.last_backend(), Some("Node"));
    }

    #[test]
    fn node_setting_bypasses_quickjs_and_auto_follows_the_build() {
        use crate::config::YouTubeJavaScriptRuntime;
        let node = Solver::configured_for(YouTubeJavaScriptRuntime::Node);
        assert_eq!(
            node.configured_backend_diagnostic_label(),
            "configured_node"
        );
        let expected = if cfg!(feature = "youtube-quickjs") {
            "configured_quickjs"
        } else {
            "configured_node"
        };
        for runtime in [
            YouTubeJavaScriptRuntime::Auto,
            YouTubeJavaScriptRuntime::QuickJs,
        ] {
            assert_eq!(
                Solver::configured_for(runtime).configured_backend_diagnostic_label(),
                expected
            );
        }
    }
}
