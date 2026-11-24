//! Whether the machine is busy: what the configured probes say and which exclusive jobs run.
//!
//! The gate fails closed. A probe that cannot be read counts as busy, because deferring costs a
//! delay while acting on a misread costs someone their work; and a machine whose `node.toml` says
//! nothing about it is *unmonitored*, which blocks too: not knowing is not the same as idle.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use legion_core::CancellationToken;
use legion_core::config::{BusyConfig, BusyProbe};
use legion_core::proto::{ErrorBody, ErrorCode};
use serde::Serialize;
use serde_json::json;
use tokio::task::JoinSet;

use super::actuator::{Actuator, Ran};
use crate::job::ExclusiveJob;

/// How long past its own timeout a probe is waited for before it is given up on, in case the
/// command cannot be stopped in time.
const PROBE_SLACK: Duration = Duration::from_secs(2);

/// Whether the machine may be interrupted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BusyState {
    /// Every probe said idle (or `node.toml` says `busy = "none"`) and no exclusive job runs.
    Idle,
    /// Something is busy, or a probe could not tell.
    Busy,
    /// `node.toml` says nothing about what keeps this machine busy.
    Unmonitored,
}

impl BusyState {
    /// The state as the result spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            BusyState::Idle => "idle",
            BusyState::Busy => "busy",
            BusyState::Unmonitored => "unmonitored",
        }
    }
}

/// Where a reason came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// A `[[busy.probes]]` entry.
    Probe,
    /// A job started with `exclusive: true`.
    Job,
}

/// One thing keeping the machine busy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Reason {
    /// A probe or a job.
    pub source: Source,
    /// The probe's or the job's name.
    pub name: String,
    /// What it said, or why it could not tell.
    pub detail: String,
}

/// The machine's busy state and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Busy {
    /// `idle`, `busy` or `unmonitored`.
    pub state: BusyState,
    /// Everything that keeps it busy; empty unless `state` is `busy`.
    pub reasons: Vec<Reason>,
}

/// Asks every probe at once, each within its own timeout, and folds in the exclusive jobs (or the
/// failure to read them, which counts as busy).
pub async fn evaluate(
    actuator: &Arc<dyn Actuator>,
    config: Option<&BusyConfig>,
    jobs: Result<Vec<ExclusiveJob>, ErrorBody>,
    cancel: &CancellationToken,
) -> Busy {
    let mut reasons = Vec::new();
    if let Some(BusyConfig::Probes(probes)) = config {
        reasons.extend(ask_probes(actuator, probes, cancel).await);
    }
    match jobs {
        Ok(jobs) => reasons.extend(jobs.into_iter().map(|job| Reason {
            source: Source::Job,
            name: job.name,
            detail: format!(
                "exclusive job {} has been running since {}",
                job.id, job.since
            ),
        })),
        Err(error) => reasons.push(Reason {
            source: Source::Job,
            name: "jobs".to_owned(),
            detail: format!(
                "could not tell whether an exclusive job is running: {}",
                error.message
            ),
        }),
    }
    let state = if !reasons.is_empty() {
        BusyState::Busy
    } else if config.is_none() {
        BusyState::Unmonitored
    } else {
        BusyState::Idle
    };
    Busy { state, reasons }
}

async fn ask_probes(
    actuator: &Arc<dyn Actuator>,
    probes: &[BusyProbe],
    cancel: &CancellationToken,
) -> Vec<Reason> {
    let mut set = JoinSet::new();
    for (index, probe) in probes.iter().enumerate() {
        let actuator = Arc::clone(actuator);
        let run = probe.run.clone();
        let cancel = cancel.clone();
        let timeout = Duration::from_secs(u64::from(probe.timeout_s));
        set.spawn(async move {
            let asked =
                tokio::time::timeout(timeout + PROBE_SLACK, actuator.run(&run, timeout, &cancel))
                    .await;
            let outcome = asked.unwrap_or_else(|_| {
                Ok(Ran {
                    timed_out: true,
                    ..Ran::default()
                })
            });
            (index, outcome)
        });
    }
    let mut answers: Vec<Option<Result<Ran, ErrorBody>>> = vec![None; probes.len()];
    while let Some(joined) = set.join_next().await {
        if let Ok((index, outcome)) = joined
            && let Some(slot) = answers.get_mut(index)
        {
            *slot = Some(outcome);
        }
    }
    probes
        .iter()
        .zip(answers)
        .filter_map(|(probe, answer)| read_probe(probe, answer.as_ref()))
        .collect()
}

