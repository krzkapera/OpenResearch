//! Local Slurm launch — the scheduler-backed twin of `local/ssh.rs`: submit
//! the experiment as a batch job on a Slurm cluster reached via its login
//! node. `--host` names an `~/.ssh/config` alias (defaultable in the slurm
//! settings). The agent authors `job.sbatch` in the experiment snapshot; orx
//! stages it and submits with `sbatch`. The run row lives in the local store
//! only; a detached `orx supervise` watches the job.

use crate::commands::exp::spawn_detached_supervise;
use crate::compute::SourceSnapshot;
use crate::error::{anyhow, Result};
use crate::jobs::{slurm, BackendDescriptor};
use crate::store::{now_ms, Store, StoredRun};

/// CLI wrapper around `submit_local_slurm`: submit, then print the summary.
pub async fn launch_local_slurm(args: &crate::ExpRunArgs) -> Result<()> {
    let run = submit_local_slurm(args).await?;
    let backend = BackendDescriptor::parse(&run.backend_json)?;
    println!("\u{2713} Slurm job submitted.");
    println!(
        "  host {}  (job {})",
        backend.namespace.as_deref().unwrap_or(""),
        backend.job_id.as_deref().unwrap_or("")
    );
    println!("  run  {}", run.id);
    println!(
        "{}",
        crate::invocation::follow_up(&run.experiment_id, &run.id)
    );
    Ok(())
}

/// Submit the local experiment's run as a Slurm batch job and detach a
/// supervisor. Requires `--backend slurm`; the login node comes from
/// `--host <alias>` or the slurm settings default.
pub async fn submit_local_slurm(args: &crate::ExpRunArgs) -> Result<StoredRun> {
    crate::compute::submit(args).await
}

pub async fn submit_local_slurm_with_source(
    args: &crate::ExpRunArgs,
    source: SourceSnapshot,
    run_id: String,
) -> Result<StoredRun> {
    if args.image.is_some() {
        return Err(anyhow!(
            "--image doesn't apply to --backend slurm — the job runs in your cluster \
             environment (modules/conda), not a container."
        ));
    }
    if args.manifest.is_some() {
        return Err(anyhow!("--manifest only applies with --backend k8s."));
    }
    // A muscle-memory `--flavor cpu` (HF/Modal habit) would become the
    // nonsense GRES `gpu:cpu` if ever applied. Flavor is optional and not
    // required for submit (the agent's job.sbatch owns resource requests).
    if let Some(f) = &args.flavor {
        if f.trim().to_ascii_lowercase().starts_with("cpu") {
            return Err(anyhow!(
                "--flavor names GPUs on --backend slurm (e.g. h100:2). For a CPU-only \
                 run just omit --flavor; put CPU requests in job.sbatch instead."
            ));
        }
    }

    let settings = slurm::load_settings()?.unwrap_or_default();
    let host = args
        .host
        .clone()
        .or_else(|| settings.host.clone())
        .ok_or_else(|| {
            anyhow!(
                "--backend slurm needs a login node: pass --host <alias> (an ~/.ssh/config \
                 alias) or use a configured default Slurm host."
            )
        })?;
    let remote_root = slurm::effective_remote_root(&settings);
    let remote_root_norm = slurm::normalize_remote_root(&remote_root);

    let store = Store::open()?;
    let exp = store
        .get_local_experiment(&args.exp_id)?
        .ok_or_else(|| anyhow!("Local experiment {} not found.", args.exp_id))?;
    let project = store
        .get_local_project(&exp.project_id)?
        .ok_or_else(|| anyhow!("Local project {} not found.", exp.project_id))?;
    if let Some(w) = crate::local::experiments::legacy_root_warning(&project, &exp) {
        eprintln!("{w}");
    }

    // The experiment run_command is informational only for Slurm now — the
    // agent-authored job.sbatch is the source of truth for what runs.
    let command_label = Some(exp.run_command.clone())
        .filter(|c| !c.trim().is_empty())
        .or_else(|| project.run_command.clone().filter(|c| !c.trim().is_empty()))
        .unwrap_or_else(|| "job.sbatch".to_string());

    crate::jobs::ssh::stage_source_at(
        &crate::jobs::ssh::SshTarget::alias(&host),
        &run_id,
        &source.path,
        &source.digest,
        &remote_root_norm,
    )
    .await?;
    let job_id = slurm::run_job(&slurm::SlurmJobSpec {
        host: host.clone(),
        run_id: run_id.clone(),
        remote_root: remote_root.clone(),
        sbatch_path: "job.sbatch".to_string(),
    })
    .await?;

    let mut descriptor = BackendDescriptor {
        kind: "slurm_job".to_string(),
        namespace: Some(host.clone()),
        job_id: Some(job_id.clone()),
        flavor: args.flavor.clone(),
        image: None,
        url: None,
        context: None,
        manifest: None,
        resources: None,
        ssh_host: None,
        ssh_port: None,
        ssh_user: None,
        timeout_secs: None,
        source_digest: None,
        source_path: None,
        source_size: None,
    };
    source.apply_to_descriptor(&mut descriptor);
    if let Err(error) = crate::compute::record_submission_handle(&run_id, &descriptor) {
        let _ = slurm::cancel_job(&host, &job_id).await;
        return Err(error);
    }
    let run = StoredRun {
        id: run_id.clone(),
        experiment_id: exp.id.clone(),
        project_id: project.id.clone(),
        status: "starting".to_string(),
        backend_json: descriptor.to_json(),
        command: command_label,
        created_at: now_ms(),
        updated_at: now_ms(),
        ended_at: None,
        exit_code: None,
        commit_sha: Some(source.revision),
        result_markdown: None,
        cancel_requested: store
            .get_run(&run_id)?
            .is_some_and(|run| run.cancel_requested),
        chat_session_id: args.launching_chat_session(),
    };
    store.upsert_run(&run)?;

    spawn_detached_supervise(&run_id)?;
    Ok(run)
}
