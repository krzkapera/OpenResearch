//! Slurm backend — submit an experiment as a batch job on a Slurm cluster.
//!
//! orx talks to the cluster's **login node** over ssh (reusing the ssh
//! backend's multiplexed transport) and drives the Slurm CLI there:
//! `sbatch --parsable` to submit, `squeue`/`sacct` to poll, `scancel` to
//! cancel. No `slurmrestd`: the REST daemon needs cluster-admin setup that
//! most users don't have, while everyone with a cluster account can ssh to
//! the login node — the same trade SkyPilot makes.
//!
//! ## Agent-authored `job.sbatch`
//!
//! After staging the snapshot into the remote run dir, orx looks for an
//! agent-authored `job.sbatch` at the staged repo root and submits that file
//! with `sbatch` from the run directory. orx does **not** generate the batch
//! script from the experiment's run command or resource settings.
//!
//! Contract the agent must honour (relative paths resolve against the run
//! dir, which is also sbatch's working directory):
//!   `#SBATCH --output=log`
//!   `#SBATCH --error=log`
//!   write `exit_code` in the run dir when the payload finishes
//!   (e.g. `echo "$code" > exit_code` from the run dir, or adjust if the
//!   payload `cd`s into `repo/`)
//!
//! ## Remote layout
//!
//! Under the configurable remote root (default `~/scratch/.orx`, see
//! `remoteRoot` in `slurm.json`):
//!   source/     content-addressed snapshot tarballs
//!   runs/<id>/
//!     repo/       the staged experiment snapshot
//!     log         merged stdout/stderr (`#SBATCH --output`/`--error`)
//!     exit_code   written by the agent's job.sbatch when the payload ends
//!
//! The reattach handle is (host, slurm job id); the run dir derives from the
//! run id + current `remoteRoot`. Job state is read exit_code-first
//! (scheduler-independent truth, like the ssh backend), then `squeue` for
//! live jobs, then `sacct` for jobs that left the queue without writing an
//! exit code (scancel/timeout/node failure). `sacct` may be disabled
//! cluster-wide, so it's a best-effort fallback, not a dependency.

use serde::{Deserialize, Serialize};

use super::ssh::{ssh_run, SshTarget};
use crate::error::{anyhow, Result};

/// Default remote base when `slurm.json` omits `remoteRoot`.
pub const DEFAULT_REMOTE_ROOT: &str = "~/scratch/.orx";

// --- settings ---------------------------------------------------------------

/// User-tunable cluster defaults, stored at
/// `$XDG_CONFIG_HOME/openresearch/slurm.json`. No secrets in here — ssh
/// holds all auth. Every field is optional: a bare `sbatch` on a
/// single-partition cluster works with no configuration at all.
///
/// `partition` / `account` / `time_limit` remain available for UI/CLI
/// convenience but are **not** applied at submit time — the agent's
/// `job.sbatch` owns all `#SBATCH` directives.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlurmSettings {
    /// ssh config host alias of the login node; the `--host` default.
    #[serde(default)]
    pub host: Option<String>,
    /// Optional partition hint (not applied to submit; agent owns `#SBATCH`).
    #[serde(default)]
    pub partition: Option<String>,
    /// Optional account hint (not applied to submit; agent owns `#SBATCH`).
    #[serde(default)]
    pub account: Option<String>,
    /// Optional time-limit hint (not applied to submit; agent owns `#SBATCH`).
    #[serde(default)]
    pub time_limit: Option<String>,
    /// Remote base for source tarballs and run dirs. Default
    /// `~/scratch/.orx`. Accepts `~/…`, `$HOME/…`, a home-relative path, or
    /// an absolute path.
    #[serde(default)]
    pub remote_root: Option<String>,
}

fn settings_path() -> std::path::PathBuf {
    crate::config::config_dir().join("slurm.json")
}

