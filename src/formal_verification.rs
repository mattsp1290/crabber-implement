use std::collections::{BTreeMap, BTreeSet, VecDeque};

use dmt::{EndStatus, Guard, NodeKind};
use flows_core::{Compiled, compile};

use crate::review_checkpoint::{self as checkpoint, CheckpointPhase, PhaseEvent};
use crate::workflow::{document_from, occurrence_allowed};

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum ReviewState {
    Missing,
    Approved,
    Changes,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct State {
    node: String,
    implemented: bool,
    review: ReviewState,
    reviews: u32,
    fixes: u32,
}

fn successor(graph: &Compiled, state: &State, label: &str) -> State {
    let edges = graph
        .graph
        .edges_from(&state.node.clone().into())
        .collect::<Vec<_>>();
    let edge = edges
        .iter()
        .find(|edge| edge.guard == Guard::Label(label.into()))
        .or_else(|| edges.iter().find(|edge| edge.guard == Guard::Default))
        .unwrap_or_else(|| panic!("no transition for {} outcome {label}", state.node));
    let mut next = state.clone();
    next.node = edge.to.as_str().into();
    next
}

fn verify_budget(budget: u32, first: u32, resumed: bool) -> (usize, usize) {
    let graph = compile(
        &document_from(budget, 3600, first, resumed),
        &BTreeMap::new(),
    )
    .expect("actual graph compiles");
    let initial = State {
        node: graph.graph.start().as_str().into(),
        implemented: resumed,
        review: ReviewState::Missing,
        reviews: first - 1,
        fixes: first - 1,
    };
    let mut queue = VecDeque::from([(initial.clone(), vec![initial.node.clone()])]);
    let mut visited = BTreeSet::from([initial]);
    let mut adjacency = BTreeMap::<State, Vec<State>>::new();
    let mut terminals = 0;
    while let Some((state, trace)) = queue.pop_front() {
        let failure = || format!("budget {budget}, path {}", trace.join(" -> "));
        assert!(
            state.reviews <= budget && state.fixes < budget,
            "session budget exceeded: {}",
            failure()
        );
        assert!(
            trace.len() <= (2 * budget + 6) as usize,
            "termination bound exceeded: {}",
            failure()
        );
        let definition = graph
            .graph
            .node(&state.node.clone().into())
            .expect("reachable node exists");
        match definition.kind {
            NodeKind::End {
                status: EndStatus::Completed,
            } => {
                assert!(
                    state.implemented,
                    "completed without implementation: {}",
                    failure()
                );
                assert_eq!(
                    state.review,
                    ReviewState::Approved,
                    "completed without fresh approval: {}",
                    failure()
                );
                assert!(
                    state.reviews > 0,
                    "completed without any review: {}",
                    failure()
                );
                terminals += 1;
                continue;
            }
            NodeKind::End {
                status: EndStatus::Failed,
            } => {
                terminals += 1;
                continue;
            }
            NodeKind::Task => {}
            _ => panic!(
                "unmodeled node kind at {}: {:?}",
                state.node, definition.kind
            ),
        }
        let metadata = graph.nodes.get(&state.node);
        if metadata.is_some() {
            assert_eq!(
                definition.retry.max_attempts,
                1,
                "task retry changes require a new concurrency model: {}",
                failure()
            );
        }
        let mut entered = state.clone();
        let occurrence = match state.node.as_str() {
            "implement" => {
                assert!(!state.implemented, "implementation repeated: {}", failure());
                1
            }
            "review" => state.reviews + 2 - first,
            "fix-review" => {
                assert_eq!(
                    state.review,
                    ReviewState::Changes,
                    "fix without selected change request: {}",
                    failure()
                );
                state.fixes + 2 - first
            }
            "borb_acquire" | "resume-review" => 1,
            node if node.starts_with("release:") => 1,
            _ => panic!("unmodeled task {}", state.node),
        };
        if metadata
            .and_then(|node| node.max_occurrences)
            .is_some_and(|max| !occurrence_allowed(occurrence, max))
        {
            terminals += 1;
            continue;
        }
        match state.node.as_str() {
            "review" => entered.reviews += 1,
            "fix-review" => entered.fixes += 1,
            _ => {}
        }
        terminals += 1;
        let labels =
            metadata.map_or_else(|| vec!["ok".to_owned()], |node| node.outcome_labels.clone());
        for label in labels {
            if state.node == "review"
                && label == "changes"
                && !checkpoint::can_fix(entered.reviews, budget)
            {
                terminals += 1;
                continue;
            }
            let mut output = entered.clone();
            match (state.node.as_str(), label.as_str()) {
                ("implement", "implemented") => output.implemented = true,
                ("review", "approved") => output.review = ReviewState::Approved,
                ("review", "changes") => output.review = ReviewState::Changes,
                ("fix-review", "fixed") => output.review = ReviewState::Missing,
                ("borb_acquire", "ok") | ("resume-review", "ready") => {}
                (node, "ok") if node.starts_with("release:") => {}
                _ => panic!("unmodeled outcome {}/{label}", state.node),
            }
            let next = successor(&graph, &output, &label);
            adjacency
                .entry(state.clone())
                .or_default()
                .push(next.clone());
            if visited.insert(next.clone()) {
                let mut next_trace = trace.clone();
                next_trace.push(format!("{}[{label}]", next.node));
                queue.push_back((next, next_trace));
            }
        }
    }
    let mut indegree = visited
        .iter()
        .cloned()
        .map(|state| (state, 0_usize))
        .collect::<BTreeMap<_, _>>();
    for successors in adjacency.values() {
        for state in successors {
            *indegree.get_mut(state).unwrap() += 1;
        }
    }
    let mut roots = indegree
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(state, _)| state.clone())
        .collect::<VecDeque<_>>();
    let mut longest = visited
        .iter()
        .cloned()
        .map(|state| (state, 1_usize))
        .collect::<BTreeMap<_, _>>();
    let mut removed = 0;
    while let Some(state) = roots.pop_front() {
        removed += 1;
        assert!(
            longest[&state] <= (2 * budget + 6) as usize,
            "longest path exceeds termination bound at budget {budget}"
        );
        if let Some(successors) = adjacency.get(&state) {
            for next in successors {
                let candidate = longest[&state] + 1;
                let length = longest.get_mut(next).unwrap();
                *length = (*length).max(candidate);
                let count = indegree.get_mut(next).unwrap();
                *count -= 1;
                if *count == 0 {
                    roots.push_back(next.clone());
                }
            }
        }
    }
    assert_eq!(
        removed,
        visited.len(),
        "reachable cycle permits nontermination at budget {budget}"
    );
    (visited.len(), terminals)
}

