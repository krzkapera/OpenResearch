//! The `agent` command group: delegate work to a second agent session.
//!
//!   orx agent spawn "<task>"        start a helper agent on its own top-level session
//!   orx agent kill <session-id>     delete a finished helper session
//!
//! Only meaningful inside a local `orx up` agent session. `ORX_LOCAL_SESSION`
//! marks the process as one; `ORX_CHAT_SESSION_ID` names the session doing the
//! spawning (see `local::chat::set_chat_session_env`). Both are needed — the
//! cloud opencode plugin exports the session id too, for run attribution.
//!
//! `spawn` only writes the child's session row and a `chat_spawns` record; it
//! never runs the child itself. The resident `orx up` picks the record up,
//! starts the helper's first turn, and (unless `--no-wake`) wakes the parent
//! when the helper is done. Same store-and-watcher split as `orx exp wake`, and for
//! the same reason: the CLI is a short-lived subprocess with no harness of its
//! own to run a turn on.
//!
//! `kill` is different: nothing about it can be a plain store write, because
//! reaping a still-live harness process means calling into the resident
//! `orx up`'s in-memory `ChatHost`, which only that process holds a handle to.
//! It refuses to delete the calling session itself — cleanup is an ancestor's
//! job, done once the helper has already reported back.

use std::io::Read;

use crate::error::{anyhow, Result};
use crate::local::harness::PermissionMode;
use crate::store::{now_ms, ChatSpawn, Store, StoredChatSession};

use crate::AgentCommand;

/// Helpers one session may have in flight at once.
pub(crate) const MAX_LIVE_SPAWNS: i64 = 5;

pub async fn run(args: crate::AgentArgs) -> Result<()> {
    let store = Store::open()?;
    match args.command {
        AgentCommand::Spawn {
            task,
            stdin,
            title,
            harness,
            model,
            permission_mode,
            reasoning_level,
            service_tier,
            no_wake,
        } => spawn(
            &store,
            task,
            stdin,
            title,
            harness,
            model,
            permission_mode,
            reasoning_level,
            service_tier,
            !no_wake,
        ),
        AgentCommand::Kill { session_id } => kill(session_id).await,
    }
}

/// Read the task from the positional argument or, with `--stdin`, from the
/// whole of stdin (agents write multi-paragraph briefs as heredocs).
fn task_text(task: Option<String>, stdin: bool) -> Result<String> {
    if stdin {
        if task.is_some() {
            return Err(anyhow!(
                "Pass the task as an argument or --stdin, not both."
            ));
        }
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| anyhow!("Could not read the task from stdin: {e}"))?;
        return non_empty(buf);
    }
    non_empty(task.unwrap_or_default())
}

fn non_empty(text: String) -> Result<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(anyhow!(
            "Describe the task for the spawned agent: `orx agent spawn \"<task>\"`."
        ));
    }
    Ok(trimmed.to_string())
}

/// Maximum spawn-chain depth: a root (never-spawned) session is depth 0. A
/// session may spawn a helper only while its own depth is below this, so a
/// chain can never grow past `MAX_SPAWN_DEPTH` levels below its root.
pub(crate) const MAX_SPAWN_DEPTH: u32 = 4;

/// How many spawn-ancestors `session` has, walking `parent_session_id` back
/// towards a root. Stops as soon as it reaches `MAX_SPAWN_DEPTH`, since a
/// caller past that point only needs "too deep", not the exact count.
fn spawn_depth(store: &Store, session: &StoredChatSession) -> Result<u32> {
    let mut depth = 0;
    let mut current = session.parent_session_id.clone();
    while let Some(id) = current {
        depth += 1;
        if depth >= MAX_SPAWN_DEPTH {
            return Ok(depth);
        }
        current = store
            .get_chat_session(&id)?
            .and_then(|ancestor| ancestor.parent_session_id);
    }
    Ok(depth)
}

/// Why this session may not spawn right now, if it may not. Depth and breadth
/// are the two ways one request becomes an unbounded tree of paid sessions:
/// depth is capped at `MAX_SPAWN_DEPTH`, breadth at `MAX_LIVE_SPAWNS`.
fn spawn_refusal(depth: u32, live: i64) -> Option<String> {
    if depth >= MAX_SPAWN_DEPTH {
        return Some(format!(
            "This session is already {depth} spawn levels deep, the most a chain may go. Do the \
             task here, or report back so an ancestor session can delegate it."
        ));
    }
    (live >= MAX_LIVE_SPAWNS).then(|| {
        format!(
            "You already have {live} agents in flight, the most one session may run at once. \
             Wait for one to report back before spawning another."
        )
    })
}