/// `Ok(None)` when the file is missing — slurm not configured (still usable
/// with explicit `--host`).
pub fn load_settings() -> Result<Option<SlurmSettings>> {
    let raw = match std::fs::read_to_string(settings_path()) {
        Ok(raw) => raw,
        Err(_) => return Ok(None),
    };
    match serde_json::from_str::<SlurmSettings>(&raw) {
        Ok(s) => Ok(Some(s)),
        Err(e) => Err(anyhow!(
            "Unreadable {} ({}). Fix or delete it and reconfigure.",
            settings_path().display(),
            e
        )),
    }
}

pub fn save_settings(settings: &SlurmSettings) -> Result<()> {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = format!("{}\n", serde_json::to_string_pretty(settings)?);
    std::fs::write(&path, body)?;
    Ok(())
}

/// Effective `remoteRoot` from settings (or the default).
pub fn effective_remote_root(settings: &SlurmSettings) -> String {
    settings
        .remote_root
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_REMOTE_ROOT)
        .to_string()
}

/// Strip a leading `~/` or `$HOME/` so the result is either absolute or
/// home-relative (no trailing slash).
pub fn normalize_remote_root(raw: &str) -> String {
    let s = raw.trim();
    let s = s
        .strip_prefix("~/")
        .or_else(|| s.strip_prefix("$HOME/"))
        .or_else(|| (s == "~" || s == "$HOME").then_some(""))
        .unwrap_or(s);
    s.trim_end_matches('/').to_string()
}

/// Shell expression for a path under the remote root (`$HOME/…` or absolute).
pub fn remote_path_shell(normalized_root_or_dir: &str) -> String {
    if normalized_root_or_dir.starts_with('/') {
        format!("\"{}\"", normalized_root_or_dir.replace('\"', "\\\""))
    } else {
        format!("\"$HOME/{normalized_root_or_dir}\"")
    }
}

// --- job spec -----------------------------------------------------------------

pub struct SlurmJobSpec {
    /// ssh config host alias of the cluster's login node.
    pub host: String,
    /// Names the remote run dir `<remoteRoot>/runs/<run_id>`.
    pub run_id: String,
    /// Remote root as configured (tilde form ok; normalized at use sites).
    pub remote_root: String,
    /// Repo-relative path to the agent-authored batch script inside the
    /// staged snapshot. Default `job.sbatch`.
    pub sbatch_path: String,
}

/// Map a `--flavor` string onto a `--gres` request. Kept for CLI/settings
/// compatibility; submit no longer injects GRES into a generated script.
///   "gpu"      -> gpu:1
///   "gpu:…"    -> passed through verbatim (already a GRES spec)
///   "h100:2"   -> gpu:h100:2
///   "h100"     -> gpu:h100
pub fn resolve_gres(flavor: &str) -> Option<String> {
    let f = flavor.trim();
    if f.is_empty() {
        return None;
    }
    if f == "gpu" {
        return Some("gpu:1".to_string());
    }
    if f.starts_with("gpu:") {
        return Some(f.to_string());
    }
    Some(format!("gpu:{f}"))
}

/// Seconds → Slurm's `--time` syntax (`D-HH:MM:SS` / `HH:MM:SS`).
/// Kept for callers that still want to format a hint; not used at submit.
pub fn slurm_time(secs: u64) -> String {
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    if days > 0 {
        format!("{days}-{h:02}:{m:02}:{s:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}")
    }
}

/// `sbatch --parsable` prints `<jobid>` or `<jobid>;<cluster>`.
fn parse_job_id(out: &str) -> Result<String> {
    let id = out.trim().split(';').next().unwrap_or("").trim();
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
        return Err(anyhow!("Unexpected sbatch output: {:?}", out.trim()));
    }
    Ok(id.to_string())
}

// --- lifecycle ----------------------------------------------------------------

/// The remote run dir for a run, relative to `$HOME` when `remote_root` is
/// home-relative, or absolute when `remote_root` is absolute.
pub fn run_dir(remote_root: &str, run_id: &str) -> String {
    let root = normalize_remote_root(remote_root);
    format!("{root}/runs/{run_id}")
}