/// What one probe's answer means: `None` for idle, a reason for busy or for not knowing.
pub fn read_probe(probe: &BusyProbe, answer: Option<&Result<Ran, ErrorBody>>) -> Option<Reason> {
    let unknown = |why: String| {
        Some(Reason {
            source: Source::Probe,
            name: probe.name.clone(),
            detail: format!("could not tell, so counted as busy: {why}"),
        })
    };
    let ran = match answer {
        None => return unknown("the probe stopped without an answer".to_owned()),
        Some(Err(error)) => return unknown(error.message.clone()),
        Some(Ok(ran)) => ran,
    };
    if ran.timed_out {
        return unknown(format!("no answer within {} s", probe.timeout_s));
    }
    match ran.exit_code {
        Some(0) => Some(Reason {
            source: Source::Probe,
            name: probe.name.clone(),
            detail: match ran.stdout.lines().map(str::trim).find(|l| !l.is_empty()) {
                Some(line) => format!("busy: {line}"),
                None => "busy (the probe exited 0)".to_owned(),
            },
        }),
        Some(1) => None,
        Some(code) => {
            let said = ran.first_line();
            if said.is_empty() {
                unknown(format!(
                    "the probe exited {code}, which is neither 0 (busy) nor 1 (idle)"
                ))
            } else {
                unknown(format!(
                    "the probe exited {code}, which is neither 0 (busy) nor 1 (idle): {said}"
                ))
            }
        }
        None => unknown("the probe was stopped by a signal".to_owned()),
    }
}