fn spawn(
    store: &Store,
    task: Option<String>,
    stdin: bool,
    title: Option<String>,
    harness: Option<String>,
    model: Option<String>,
    permission_mode: Option<String>,
    reasoning_level: Option<String>,
    service_tier: Option<String>,
    wake_parent: bool,
) -> Result<()> {
    if !crate::local::chat::in_local_session() {
        return Err(anyhow!(
            "`orx agent spawn` is only available inside a local `orx up` agent session."
        ));
    }
    let parent_id = crate::local::chat::launching_chat_session()
        .ok_or_else(|| anyhow!("This agent session has no chat id to spawn from."))?;
    let parent = store
        .get_chat_session(&parent_id)?
        .ok_or_else(|| anyhow!("The current chat session no longer exists."))?;
    let prompt = task_text(task, stdin)?;
    let depth = spawn_depth(store, &parent)?;
    if let Some(refusal) = spawn_refusal(depth, store.count_live_chat_spawns(&parent_id)?) {
        return Err(anyhow!(refusal));
    }
    let harness = harness.unwrap_or_else(|| parent.harness.clone());
    if !crate::local::harness::is_chat_harness(&harness) {
        return Err(anyhow!("unknown harness: {harness}"));
    }
    let nonempty = |value: Option<String>| value.filter(|item| !item.trim().is_empty());
    let permission_mode = nonempty(permission_mode);
    let reasoning_level = nonempty(reasoning_level);
    let service_tier = nonempty(service_tier);
    if permission_mode
        .as_deref()
        .is_some_and(|mode| crate::local::harness::permission_mode_for(&harness, mode).is_none())
    {
        return Err(anyhow!("invalid permission mode for selected harness"));
    }
    if service_tier
        .as_deref()
        .is_some_and(|tier| crate::local::harness::service_tier_for(&harness, tier).is_none())
    {
        return Err(anyhow!("invalid service tier for selected harness"));
    }
    // Settings only carry over when the child runs the same harness; a model or
    // permission-mode id from one CLI is meaningless to another. An explicit
    // flag always wins over both the parent and the harness default.
    let inherits = harness == parent.harness;
    let changes_model = model
        .as_deref()
        .is_some_and(|model| parent.model.as_deref() != Some(model));
    // Claude activates Plan through its permission mode, not the plan axis, so
    // clearing `plan_mode` alone would still hand a planning parent's helper a
    // mode that only ever produces a plan.
    let plan_permission =
        crate::local::harness::permission_id_for_mode(&harness, PermissionMode::Plan);
    let title = title
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    let session = StoredChatSession {
        id: format!("chat_{}", uuid::Uuid::new_v4()),
        project_id: parent.project_id.clone(),
        harness,
        native_session_id: None,
        // "user" = explicitly chosen, so auto-titling leaves it alone.
        title_source: title.is_some().then(|| "user".to_string()),
        title,
        model: model.or_else(|| inherits.then(|| parent.model.clone()).flatten()),
        service_tier: service_tier.or_else(|| {
            (inherits && !changes_model)
                .then(|| parent.service_tier.clone())
                .flatten()
        }),
        permission_mode: permission_mode.or_else(|| {
            inherits
                .then(|| parent.permission_mode.clone())
                .flatten()
                .filter(|mode| Some(mode) != plan_permission.as_ref())
        }),
        plan_mode: false,
        plan_reset_pending: false,
        reasoning_level: reasoning_level
            .or_else(|| inherits.then(|| parent.reasoning_level.clone()).flatten()),
        archived: false,
        context_usage_json: None,
        bootstrap_context: None,
        active_leaf_id: None,
        parent_session_id: Some(parent_id.clone()),
        auto_resume: false,
        created_at: now_ms(),
        updated_at: now_ms(),
    };
    // One transaction: a session row without its spawn row is an empty session
    // in the user's sidebar that nothing will ever start.
    let tx = store.begin()?;
    store.create_chat_session(&session)?;
    store.create_chat_spawn(&ChatSpawn {
        session_id: session.id.clone(),
        parent_session_id: parent_id,
        prompt,
        wake_parent,
        attempts: 0,
        finished_at: None,
    })?;
    tx.commit()?;
    println!("Spawned agent session {}.", session.id);
    println!("It starts within a few seconds and works in its own git worktree.");
    if wake_parent {
        println!("This chat will be resumed with its result when it finishes.");
    } else {
        println!("You will NOT be told when it finishes; check its session yourself.");
    }
    Ok(())
}