/// Convenience: run dir from current settings (or the default remote root).
pub fn run_dir_from_settings(run_id: &str) -> Result<String> {
    let settings = load_settings()?.unwrap_or_default();
    Ok(run_dir(&effective_remote_root(&settings), run_id))
}

/// Submit the job: require an agent-authored `job.sbatch` in the already-staged
/// repo, then `sbatch --parsable` from the run directory. Returns the Slurm
/// job id — the reattach handle (together with the host).
pub async fn run_job(spec: &SlurmJobSpec) -> Result<String> {
    let dir = run_dir(&spec.remote_root, &spec.run_id);
    let dir_shell = remote_path_shell(&dir);
    let sbatch_rel = spec.sbatch_path.trim().trim_start_matches('/');
    let sbatch_rel = if sbatch_rel.is_empty() {
        "job.sbatch"
    } else {
        sbatch_rel
    };
    // Staged snapshot lands in repo/; the agent-authored script lives there.
    let repo_sbatch = format!("repo/{sbatch_rel}");

    let check = ssh_run(
        &SshTarget::alias(&spec.host),
        &format!(
            "d={dir_shell}; \
             if [ ! -d \"$d/repo\" ]; then echo MISSING_REPO; \
             elif [ ! -f \"$d/{repo_sbatch}\" ]; then echo MISSING_SBATCH; \
             else echo OK; fi"
        ),
        None,
    )
    .await
    .map_err(|e| anyhow!("Could not check the staged run dir: {e}"))?;

    match check.trim() {
        "OK" => {}
        "MISSING_REPO" => {
            return Err(anyhow!(
                "Staged snapshot missing at {dir}/repo — stage the source before submit."
            ));
        }
        _ => {
            return Err(anyhow!(
                "No {sbatch_rel} in the staged experiment snapshot. \
                 Add an agent-authored job.sbatch at the repo root (with \
                 #SBATCH --output=log / --error=log and write exit_code in the \
                 run directory), then relaunch."
            ));
        }
    }

    // Submit from the run dir so relative --output/--error/exit_code land there.
    let out = ssh_run(
        &SshTarget::alias(&spec.host),
        &format!("cd {dir_shell} && sbatch --parsable {repo_sbatch}"),
        None,
    )
    .await
    .map_err(|e| anyhow!("sbatch failed: {e}"))?;
    parse_job_id(&out)
}

/// Job state in the shared stage vocabulary (see `jobs::stage_to_run_status`).
#[derive(Debug, Clone)]
pub struct JobState {
    pub stage: String,
    pub message: Option<String>,
    /// The payload's exit code, read from the run dir's `exit_code` file.
    pub exit_code: Option<i64>,
}

/// One combined remote probe emitting a single token — exit_code first
/// (ground truth), then live queue state, then accounting for jobs that left
/// the queue without one. POSIX-sh only: the command runs under the remote
/// user's login shell. `sacct -P` (parsable) because the default fixed-width
/// State column truncates ("CANCELLED by 1234" prints as "CANCELLED+").
///
/// `GONE` is NOT terminal by itself: it also fires when slurmctld is briefly
/// down or the exit_code write is NFS-lagged — the supervisor debounces it
/// over several polls before declaring the job lost.
pub async fn inspect_job(host: &str, run_id: &str, job_id: &str) -> Result<JobState> {
    let dir = run_dir_from_settings(run_id)?;
    let dir_shell = remote_path_shell(&dir);
    let cmd = format!(
        "d={dir_shell}; \
         if [ -f \"$d/exit_code\" ]; then echo \"EXIT $(cat \"$d/exit_code\")\"; \
         else st=$(squeue -h -j {job_id} -o %T 2>/dev/null | head -n1); \
           if [ -n \"$st\" ]; then echo \"SQ $st\"; \
           else st=$(sacct -nPX -j {job_id} -o State 2>/dev/null | head -n1); \
             if [ -n \"$st\" ]; then echo \"SA $st\"; else echo GONE; fi; \
           fi; \
         fi",
    );
    let out = ssh_run(&SshTarget::alias(host), &cmd, None).await?;
    Ok(map_inspect_token(out.trim()))
}

