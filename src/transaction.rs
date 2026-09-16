use std::env;
use std::io;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, TryRecvError, bounded};
use rift_client::{RiftCommand, RiftEvent};

use crate::rift::Rift;
use crate::{Result, state_error};

const DEFAULT_STEP_TIMEOUT: Duration = Duration::from_millis(2_500);
const BATCH_ITEM_BUDGET: Duration = Duration::from_secs(2);
const BATCH_SETUP_TIMEOUT: Duration = Duration::from_millis(2_500);
const MIN_BATCH_TIMEOUT: Duration = Duration::from_secs(30);
const STATE_PROBE_INTERVAL: Duration = Duration::from_millis(20);
/// How long a confirmed step insists on a matching event before it will accept
/// a direct state probe.
///
/// The event requirement exists to avoid reading pre-command state and calling
/// it success. It also means a command that is already satisfied -- a
/// reassertion of placement Rift has done -- emits nothing and so blocks for
/// the whole timeout before the post-deadline check notices. Past this grace
/// period a probe is allowed, which keeps the staleness guarantee for the
/// window where it matters while letting a no-op settle in ~250 ms rather than
/// the full budget.
const EVENT_GRACE_PERIOD: Duration = Duration::from_millis(250);
const STABLE_STATE_DURATION: Duration = Duration::from_millis(350);
const SLOW_STEP_THRESHOLD: Duration = Duration::from_millis(750);

#[derive(Clone, Copy)]
pub enum EventExpectation<'a> {
    Display(&'a str),
    Workspace {
        display_uuid: &'a str,
        workspace_name: &'a str,
    },
}

impl EventExpectation<'_> {
    fn matches(self, event: &RiftEvent) -> bool {
        if !is_transition_event(event) {
            return false;
        }
        match self {
            Self::Display(display_uuid) => event.display_uuid() == Some(display_uuid),
            Self::Workspace {
                display_uuid,
                workspace_name,
            } => {
                event.display_uuid() == Some(display_uuid)
                    && event_workspace(event) == Some(workspace_name)
            }
        }
    }
}

enum EventMessage {
    Event(Box<RiftEvent>),
    Error(String),
}

pub struct RiftTransaction<'a> {
    rift: &'a Rift,
    events: Receiver<EventMessage>,
    step_timeout: Duration,
    batch_deadline: Option<Instant>,
}

impl<'a> RiftTransaction<'a> {
    pub fn new(rift: &'a Rift) -> Result<Self> {
        Self::with_limits(rift, DEFAULT_STEP_TIMEOUT, None)
    }

    pub fn for_batch(rift: &'a Rift, item_count: usize) -> Result<Self> {
        Self::with_limits(
            rift,
            DEFAULT_STEP_TIMEOUT,
            Some(Instant::now() + Self::batch_timeout(item_count)),
        )
    }

    fn with_limits(
        rift: &'a Rift,
        step_timeout: Duration,
        batch_deadline: Option<Instant>,
    ) -> Result<Self> {
        let events = start_event_reader(rift.subscribe()?);
        Ok(Self {
            rift,
            events,
            step_timeout,
            batch_deadline,
        })
    }

    pub fn step<StateCheck>(
        &self,
        description: &str,
        command: RiftCommand,
        expectation: EventExpectation<'_>,
        state_matches: StateCheck,
    ) -> Result<()>
    where
        StateCheck: FnMut(&Rift) -> Result<bool>,
    {
        self.ensure_budget(description)?;
        let started = Instant::now();
        self.rift.execute(command)?;
        self.wait_for_state(
            description,
            expectation,
            state_matches,
            started,
            self.step_timeout,
            false,
        )
    }

    pub fn confirmed_step_with_timeout<StateCheck>(
        &self,
        description: &str,
        command: RiftCommand,
        expectation: EventExpectation<'_>,
        state_matches: StateCheck,
        timeout: Duration,
    ) -> Result<()>
    where
        StateCheck: FnMut(&Rift) -> Result<bool>,
    {
        self.ensure_budget(description)?;
        self.drain_pending_events()?;
        let started = Instant::now();
        self.rift.execute(command)?;
        self.wait_for_state(
            description,
            expectation,
            state_matches,
            started,
            timeout,
            true,
        )
    }

    pub fn confirmed_phase<Commands, StateCheck>(
        &self,
        description: &str,
        commands: Commands,
        expectation: EventExpectation<'_>,
        state_matches: StateCheck,
    ) -> Result<()>
    where
        Commands: IntoIterator<Item = RiftCommand>,
        StateCheck: FnMut(&Rift) -> Result<bool>,
    {
        self.confirmed_phase_with_timeout(
            description,
            commands,
            expectation,
            state_matches,
            self.step_timeout,
        )
    }