#[test]
fn formal_workflow_reachability() {
    let mut states = 0;
    let mut terminals = 0;
    for budget in 1..=100 {
        let (count, ends) = verify_budget(budget, 1, false);
        states += count;
        terminals += ends;
        for first in 1..=budget {
            for refresh in [false, true] {
                let entry_round = if refresh {
                    checkpoint::refresh_round(first, budget).expect("eligible source round")
                } else {
                    first
                };
                assert_eq!(entry_round, first, "refresh global review budget reset");
                let (count, ends) = verify_budget(budget, entry_round, true);
                states += count;
                terminals += ends;
            }
        }
    }
    println!(
        "crabber-workflow-checker-v3 budgets=1..100 states={states} terminal_cases={terminals} safety=PASS termination=PASS"
    );
}

#[test]
fn formal_checkpoint_reachability() {
    use CheckpointPhase::*;
    let phases = [
        ReviewPending,
        ReviewReportsReady,
        FixIntentRecorded,
        FixCommitted,
        Completed,
    ];
    let mut checked = 0usize;
    for phase in phases {
        for same in [false, true] {
            for first in [false, true] {
                for second in [false, true] {
                    let valid = [first, second];
                    for published in [false, true] {
                        let actual_valid = valid.map(|v| v && published);
                        assert_eq!(
                            checkpoint::reports_ready(actual_valid),
                            first && second && published,
                            "incomplete publication counted as two reports"
                        );
                        assert_eq!(
                            checkpoint::next_phase(phase, PhaseEvent::Reports(actual_valid)),
                            if matches!(phase, ReviewPending | ReviewReportsReady)
                                && actual_valid.into_iter().all(|v| v)
                            {
                                Some(ReviewReportsReady)
                            } else {
                                None
                            },
                            "ready state before durable paired reports"
                        );
                        for (i, complete) in actual_valid.into_iter().enumerate() {
                            let action = checkpoint::role_start(phase, same, complete);
                            if action == Some(true) {
                                assert!(
                                    same && !complete && phase == ReviewPending,
                                    "role relaunched or mutation boundary bypassed, role {i}"
                                );
                            }
                            if !same
                                || matches!(phase, FixIntentRecorded | FixCommitted | Completed)
                            {
                                assert!(action.is_none(), "stale or mutating boundary resumed");
                            }
                        }
                        for approved in [false, true] {
                            if checkpoint::next_phase(
                                phase,
                                PhaseEvent::Complete(actual_valid, approved),
                            )
                            .is_some()
                            {
                                assert!(
                                    phase == ReviewReportsReady
                                        && first
                                        && second
                                        && published
                                        && approved,
                                    "completed without fresh paired reports"
                                );
                            }
                        }
                        if checkpoint::next_phase(phase, PhaseEvent::Fix(actual_valid)).is_some() {
                            assert!(
                                phase == ReviewReportsReady && first && second && published,
                                "fix without complete reports"
                            );
                        }
                        checked += 1;
                    }
                }
            }
        }
    }
    for phase in phases {
        assert_eq!(
            checkpoint::can_resume(phase),
            matches!(phase, ReviewPending | ReviewReportsReady),
            "unsafe checkpoint resumed"
        );
    }
    for budget in 1..=100 {
        for first in 1..=budget {
            for local in 0..=budget + 1 {
                let round = checkpoint::review_round(first, local, budget);
                let expected = if local > 0 && first + local - 1 <= budget {
                    Some(first + local - 1)
                } else {
                    None
                };
                assert_eq!(round, expected, "global review budget reset");
            }
        }
    }
    let (_tmp, config) = checkpoint::tests::fixture();
    let inputs = checkpoint::ReviewInputs::current(&config, ["a".into(), "b".into()]).unwrap();
    assert!(checkpoint::identity_matches(&inputs, &inputs));
    for changed in 0..19 {
        let mut other = inputs.clone();
        match changed {
            0 => other.head.push('x'),
            1 => other.branch.push('x'),
            2 => other.model.push('x'),
            3 => other.budget += 1,
            4 => other.prompts[0].push('x'),
            5 => other.repo.push("other"),
            6 => other.git_dir.push("other"),
            7 => other.base.push('x'),
            8 => other.plan.push('x'),
            9 => other.skill.push('x'),
            10 => other.endpoint.push('x'),
            11 => other.turns += 1,
            12 => other.provider_profile.push_str("-changed"),
            13 => other.reasoning_effort.push_str("-changed"),
            14 => other.executable.push('x'),
            15 => other.prompts[1].push('x'),
            16 => {
                other.snapshot = Some(crate::review_inputs::SnapshotIdentity {
                    manifest: "0".repeat(64),
                    policy: 1,
                })
            }
            17 => other.origin = Some("0".repeat(64)),
            _ => other.protocol.push_str("-changed"),
        }
        assert!(
            !checkpoint::identity_matches(&inputs, &other),
            "stale checkpoint identity accepted"
        );
    }
    println!(
        "crabber-checkpoint-checker-v1 cases={checked} budgets=1..100 safety=PASS termination=PASS"
    );
}