/// Delete a finished session. Refuses to delete the caller's own session —
/// only an ancestor may clean up a helper, and only once it is done; deleting
/// a still-live session out from under itself would kill its own turn.
async fn kill(session_id: String) -> Result<()> {
    if !crate::local::chat::in_local_session() {
        return Err(anyhow!(
            "`orx agent kill` is only available inside a local `orx up` agent session."
        ));
    }
    let caller_id = crate::local::chat::launching_chat_session()
        .ok_or_else(|| anyhow!("This agent session has no chat id to kill from."))?;
    if session_id == caller_id {
        return Err(anyhow!(
            "A session may not delete itself. Let the session that spawned you (or another \
             ancestor) delete you once you have reported back."
        ));
    }
    let port = crate::local::chat::trusted_up_port()?.ok_or_else(|| {
        anyhow!("This agent session lost its link to the orx up server; restart `orx up`.")
    })?;
    crate::commands::up::delete_chat_session_via_up(port, &session_id).await?;
    println!("Deleted session {session_id}.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{spawn_depth, spawn_refusal, task_text, MAX_LIVE_SPAWNS, MAX_SPAWN_DEPTH};
    use crate::store::{Store, StoredChatSession};

    fn session(id: &str, parent_session_id: Option<&str>) -> StoredChatSession {
        StoredChatSession {
            id: id.into(),
            project_id: "p1".into(),
            harness: "codex".into(),
            native_session_id: None,
            title: None,
            title_source: None,
            model: None,
            service_tier: None,
            permission_mode: None,
            plan_mode: false,
            plan_reset_pending: false,
            reasoning_level: None,
            archived: false,
            context_usage_json: None,
            bootstrap_context: None,
            active_leaf_id: None,
            parent_session_id: parent_session_id.map(str::to_string),
            auto_resume: false,
            created_at: 1,
            updated_at: 1,
        }
    }

    fn temp_store(name: &str) -> (Store, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("orx-agent-{name}-{}", uuid::Uuid::new_v4()));
        (Store::open_at(dir.clone()).unwrap(), dir)
    }

    #[test]
    fn spawn_is_refused_at_max_depth_regardless_of_breadth() {
        let refusal = spawn_refusal(MAX_SPAWN_DEPTH, 0).expect("max depth must be refused");
        assert!(refusal.contains("levels deep"), "{refusal}");
        assert!(spawn_refusal(MAX_SPAWN_DEPTH - 1, 0).is_none());
    }

    #[test]
    fn one_session_may_only_run_so_many_helpers_at_once() {
        assert!(spawn_refusal(0, MAX_LIVE_SPAWNS - 1).is_none());
        let refusal = spawn_refusal(0, MAX_LIVE_SPAWNS).expect("the cap must refuse one more");
        assert!(refusal.contains("in flight"), "{refusal}");
    }

    #[test]
    fn spawn_depth_counts_a_shallow_chain_exactly() {
        let (store, dir) = temp_store("depth-shallow");
        let root = session("chat_root", None);
        store.create_chat_session(&root).unwrap();
        let child = session("chat_child", Some("chat_root"));
        store.create_chat_session(&child).unwrap();
        let grandchild = session("chat_grandchild", Some("chat_child"));

        assert_eq!(spawn_depth(&store, &root).unwrap(), 0);
        assert_eq!(spawn_depth(&store, &child).unwrap(), 1);
        assert_eq!(spawn_depth(&store, &grandchild).unwrap(), 2);

        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn spawn_depth_stops_walking_at_the_cap() {
        let (store, dir) = temp_store("depth-cap");
        let mut previous_id: Option<String> = None;
        let mut last = session("chat_0", None);
        for i in 0..(MAX_SPAWN_DEPTH + 2) {
            let current = session(&format!("chat_{i}"), previous_id.as_deref());
            store.create_chat_session(&current).unwrap();
            previous_id = Some(current.id.clone());
            last = current;
        }

        // The chain is deeper than the cap; the walk must stop counting there,
        // not report the true (longer) depth.
        assert_eq!(spawn_depth(&store, &last).unwrap(), MAX_SPAWN_DEPTH);

        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_task_is_required_and_comes_from_one_place() {
        assert_eq!(
            task_text(Some("  Sweep the literature  ".into()), false).unwrap(),
            "Sweep the literature"
        );
        assert!(task_text(None, false).is_err());
        assert!(task_text(Some("   ".into()), false).is_err());
        // --stdin and a positional together are ambiguous, so neither is used.
        assert!(task_text(Some("from the args".into()), true).is_err());
    }
}