    pub fn confirmed_phase_with_timeout<Commands, StateCheck>(
        &self,
        description: &str,
        commands: Commands,
        expectation: EventExpectation<'_>,
        state_matches: StateCheck,
        timeout: Duration,
    ) -> Result<()>
    where
        Commands: IntoIterator<Item = RiftCommand>,
        StateCheck: FnMut(&Rift) -> Result<bool>,
    {
        self.ensure_budget(description)?;
        self.drain_pending_events()?;
        let started = Instant::now();
        for command in commands {
            self.rift.execute(command)?;
        }
        self.wait_for_state(
            description,
            expectation,
            state_matches,
            started,
            timeout,
            true,
        )
    }

    /// Wait for state already set in motion outside this transaction -- an
    /// application activation, a space switch -- without issuing a command.
    pub fn await_state<StateCheck>(
        &self,
        description: &str,
        expectation: EventExpectation<'_>,
        state_matches: StateCheck,
        timeout: Duration,
    ) -> Result<()>
    where
        StateCheck: FnMut(&Rift) -> Result<bool>,
    {
        self.ensure_budget(description)?;
        self.wait_for_state(
            description,
            expectation,
            state_matches,
            Instant::now(),
            timeout,
            false,
        )
    }

    pub fn settle<StateCheck>(
        &self,
        description: &str,
        expectation: EventExpectation<'_>,
        mut state_matches: StateCheck,
    ) -> Result<()>
    where
        StateCheck: FnMut(&Rift) -> Result<bool>,
    {
        self.ensure_budget(description)?;
        let started = Instant::now();
        let deadline = self.deadline(started, self.step_timeout);
        let mut stable_since = state_matches(self.rift)?.then(Instant::now);

        while Instant::now() < deadline {
            if stable_since.is_some_and(|stable| stable.elapsed() >= STABLE_STATE_DURATION) {
                trace_wait(description, "stable state", started.elapsed());
                return Ok(());
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            let wait = remaining.min(STATE_PROBE_INTERVAL);
            match self.events.recv_timeout(wait) {
                Ok(EventMessage::Event(event)) if expectation.matches(&event) => {
                    stable_since = state_matches(self.rift)?.then(Instant::now);
                }
                Ok(EventMessage::Event(_)) => {}
                Ok(EventMessage::Error(error)) => {
                    return Err(state_error(format!("event subscription failed: {error}")));
                }
                Err(RecvTimeoutError::Timeout) => {
                    if state_matches(self.rift)? {
                        stable_since.get_or_insert_with(Instant::now);
                    } else {
                        stable_since = None;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(state_error("event subscription disconnected"));
                }
            }
        }

        crate::logging::warn(&format!(
            "timeout waiting for stable {description} after {} ms",
            started.elapsed().as_millis()
        ));
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("timed out waiting for stable {description}"),
        )
        .into())
    }

    fn wait_for_state<StateCheck>(
        &self,
        description: &str,
        expectation: EventExpectation<'_>,
        mut state_matches: StateCheck,
        started: Instant,
        timeout: Duration,
        require_matching_event: bool,
    ) -> Result<()>
    where
        StateCheck: FnMut(&Rift) -> Result<bool>,
    {
        let deadline = self.deadline(started, timeout);
        let mut matching_event_seen = false;

        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let wait = remaining.min(STATE_PROBE_INTERVAL);
            match self.events.recv_timeout(wait) {
                Ok(EventMessage::Event(event)) if expectation.matches(&event) => {
                    matching_event_seen = true;
                    if state_matches(self.rift)? {
                        trace_wait(description, "matching event", started.elapsed());
                        return Ok(());
                    }
                }
                Ok(EventMessage::Event(_)) => {}
                Ok(EventMessage::Error(error)) => {
                    return Err(state_error(format!("event subscription failed: {error}")));
                }
                Err(RecvTimeoutError::Timeout) => {
                    let event_requirement_satisfied = !require_matching_event
                        || matching_event_seen
                        || started.elapsed() >= EVENT_GRACE_PERIOD;
                    if event_requirement_satisfied && state_matches(self.rift)? {
                        trace_wait(description, "state probe", started.elapsed());
                        return Ok(());
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(state_error("event subscription disconnected"));
                }
            }
        }

        if state_matches(self.rift)? {
            trace_wait(description, "deadline validation", started.elapsed());
            return Ok(());
        }

        crate::logging::warn(&format!(
            "timeout waiting for {description} after {} ms",
            started.elapsed().as_millis()
        ));
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("timed out waiting for {description}"),
        )
        .into())
    }

    fn drain_pending_events(&self) -> Result<()> {
        loop {
            match self.events.try_recv() {
                Ok(EventMessage::Event(_)) => {}
                Ok(EventMessage::Error(error)) => {
                    return Err(state_error(format!("event subscription failed: {error}")));
                }
                Err(TryRecvError::Empty) => return Ok(()),
                Err(TryRecvError::Disconnected) => {
                    return Err(state_error("event subscription disconnected"));
                }
            }
        }
    }

    fn batch_timeout(item_count: usize) -> Duration {
        let item_count = u32::try_from(item_count).unwrap_or(u32::MAX);
        BATCH_SETUP_TIMEOUT
            .saturating_add(BATCH_ITEM_BUDGET.saturating_mul(item_count))
            .max(MIN_BATCH_TIMEOUT)
    }

    fn deadline(&self, started: Instant, timeout: Duration) -> Instant {
        let step_deadline = started + timeout;
        self.batch_deadline.map_or(step_deadline, |batch_deadline| {
            step_deadline.min(batch_deadline)
        })
    }

    fn ensure_budget(&self, description: &str) -> Result<()> {
        // Every step goes through here, so this is where a running workflow
        // keeps telling the focus-follow hook to stay out of its way.
        crate::focus_lease::renew();
        if self
            .batch_deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("batch timeout exhausted before {description}"),
            )
            .into());
        }
        Ok(())
    }
}

