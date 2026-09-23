//! One resource owner per run; callers hold the instance lock during mutations.
use super::*;
use machineemu_core::runtime::AsyncRunningInstance;

type Connection = Arc<tokio::sync::Mutex<AsyncRunningInstance>>;
pub(super) struct RunSupervisor {
    pub run_id: Id,
    pub running: Option<Connection>,
    pub helpers: Vec<ManagedProcess>,
    pub display: Option<ManagedProcess>,
    pub watching: bool,
}
impl RunSupervisor {
    pub fn new(run_id: Id) -> Self {
        Self {
            run_id,
            running: None,
            helpers: Vec::new(),
            display: None,
            watching: false,
        }
    }
}

pub(super) fn connection(state: &AppState, id: &str) -> Result<Option<Connection>, RuntimeError> {
    Ok(state
        .supervisors
        .lock()
        .map_err(|_| RuntimeError::Process("supervisor lock poisoned".into()))?
        .get(id)
        .and_then(|owner| owner.running.clone()))
}

pub(super) fn install(
    state: &AppState,
    id: &str,
    run_id: Id,
    running: Option<Connection>,
    helpers: Vec<ManagedProcess>,
) -> Result<(), RuntimeError> {
    let mut owners = state
        .supervisors
        .lock()
        .map_err(|_| RuntimeError::Process("supervisor lock poisoned".into()))?;
    let owner = owners
        .entry(id.into())
        .or_insert_with(|| RunSupervisor::new(run_id.clone()));
    if owner.run_id != run_id {
        return Err(RuntimeError::ActiveRun(id.into()));
    }
    owner.running = running;
    if !helpers.is_empty() {
        owner.helpers = helpers;
    }
    Ok(())
}

pub(super) fn disconnect(state: &AppState, id: &str, run_id: &Id) -> Result<(), RuntimeError> {
    if let Some(owner) = state
        .supervisors
        .lock()
        .map_err(|_| RuntimeError::Process("supervisor lock poisoned".into()))?
        .get_mut(id)
        && owner.run_id == *run_id
    {
        owner.running = None;
    }
    Ok(())
}

/// Common teardown for operator stop and observed process exit. Stale watchers
/// cannot revoke resources belonging to a replacement run.
pub(super) fn teardown(state: &AppState, id: &str, run_id: &Id) -> Result<(), RuntimeError> {
    let owner = {
        let mut owners = state
            .supervisors
            .lock()
            .map_err(|_| RuntimeError::Process("supervisor lock poisoned".into()))?;
        if owners.get(id).is_some_and(|owner| owner.run_id != *run_id) {
            return Ok(());
        }
        owners.remove(id)
    };
    streams::revoke_display(state, id, run_id.as_str());
    state
        .audio_sessions
        .lock()
        .map_err(|_| RuntimeError::Process("audio session lock poisoned".into()))?
        .retain(|_, session| session.instance_id != id);
    state
        .stream_tickets
        .lock()
        .map_err(|_| RuntimeError::Process("stream ticket lock poisoned".into()))?
        .retain(|_, ticket| ticket.instance_id != id);
    if let Some(mut owner) = owner {
        owner.display.take();
        helpers::stop_all_checked(&mut owner.helpers)?;
    }
    Ok(())
}

/// The instance gate is held by callers before acquiring the shared QMP owner.
pub(super) struct QmpSession(tokio::sync::OwnedMutexGuard<AsyncRunningInstance>);
impl std::ops::Deref for QmpSession {
    type Target = machineemu_core::protocols::async_qmp::AsyncQmp;
    fn deref(&self) -> &Self::Target {
        &self.0.qmp
    }
}
impl std::ops::DerefMut for QmpSession {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0.qmp
    }
}

pub(super) async fn qmp(
    state: &AppState,
    run: &machineemu_core::domain::Run,
) -> Result<QmpSession, RuntimeError> {
    let connection = match connection(state, run.instance_id.as_str())? {
        Some(connection) => connection,
        None => {
            let running = AsyncRunningInstance::recover(run).await?;
            let connection = Arc::new(tokio::sync::Mutex::new(running));
            install(
                state,
                run.instance_id.as_str(),
                run.run_id.clone(),
                Some(connection.clone()),
                Vec::new(),
            )?;
            connection
        }
    };
    let running = connection.lock_owned().await;
    if running.run_id != run.run_id {
        return Err(RuntimeError::Process(
            "QMP request belongs to a stale run".into(),
        ));
    }
    Ok(QmpSession(running))
}