/// Pure token → stage mapping (unit-tested; Slurm state names are stable).
/// Emits the internal `GONE` stage for a job the scheduler no longer knows —
/// the caller debounces it (see `inspect_job`).
fn map_inspect_token(out: &str) -> JobState {
    let state = |stage: &str, message: Option<String>| JobState {
        stage: stage.to_string(),
        message,
        exit_code: None,
    };
    if let Some(code) = out.strip_prefix("EXIT ") {
        // An empty file is the window between open(O_TRUNC) and the write —
        // not a verdict; poll again.
        if code.trim().is_empty() {
            return state("RUNNING", None);
        }
        let parsed: Option<i64> = code.trim().parse().ok();
        let code = parsed.unwrap_or(-1);
        let verdict = if code == 0 {
            state("COMPLETED", None)
        } else {
            state("ERROR", Some(format!("exited with code {code}")))
        };
        return JobState {
            exit_code: parsed,
            ..verdict
        };
    }
    // `squeue %T` / `sacct -P State` values. sacct suffixes cancellations
    // ("CANCELLED by 1234"), so match on the first word; the trailing-`+`
    // trim is belt-and-braces against fixed-width truncation.
    let (source, raw) = match (out.strip_prefix("SQ "), out.strip_prefix("SA ")) {
        (Some(s), _) => ("squeue", s),
        (_, Some(s)) => ("sacct", s),
        _ if out == "GONE" => return state("GONE", None),
        _ => return state("RUNNING", Some(format!("unexpected inspect output: {out}"))),
    };
    let word = raw
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_end_matches('+');
    match word {
        "PENDING" | "CONFIGURING" | "REQUEUED" | "RESV_DEL_HOLD" => state("SCHEDULING", None),
        "RUNNING" | "COMPLETING" | "SUSPENDED" | "STAGE_OUT" | "SIGNALING" => {
            state("RUNNING", None)
        }
        // Terminal per the scheduler but no exit_code file yet (NFS lag):
        // the agent's job.sbatch should exit with the payload's code, so
        // sacct's verdict mirrors the payload — trust it. From squeue it's
        // transient; keep polling.
        "COMPLETED" if source == "sacct" => state("COMPLETED", None),
        "COMPLETED" => state("RUNNING", None),
        // Also transient from squeue: default JobRequeue=1 (and
        // PreemptMode=REQUEUE) re-queues these under the same job id — a poll
        // landing in the window must not finalize a run that's about to
        // re-run. If the job is NOT requeued it leaves the queue and the
        // sacct/GONE path delivers the terminal verdict.
        "NODE_FAIL" | "PREEMPTED" if source == "squeue" => state("RUNNING", None),
        "CANCELLED" | "REVOKED" => state("CANCELED", None),
        "TIMEOUT" | "DEADLINE" => state("ERROR", Some("job hit its time limit".into())),
        "FAILED" | "NODE_FAIL" | "BOOT_FAIL" | "OUT_OF_MEMORY" | "PREEMPTED" => state(
            "ERROR",
            Some(format!("slurm reported {}", word.to_ascii_lowercase())),
        ),
        other => state(
            "RUNNING",
            Some(format!("unrecognized slurm state: {other}")),
        ),
    }
}

/// Cancel = `scancel`. Tolerant of already-finished jobs (scancel exits
/// non-zero for them); the supervisor's next poll observes the outcome.
pub async fn cancel_job(host: &str, job_id: &str) -> Result<()> {
    ssh_run(
        &SshTarget::alias(host),
        &format!("scancel {job_id} 2>/dev/null || true"),
        None,
    )
    .await?;
    Ok(())
}