/// Whether a step failed only by running out of time, which callers with a
/// fallback or a later reconciliation pass treat as recoverable.
pub fn is_timeout(error: &crate::DynError) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(|error| error.kind() == io::ErrorKind::TimedOut)
}

fn start_event_reader(subscription: rift_client::RiftMachSubscription) -> Receiver<EventMessage> {
    let (tx, rx) = bounded(64);
    thread::spawn(move || {
        loop {
            match subscription.recv_event() {
                Ok(event) => {
                    if !is_transition_event(&event) {
                        continue;
                    }
                    if tx.send(EventMessage::Event(Box::new(event))).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = tx.send(EventMessage::Error(error.to_string()));
                    return;
                }
            }
        }
    });
    rx
}

fn is_transition_event(event: &RiftEvent) -> bool {
    matches!(
        event,
        RiftEvent::WorkspaceChanged { .. }
            | RiftEvent::WindowsChanged { .. }
            | RiftEvent::FocusedWindowChanged { .. }
            | RiftEvent::LayoutChanged { .. }
    )
}

fn event_workspace(event: &RiftEvent) -> Option<&str> {
    match event {
        RiftEvent::WorkspaceChanged { workspace_name, .. }
        | RiftEvent::WindowsChanged { workspace_name, .. }
        | RiftEvent::FocusedWindowChanged { workspace_name, .. }
        | RiftEvent::LayoutChanged { workspace_name, .. } => Some(workspace_name),
        RiftEvent::WindowTitleChanged { .. } | RiftEvent::StacksChanged { .. } => None,
    }
}

fn trace_wait(description: &str, completion: &str, elapsed: Duration) {
    if env::var_os("RIFT_ERGO_TRACE").is_some() {
        eprintln!(
            "rift-ergo: {description} completed by {completion} in {} ms",
            elapsed.as_millis()
        );
    }
    // A step that takes this long still succeeded, but it is the earliest
    // signal that Rift's confirmation path has regressed.
    if elapsed >= SLOW_STEP_THRESHOLD {
        crate::logging::warn(&format!(
            "slow {description} completed by {completion} in {} ms",
            elapsed.as_millis()
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_client::{WindowId, WorkspaceId};

    #[test]
    fn workspace_expectation_requires_exact_display_and_workspace() {
        let event = RiftEvent::WindowsChanged {
            workspace_id: WorkspaceId { idx: 1, version: 1 },
            workspace_name: "W".into(),
            windows: vec![format!("{:?}", WindowId::new(42, 7).unwrap())],
            space_id: 9,
            display_uuid: Some("external".into()),
        };

        assert!(
            EventExpectation::Workspace {
                display_uuid: "external",
                workspace_name: "W"
            }
            .matches(&event)
        );
        assert!(
            !EventExpectation::Workspace {
                display_uuid: "builtin",
                workspace_name: "W"
            }
            .matches(&event)
        );
        assert!(
            !EventExpectation::Workspace {
                display_uuid: "external",
                workspace_name: "A"
            }
            .matches(&event)
        );
    }

    #[test]
    fn batch_timeout_scales_with_item_count() {
        assert_eq!(RiftTransaction::batch_timeout(0), Duration::from_secs(30));
        assert_eq!(RiftTransaction::batch_timeout(1), Duration::from_secs(30));
        assert_eq!(RiftTransaction::batch_timeout(6), Duration::from_secs(30));
        assert_eq!(
            RiftTransaction::batch_timeout(20),
            Duration::from_millis(42_500)
        );
    }
}