#[test]
fn formal_refresh_readiness() {
    use CheckpointPhase::*;
    let phases = [
        ReviewPending,
        ReviewReportsReady,
        FixIntentRecorded,
        FixCommitted,
        Completed,
    ];
    let mut checked = 0usize;
    for phase in phases {
        for bits in 0..32 {
            let same_identity = bits & 1 != 0;
            let source_verified = bits & 2 != 0;
            let origin_published = bits & 4 != 0;
            let snapshot_verified = bits & 8 != 0;
            let reader_integrity = bits & 16 != 0;
            let ready = checkpoint::refresh_eligible(
                phase,
                same_identity,
                source_verified,
                origin_published,
                snapshot_verified,
            );
            assert_eq!(
                ready,
                matches!(phase, ReviewPending | ReviewReportsReady)
                    && same_identity
                    && source_verified
                    && origin_published
                    && snapshot_verified,
                "refresh accepted stale source or incomplete snapshot readiness"
            );
            for source_reports in [[false, false], [false, true], [true, false], [true, true]] {
                let (attempts, sessions, reused) = checkpoint::fresh_roles();
                assert_eq!(attempts, [0, 0], "refresh imported old report attempts");
                assert_eq!(
                    sessions,
                    [None, None],
                    "refresh imported old report sessions"
                );
                assert_eq!(
                    reused,
                    [false, false],
                    "refresh imported old report as approval"
                );
                for (role, reused) in reused.into_iter().enumerate() {
                    assert_eq!(
                        checkpoint::role_start(ReviewPending, true, reused),
                        Some(true),
                        "source report suppressed fresh reviewer, role {role}, source {source_reports:?}"
                    );
                }
                for fresh in [[false, false], [false, true], [true, false], [true, true]] {
                    let validated = fresh.map(|report| report && ready && reader_integrity);
                    if checkpoint::next_phase(
                        ReviewReportsReady,
                        PhaseEvent::Complete(validated, true),
                    )
                    .is_some()
                    {
                        assert!(
                            ready && reader_integrity && fresh == [true, true],
                            "refresh completed without two fresh matching reports"
                        );
                    }
                }
            }
            checked += 1;
        }
    }
    for budget in 1..=100 {
        assert_eq!(
            checkpoint::refresh_round(0, budget),
            None,
            "zero refresh round accepted"
        );
        assert_eq!(
            checkpoint::refresh_round(budget + 1, budget),
            None,
            "exhausted refresh round accepted"
        );
        for source_round in 1..=budget {
            let first = checkpoint::refresh_round(source_round, budget).unwrap();
            assert_eq!(first, source_round, "refresh global review budget reset");
            for local in 1..=budget + 1 {
                let resumed_round = checkpoint::review_round(first, local, budget);
                let expected =
                    (source_round + local - 1 <= budget).then_some(source_round + local - 1);
                assert_eq!(
                    resumed_round, expected,
                    "strict resume reset refreshed budget"
                );
                if let Some(round) = resumed_round {
                    assert_eq!(
                        checkpoint::can_fix(round, budget),
                        round < budget,
                        "final global review admitted a fix"
                    );
                }
            }
        }
    }
    println!(
        "crabber-refresh-checker-v1 cases={checked} budgets=1..100 safety=PASS termination=PASS"
    );
}