// Log streaming reuses `ssh::stream_logs` directly — the run-dir/`log` layout
// is identical, and Slurm appends to the same file via `--output`.

// --- preflight ----------------------------------------------------------------

/// Per-host readiness for the Settings UI: reachable, Slurm CLI + snapshot tools
/// present, and which partitions exist.
pub struct SlurmPreflight {
    pub reachable: bool,
    pub slurm_found: bool,
    pub tools_found: bool,
    /// From `sinfo` (default partition's trailing `*` stripped).
    pub partitions: Vec<String>,
    pub error: Option<String>,
}

pub async fn preflight(host: &str) -> SlurmPreflight {
    let cmd = "if command -v sbatch >/dev/null 2>&1 && command -v squeue >/dev/null 2>&1 \
               && command -v scancel >/dev/null 2>&1; then echo SLURM_OK; fi; \
               if command -v bash >/dev/null 2>&1 && command -v tar >/dev/null 2>&1; \
               then echo TOOLS_OK; fi; \
               sinfo -h -o %P 2>/dev/null || true";
    match ssh_run(&SshTarget::alias(host), cmd, None).await {
        Ok(out) => {
            let mut slurm_found = false;
            let mut tools_found = false;
            let mut partitions = Vec::new();
            for line in out.lines().map(str::trim).filter(|l| !l.is_empty()) {
                match line {
                    "SLURM_OK" => slurm_found = true,
                    "TOOLS_OK" => tools_found = true,
                    p => {
                        let p = p.trim_end_matches('*').to_string();
                        if !p.is_empty() && !partitions.contains(&p) {
                            partitions.push(p);
                        }
                    }
                }
            }
            SlurmPreflight {
                reachable: true,
                slurm_found,
                tools_found,
                partitions,
                error: None,
            }
        }
        Err(e) => SlurmPreflight {
            reachable: false,
            slurm_found: false,
            tools_found: false,
            partitions: Vec::new(),
            error: Some(e.to_string()),
        },
    }
}