/// The `busy` error a gated call fails with, saying what keeps the machine busy and, when nothing
/// is configured, exactly what to write in `node.toml`.
pub fn refusal(busy: &Busy, what: &str, node_toml: &Path) -> ErrorBody {
    let details = json!({"state": busy.state.as_str(), "reasons": busy.reasons});
    if busy.state == BusyState::Unmonitored {
        return ErrorBody::new(
            ErrorCode::Busy,
            format!(
                "Refused to {what}: node.toml does not say what keeps this machine busy, and not knowing is not the same as idle."
            ),
        )
        .with_hint(format!(
            "If nothing on this machine needs protecting, add the line busy = \"none\" to {}. Otherwise describe what does with [[busy.probes]] tables (name, run = [program and arguments, exit 0 while busy and 1 while idle], timeout_s). To go ahead this once anyway, pass force: true.",
            node_toml.display()
        ))
        .with_details(details);
    }
    let listed: Vec<String> = busy
        .reasons
        .iter()
        .map(|reason| {
            let source = match reason.source {
                Source::Probe => "probe",
                Source::Job => "job",
            };
            format!("{source} {}: {}", reason.name, reason.detail)
        })
        .collect();
    ErrorBody::new(
        ErrorCode::Busy,
        format!("Refused to {what}: the machine is busy ({}).", listed.join("; ")),
    )
    .with_hint(
        "Wait until what keeps it busy has finished (power.status shows the state), or pass force: true to go ahead anyway.",
    )
    .with_details(details)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::power::fake::{RecordingActuator, Reply};

    fn probe(name: &str, run: &[&str], timeout_s: u32) -> BusyProbe {
        BusyProbe {
            name: name.to_owned(),
            run: run.iter().map(|part| (*part).to_owned()).collect(),
            timeout_s,
        }
    }

    fn job() -> ExclusiveJob {
        ExclusiveJob {
            id: "j-1".to_owned(),
            name: "render-frames".to_owned(),
            since: "2026-11-24T10:00:00.000Z".to_owned(),
        }
    }

    async fn evaluate_with(
        fake: RecordingActuator,
        config: Option<&BusyConfig>,
        jobs: Result<Vec<ExclusiveJob>, ErrorBody>,
    ) -> Busy {
        let actuator: Arc<dyn Actuator> = Arc::new(fake);
        evaluate(&actuator, config, jobs, &CancellationToken::new()).await
    }

    #[tokio::test]
    async fn nothing_configured_is_unmonitored_and_none_is_idle() {
        let unmonitored = evaluate_with(RecordingActuator::new(), None, Ok(Vec::new())).await;
        assert_eq!(unmonitored.state, BusyState::Unmonitored);
        assert!(unmonitored.reasons.is_empty());
        let idle = evaluate_with(
            RecordingActuator::new(),
            Some(&BusyConfig::Never),
            Ok(Vec::new()),
        )
        .await;
        assert_eq!(idle.state, BusyState::Idle);
    }

    #[tokio::test]
    async fn an_exclusive_job_makes_any_machine_busy() {
        for config in [None, Some(&BusyConfig::Never)] {
            let busy = evaluate_with(RecordingActuator::new(), config, Ok(vec![job()])).await;
            assert_eq!(busy.state, BusyState::Busy);
            assert_eq!(busy.reasons[0].source, Source::Job);
            assert_eq!(busy.reasons[0].name, "render-frames");
            assert!(busy.reasons[0].detail.contains("j-1"), "{:?}", busy.reasons);
        }
        let unreadable = evaluate_with(
            RecordingActuator::new(),
            Some(&BusyConfig::Never),
            Err(ErrorBody::new(
                ErrorCode::Failed,
                "jobs directory unreadable",
            )),
        )
        .await;
        assert_eq!(unreadable.state, BusyState::Busy);
        assert!(unreadable.reasons[0].detail.contains("could not tell"));
    }

    #[tokio::test]
    async fn probe_exit_codes_mean_busy_idle_or_unknown() {
        let fake = RecordingActuator::new()
            .reply(Reply::new(&["is-rendering"], 0, "blender 4242\n"))
            .reply(Reply::new(&["is-backing-up"], 1, ""))
            .reply(Reply {
                stderr: "permission denied\n".to_owned(),
                ..Reply::new(&["is-odd"], 2, "")
            })
            .reply(Reply {
                argv: vec!["is-slow".to_owned()],
                timed_out: true,
                ..Reply::default()
            })
            .reply(Reply {
                argv: vec!["is-missing".to_owned()],
                error: Some(ErrorBody::new(
                    ErrorCode::Unsupported,
                    "`is-missing` is not installed on this machine.",
                )),
                ..Reply::default()
            });
        let config = BusyConfig::Probes(vec![
            probe("render", &["is-rendering"], 5),
            probe("backup", &["is-backing-up"], 5),
            probe("odd", &["is-odd"], 5),
            probe("slow", &["is-slow"], 7),
            probe("missing", &["is-missing"], 5),
        ]);
        let busy = evaluate_with(fake, Some(&config), Ok(Vec::new())).await;
        assert_eq!(busy.state, BusyState::Busy);
        let by_name = |name: &str| busy.reasons.iter().find(|reason| reason.name == name);
        assert_eq!(by_name("render").unwrap().detail, "busy: blender 4242");
        assert!(by_name("backup").is_none());
        assert!(by_name("odd").unwrap().detail.contains("exited 2"));
        assert!(by_name("odd").unwrap().detail.contains("permission denied"));
        assert!(by_name("slow").unwrap().detail.contains("within 7 s"));
        assert!(by_name("missing").unwrap().detail.contains("not installed"));
        // Reasons keep the order the probes are written in.
        let names: Vec<&str> = busy.reasons.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["render", "odd", "slow", "missing"]);
    }

    #[tokio::test]
    async fn every_probe_saying_idle_is_idle() {
        let fake = RecordingActuator::new().reply(Reply::new(&["check"], 1, ""));
        let config = BusyConfig::Probes(vec![
            probe("a", &["check", "a"], 5),
            probe("b", &["check", "b"], 5),
        ]);
        let busy = evaluate_with(fake, Some(&config), Ok(Vec::new())).await;
        assert_eq!(busy.state, BusyState::Idle);
    }

    #[test]
    fn the_refusal_says_what_to_write_when_nothing_is_configured() {
        let node_toml = Path::new("/home/me/.config/legion/node.toml");
        let unmonitored = Busy {
            state: BusyState::Unmonitored,
            reasons: Vec::new(),
        };
        let error = refusal(&unmonitored, "restart this machine", node_toml);
        assert_eq!(error.code, ErrorCode::Busy);
        let hint = error.hint.unwrap();
        assert!(hint.contains("busy = \"none\""), "{hint}");
        assert!(hint.contains("/home/me/.config/legion/node.toml"), "{hint}");
        assert!(hint.contains("force: true"), "{hint}");

        let busy = Busy {
            state: BusyState::Busy,
            reasons: vec![Reason {
                source: Source::Probe,
                name: "render".to_owned(),
                detail: "busy: blender".to_owned(),
            }],
        };
        let error = refusal(&busy, "put this machine to sleep", node_toml);
        assert!(
            error.message.contains("probe render: busy: blender"),
            "{}",
            error.message
        );
        assert_eq!(
            error.details.unwrap()["reasons"][0]["source"],
            json!("probe")
        );
    }
}
