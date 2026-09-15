# Slurm (`--backend slurm`)

Use this backend only when the user explicitly requests their Slurm cluster or
it is the configured default. `orx` reaches the login node over SSH, stages the
committed snapshot under the configured remote root, and submits the
**agent-authored** `job.sbatch` with `sbatch`.

```sh
orx exp run <expId> --backend slurm --host login-node
orx exp run <expId> --backend slurm
```

## Agent `job.sbatch` contract

Put `job.sbatch` at the experiment repo root (it is part of the committed
snapshot). orx does **not** generate this file from the run command or from
flavor/partition/timeout settings.

Required:

- `#SBATCH --output=log` and `#SBATCH --error=log` (relative to the run
  directory — the cwd when `sbatch` runs)
- Write `exit_code` in the **run directory** when the payload finishes, e.g.:

```bash
#!/usr/bin/env bash
#SBATCH --job-name=my-exp
#SBATCH --output=log
#SBATCH --error=log
#SBATCH --partition=gpu
#SBATCH --gres=gpu:1
#SBATCH --time=04:00:00
(
  cd repo || exit 97
  # modules, conda, training command, …
  python train.py
)
code=$?
echo "$code" > exit_code
exit "$code"
```

A clear error is raised at submit if `job.sbatch` is missing from the staged
snapshot.

## Host and remote root

- `--host` is an alias from `~/.ssh/config`; omit it only when a Slurm default
  host is configured in `slurm.json`.
- Remote files live under `remoteRoot` from `slurm.json` (default
  `~/scratch/.orx`): `source/` for snapshot tarballs and `runs/<runId>/` for
  `repo/`, `log`, and `exit_code`. Override with `"remoteRoot": "/path/.orx"`.
- `--flavor`, `--timeout`, and settings `partition` / `account` / `timeLimit`
  may still appear in the CLI or Settings UI but are **not required** for
  submit and are not injected into the batch script — put those directives in
  `job.sbatch`.
- There is no image flag. The cluster environment—modules, conda, and login
  profile—is used as-is.
- A detached `orx supervise` process records scheduler status and logs; do not
  kill it. Cancel/log flow is unchanged (`scancel`, tail of `log`).