// --- tests ----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_remote_root_strips_tilde_and_home() {
        assert_eq!(normalize_remote_root("~/scratch/.orx"), "scratch/.orx");
        assert_eq!(normalize_remote_root("$HOME/scratch/.orx"), "scratch/.orx");
        assert_eq!(normalize_remote_root("scratch/.orx/"), "scratch/.orx");
        assert_eq!(
            normalize_remote_root("/mnt/scratch/.orx"),
            "/mnt/scratch/.orx"
        );
        assert_eq!(normalize_remote_root(DEFAULT_REMOTE_ROOT), "scratch/.orx");
    }

    #[test]
    fn run_dir_uses_remote_root() {
        assert_eq!(run_dir("~/scratch/.orx", "abc"), "scratch/.orx/runs/abc");
        assert_eq!(
            run_dir("/mnt/scratch/.orx", "abc"),
            "/mnt/scratch/.orx/runs/abc"
        );
        assert_eq!(run_dir(".orx", "abc"), ".orx/runs/abc");
    }

    #[test]
    fn remote_path_shell_quotes_home_or_absolute() {
        assert_eq!(
            remote_path_shell("scratch/.orx/runs/r1"),
            "\"$HOME/scratch/.orx/runs/r1\""
        );
        assert_eq!(
            remote_path_shell("/mnt/scratch/.orx/runs/r1"),
            "\"/mnt/scratch/.orx/runs/r1\""
        );
    }

    #[test]
    fn effective_remote_root_defaults() {
        let empty = SlurmSettings::default();
        assert_eq!(effective_remote_root(&empty), DEFAULT_REMOTE_ROOT);
        let set = SlurmSettings {
            remote_root: Some("/data/.orx".into()),
            ..Default::default()
        };
        assert_eq!(effective_remote_root(&set), "/data/.orx");
    }

    #[test]
    fn slurm_time_formats() {
        assert_eq!(slurm_time(90), "00:01:30");
        assert_eq!(slurm_time(4 * 3600), "04:00:00");
        assert_eq!(slurm_time(86_400 + 3661), "1-01:01:01");
    }

    #[test]
    fn gres_from_flavor() {
        assert_eq!(resolve_gres(""), None);
        assert_eq!(resolve_gres("gpu").as_deref(), Some("gpu:1"));
        assert_eq!(resolve_gres("gpu:a100:4").as_deref(), Some("gpu:a100:4"));
        assert_eq!(resolve_gres("h100:2").as_deref(), Some("gpu:h100:2"));
        assert_eq!(resolve_gres("h100").as_deref(), Some("gpu:h100"));
    }

    #[test]
    fn job_id_parsing() {
        assert_eq!(parse_job_id("123\n").unwrap(), "123");
        assert_eq!(parse_job_id("123;cluster2\n").unwrap(), "123");
        assert!(parse_job_id("sbatch: error").is_err());
        assert!(parse_job_id("").is_err());
    }

    #[test]
    fn inspect_token_mapping() {
        assert_eq!(map_inspect_token("EXIT 0").stage, "COMPLETED");
        assert_eq!(map_inspect_token("EXIT 0").exit_code, Some(0));
        let failed = map_inspect_token("EXIT 137");
        assert_eq!(failed.stage, "ERROR");
        assert_eq!(failed.exit_code, Some(137));
        assert!(failed.message.unwrap().contains("137"));
        assert_eq!(map_inspect_token("EXIT garbage").exit_code, None);
        assert_eq!(map_inspect_token("SQ RUNNING").exit_code, None);
        // Empty exit_code = caught mid-write; not a verdict.
        assert_eq!(map_inspect_token("EXIT ").stage, "RUNNING");
        assert_eq!(map_inspect_token("SQ PENDING").stage, "SCHEDULING");
        assert_eq!(map_inspect_token("SQ RUNNING").stage, "RUNNING");
        assert_eq!(map_inspect_token("SQ COMPLETING").stage, "RUNNING");
        // squeue COMPLETED without exit_code: transient, keep polling.
        assert_eq!(map_inspect_token("SQ COMPLETED").stage, "RUNNING");
        // sacct COMPLETED means the script (= payload) exited 0.
        assert_eq!(map_inspect_token("SA COMPLETED").stage, "COMPLETED");
        assert_eq!(map_inspect_token("SA CANCELLED by 1234").stage, "CANCELED");
        // Fixed-width sacct truncates with a trailing '+'; -P avoids it, but
        // the parser must survive it anyway.
        assert_eq!(map_inspect_token("SA CANCELLED+").stage, "CANCELED");
        assert_eq!(map_inspect_token("SA TIMEOUT").stage, "ERROR");
        assert_eq!(map_inspect_token("SA NODE_FAIL").stage, "ERROR");
        // From squeue these are requeue-transient (JobRequeue=1); terminal
        // only once sacct (or GONE) confirms the job really left.
        assert_eq!(map_inspect_token("SQ NODE_FAIL").stage, "RUNNING");
        assert_eq!(map_inspect_token("SQ PREEMPTED").stage, "RUNNING");
        // GONE is not terminal here — the supervisor debounces it.
        assert_eq!(map_inspect_token("GONE").stage, "GONE");
        // Unknown states never wedge the supervisor into a terminal state.
        assert_eq!(map_inspect_token("SQ SOMETHING_NEW").stage, "RUNNING");
    }

    /// Live E2E against a real cluster — opt-in, never runs in CI:
    ///   ORX_SLURM_TEST_HOST=<ssh alias> cargo test jobs::slurm -- --ignored
    #[tokio::test]
    #[ignore = "needs a live slurm cluster; set ORX_SLURM_TEST_HOST"]
    async fn e2e_lifecycle_against_live_cluster() {
        let Ok(host) = std::env::var("ORX_SLURM_TEST_HOST") else {
            panic!("set ORX_SLURM_TEST_HOST to an ~/.ssh/config alias of a slurm login node");
        };
        let settings = load_settings().ok().flatten().unwrap_or_default();
        let remote_root = effective_remote_root(&settings);
        let mk = |run_id: &str| SlurmJobSpec {
            host: host.clone(),
            run_id: run_id.into(),
            remote_root: remote_root.clone(),
            sbatch_path: "job.sbatch".into(),
        };
        let poll = |run_id: String, job_id: String, until: &'static [&'static str]| {
            let host = host.clone();
            async move {
                for _ in 0..150 {
                    let s = inspect_job(&host, &run_id, &job_id).await.unwrap();
                    if until.contains(&s.stage.as_str()) {
                        return s;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
                panic!("job {job_id} never reached {until:?}");
            }
        };

        let run_a = format!("e2e-a-{}", uuid::Uuid::new_v4());
        let run_b = format!("e2e-b-{}", uuid::Uuid::new_v4());
        let dir_a = run_dir(&remote_root, &run_a);
        let dir_b = run_dir(&remote_root, &run_b);
        let dir_a_shell = remote_path_shell(&dir_a);
        let dir_b_shell = remote_path_shell(&dir_b);
        let cleanup = format!("rm -rf {dir_a_shell} {dir_b_shell}");

        let sbatch_body = "#!/usr/bin/env bash\n#SBATCH --job-name=orx-e2e\n#SBATCH --output=log\n#SBATCH --error=log\n#SBATCH --open-mode=append\n(\ncd repo || exit 97\necho hello-from-slurm\n)\ncode=$?\necho \"$code\" > exit_code\nexit \"$code\"\n";
        let stage = |run_id: &str, body: &str| {
            let host = host.clone();
            let remote_root = remote_root.clone();
            let dir = run_dir(&remote_root, run_id);
            let dir_shell = remote_path_shell(&dir);
            let quoted = body.replace('\'', "'\\''");
            let script = format!(
                "umask 077; mkdir -p {dir_shell}/repo; \
                 printf '%s' '{quoted}' > {dir_shell}/repo/job.sbatch; \
                 chmod +x {dir_shell}/repo/job.sbatch"
            );
            async move {
                ssh_run(&SshTarget::alias(&host), &script, None)
                    .await
                    .unwrap();
            }
        };

        stage(&run_a, sbatch_body).await;
        let job_a = run_job(&mk(&run_a)).await.unwrap();
        let done = poll(run_a.clone(), job_a, &["COMPLETED", "ERROR"]).await;
        assert_eq!(done.stage, "COMPLETED", "message: {:?}", done.message);
        let mut lines = Vec::new();
        crate::jobs::ssh::stream_logs(
            &SshTarget::alias(&host),
            &dir_a,
            0,
            std::time::Duration::from_secs(5),
            &mut |l: &str| lines.push(l.to_string()),
        )
        .await
        .unwrap();
        assert!(lines.iter().any(|l| l == "hello-from-slurm"), "{lines:?}");

        let sleep_sbatch = "#!/usr/bin/env bash\n#SBATCH --job-name=orx-e2e-sleep\n#SBATCH --output=log\n#SBATCH --error=log\nsleep 300\necho 0 > exit_code\n";
        stage(&run_b, sleep_sbatch).await;
        let job_b = run_job(&mk(&run_b)).await.unwrap();
        poll(run_b.clone(), job_b.clone(), &["RUNNING", "SCHEDULING"]).await;
        cancel_job(&host, &job_b).await.unwrap();
        let after = poll(run_b, job_b, &["CANCELED", "GONE", "ERROR"]).await;
        let _ = ssh_run(&SshTarget::alias(&host), &cleanup, None).await;
        assert!(
            after.stage == "CANCELED" || after.stage == "GONE",
            "unexpected post-cancel stage: {after:?}"
        );
    }
}
