use std::collections::{BTreeMap, BTreeSet, VecDeque};

use dmt::{EndStatus, Guard, NodeKind};
use flows_core::{Compiled, compile};

use crate::workflow::{document, occurrence_allowed};

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

fn verify_budget(budget: u32) -> (usize, usize) {
    let graph = compile(&document(budget, 3600), &BTreeMap::new()).expect("actual graph compiles");
    let initial = State {
        node: graph.graph.start().as_str().into(),
        implemented: false,
        review: ReviewState::Missing,
        reviews: 0,
        fixes: 0,
    };
    let mut queue = VecDeque::from([(initial.clone(), vec![initial.node.clone()])]);
    let mut visited = BTreeSet::from([initial]);
    let mut adjacency = BTreeMap::<State, Vec<State>>::new();
    let mut terminals = 0;
    while let Some((state, trace)) = queue.pop_front() {
        let failure = || format!("budget {budget}, path {}", trace.join(" -> "));
        assert!(
            state.reviews <= budget && state.fixes <= budget,
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
            "review" => state.reviews + 1,
            "fix-review" => {
                assert_eq!(
                    state.review,
                    ReviewState::Changes,
                    "fix without selected change request: {}",
                    failure()
                );
                state.fixes + 1
            }
            "borb_acquire" => 1,
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
            let mut output = entered.clone();
            match (state.node.as_str(), label.as_str()) {
                ("implement", "implemented") => output.implemented = true,
                ("review", "approved") => output.review = ReviewState::Approved,
                ("review", "changes") => output.review = ReviewState::Changes,
                ("fix-review", "fixed") => output.review = ReviewState::Missing,
                ("borb_acquire", "ok") => {}
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
        let (count, ends) = verify_budget(budget);
        states += count;
        terminals += ends;
    }
    println!(
        "crabber-workflow-checker-v1 budgets=1..100 states={states} terminal_cases={terminals} safety=PASS termination=PASS"
    );
}