#[test]
fn formal_snapshot_final_prompt_bound() {
    use crate::review_inputs::{PROMPT_LIMIT, validate_prompt};
    for unit in ["x", "\\", "\u{0001}", "😀"] {
        let mut exact = unit.repeat(PROMPT_LIMIT / unit.len());
        assert_eq!(exact.len(), PROMPT_LIMIT);
        assert!(
            validate_prompt(&exact).is_ok(),
            "legal prompt boundary rejected"
        );
        exact.push('x');
        assert!(
            validate_prompt(&exact).is_err(),
            "final rendered prompt cap omitted"
        );
    }
}

#[test]
fn formal_refresh_old_reports_are_evidence_only() {
    use crate::{durable, git};
    use std::fs;
    let (_temporary, mut config) = checkpoint::tests::fixture();
    config.executable_digest = durable::digest(b"source executable");
    git::output(&config.repo, &["switch", "-c", "feature"]).unwrap();
    fs::write(config.repo.join("hello.txt"), "implemented\n").unwrap();
    git::output(&config.repo, &["add", "hello.txt"]).unwrap();
    git::output(&config.repo, &["commit", "-m", "implementation"]).unwrap();
    let commit = git::output(&config.repo, &["rev-parse", "HEAD"]).unwrap();
    let snapshot = checkpoint::tests::snapshot(&config, 2);
    let mut source =
        checkpoint::ReviewCheckpoint::create(&config, 2, commit.clone(), &snapshot).unwrap();
    durable::write_json(
        &config.artifacts.join("implementation.json"),
        &serde_json::json!({"commit":commit,"plan":source.inputs.plan}),
        false,
    )
    .unwrap();
    let approve =
        r#"{"verdict":"APPROVE","summary":"source review","findings":[],"positive_notes":[]}"#;
    let mut reports = Vec::new();
    for role in 0..2 {
        source.reserve_role(&config.artifacts, role).unwrap();
        source
            .publish_role(&config, role, approve, &snapshot)
            .unwrap();
        let path = source
            .directory(&config.artifacts)
            .join(format!("{}.report.json", checkpoint::ROLES[role].0));
        reports.push((path.clone(), fs::read(path).unwrap()));
    }
    source.phase = CheckpointPhase::ReviewReportsReady;
    source.persist(&config.artifacts).unwrap();
    let imported = checkpoint::RefreshSource::load(&config, &config.artifacts).unwrap();
    config.run_id = uuid::Uuid::new_v4().to_string();
    config.artifacts = config.artifacts.parent().unwrap().join(&config.run_id);
    config.refresh_source = Some(imported);
    config.executable_digest = durable::digest(b"child executable");
    assert!(
        source.inputs.verify(&config).is_err(),
        "strict resume accepted changed executable"
    );
    checkpoint::begin_execution(&mut config).unwrap();
    let child = config.resume.as_ref().unwrap();
    assert_eq!(child.round, 2, "refresh global review budget reset");
    assert_eq!(child.inputs.executable, config.executable_digest);
    for role in 0..2 {
        assert!(
            child.report(&config.artifacts, role).unwrap().is_none(),
            "refresh imported old report as approval"
        );
    }
    assert_eq!(child.attempts, [0, 0]);
    assert_eq!(child.sessions, [None, None]);
    assert_eq!(child.reused, [false, false]);
    for (path, bytes) in reports {
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
}

#[test]
fn formal_delegation_reservations() {
    use crate::review_dispatch::Reservations;
    let mut cases = 0;
    for missing in [[false, false], [false, true], [true, false], [true, true]] {
        for background in [false, true] {
            for profile in ["correctness", "maintainability", "unknown"] {
                for task in [
                    "correctness-attempt",
                    "maintainability-attempt",
                    "unreserved",
                ] {
                    let names = std::array::from_fn(|role| {
                        missing[role].then(|| format!("{}-attempt", checkpoint::ROLES[role].0))
                    });
                    let mut reservations = Reservations::new(names);
                    let expected = checkpoint::ROLES
                        .iter()
                        .position(|role| role.0 == profile)
                        .filter(|role| {
                            !background
                                && missing[*role]
                                && task == format!("{}-attempt", checkpoint::ROLES[*role].0)
                        });
                    assert_eq!(
                        reservations.claim(profile, task, background),
                        expected,
                        "delegation escaped immutable role reservation"
                    );
                    if expected.is_some() {
                        assert!(
                            reservations.claim(profile, task, background).is_none(),
                            "delegation dispatched the same role twice"
                        );
                    }
                    for (role, missing) in missing.iter().enumerate() {
                        let result = reservations.claim(
                            checkpoint::ROLES[role].0,
                            &format!("{}-attempt", checkpoint::ROLES[role].0),
                            false,
                        );
                        assert_eq!(
                            result,
                            (*missing && expected != Some(role)).then_some(role),
                            "delegation consumed an independent role"
                        );
                    }
                    cases += 1;
                }
            }
        }
    }
    println!("crabber-delegation-checker-v1 cases={cases} safety=PASS termination=PASS");
}
