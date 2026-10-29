use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

pub(crate) const REPORTS: [&str; 5] = [
    "00-overview.md",
    "01-critical-and-important.md",
    "02-suggestions.md",
    "03-positive-notes.md",
    "04-action-items.md",
];

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum ReviewVerdict {
    Approve,
    RequestChanges,
    NeedsDiscussion,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewReport {
    pub(crate) verdict: ReviewVerdict,
    pub(crate) summary: String,
    pub(crate) findings: Vec<Finding>,
    pub(crate) positive_notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum Severity {
    Critical,
    Important,
    Suggestion,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Finding {
    pub(crate) severity: Severity,
    pub(crate) path: String,
    pub(crate) line: u32,
    pub(crate) description: String,
    pub(crate) suggested_fix: String,
}

impl ReviewReport {
    pub(crate) fn blocking_items(&self) -> usize {
        self.findings
            .iter()
            .filter(|finding| finding.severity != Severity::Suggestion)
            .count()
    }
}

pub(crate) fn parse_report(text: &str) -> Result<ReviewReport> {
    let text = text.trim();
    let text = text.strip_suffix("```").unwrap_or(text).trim_end();
    let report = text
        .char_indices()
        .filter(|(_, character)| *character == '{')
        .find_map(|(offset, _)| serde_json::from_str::<ReviewReport>(&text[offset..]).ok())
        .context("review must return its structured JSON report")?;
    ensure!(
        !report.summary.trim().is_empty(),
        "review summary cannot be empty"
    );
    for finding in &report.findings {
        let path = std::path::Path::new(&finding.path);
        ensure!(
            !finding.path.is_empty()
                && !path.is_absolute()
                && !path
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir)),
            "finding path must be repository-relative"
        );
        ensure!(
            finding.line > 0
                && !finding.description.trim().is_empty()
                && !finding.suggested_fix.trim().is_empty(),
            "finding needs a line, description, and suggested fix"
        );
    }
    ensure!(
        report.verdict != ReviewVerdict::Approve || report.blocking_items() == 0,
        "review approval contradicts blocking findings"
    );
    Ok(report)
}

pub(crate) fn render_report(
    report: &ReviewReport,
    branch: &str,
    head: &str,
    reviewer: &(&str, &str, &str),
    stats: &str,
) -> [String; 5] {
    let verdict = serde_json::to_value(report.verdict)
        .expect("enum serializes")
        .as_str()
        .unwrap()
        .to_owned();
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let overview = format!(
        "# Review by {}\n\nBranch: {branch}\nHEAD: {head}\nDate: Unix timestamp {timestamp}\nReviewer slug: {}\nRole: {}\n\n{}\n\nOverall verdict: {verdict}\n\n```text\n{stats}\n```\n",
        reviewer.1, reviewer.0, reviewer.2, report.summary
    );
    let details = |suggestions: bool| {
        let mut body = String::new();
        for finding in report
            .findings
            .iter()
            .filter(|finding| (finding.severity == Severity::Suggestion) == suggestions)
        {
            body.push_str(&format!(
                "## {:?} at {}:{}\n\n{}\n\nSuggested fix\n\n{}\n\n",
                finding.severity,
                finding.path,
                finding.line,
                finding.description,
                finding.suggested_fix
            ));
        }
        if body.is_empty() {
            body.push_str("No issues found.\n");
        }
        body
    };
    let positives = if report.positive_notes.is_empty() {
        "No specific positive notes recorded.\n".into()
    } else {
        report
            .positive_notes
            .iter()
            .map(|note| format!("- {note}\n"))
            .collect()
    };
    let mut actions = String::from("# Action items\n\n");
    for severity in [
        Severity::Critical,
        Severity::Important,
        Severity::Suggestion,
    ] {
        actions.push_str(&format!("## {severity:?}\n\n"));
        for finding in report
            .findings
            .iter()
            .filter(|finding| finding.severity == severity)
        {
            actions.push_str(&format!(
                "- [ ] [{}:{}] {} Suggested fix: {}\n",
                finding.path,
                finding.line,
                finding.description.replace('\n', " "),
                finding.suggested_fix.replace('\n', " ")
            ));
        }
        actions.push('\n');
    }
    [overview, details(false), details(true), positives, actions]
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: String,
    branch: String,
    base_ref: String,
    head_sha: String,
    pass: u32,
    reviewers: Vec<serde_json::Value>,
    inputs_digest: String,
    report_digests: [String; 2],
    files: BTreeMap<String, String>,
}

pub(crate) fn publish_review(
    config: &crate::config::RunConfig,
    state: &mut crate::review_checkpoint::ReviewCheckpoint,
    reports: &[ReviewReport; 2],
    stats: &str,
) -> Result<PathBuf> {
    use crate::{durable, review_checkpoint::ROLES};
    let root = config.repo.join("reviews");
    let name = format!("{}-{}", config.run_id, state.round);
    let final_dir = root.join(&name);
    let digests = [
        durable::digest(&serde_json::to_vec(&reports[0])?),
        durable::digest(&serde_json::to_vec(&reports[1])?),
    ];
    if final_dir.try_exists()? {
        durable::directory_existing(&final_dir)?;
        let manifest: Manifest =
            durable::json(&final_dir.join("manifest.json"), durable::METADATA_LIMIT)?;
        ensure!(
            manifest.schema_version == "review-v1"
                && manifest.inputs_digest == state.inputs.digest()?
                && manifest.report_digests == digests
                && manifest.head_sha == state.inputs.head
                && manifest.branch == state.inputs.branch
                && manifest.pass == state.round,
            "published review identity mismatch"
        );
        ensure!(manifest.files.len() == 10, "published review is incomplete");
        for role in ROLES {
            durable::directory_existing(&final_dir.join(role.0))?;
            for file in REPORTS {
                let name = format!("{}/{file}", role.0);
                ensure!(
                    manifest.files.get(&name)
                        == Some(&durable::digest(&durable::read(
                            &final_dir.join(&name),
                            durable::REPORT_LIMIT
                        )?)),
                    "published review file differs"
                );
            }
        }
        durable::sync_dir(&root)?;
        if state.staging_generation.take().is_some() {
            state.persist(&config.artifacts)?;
        }
        return Ok(final_dir);
    }
    let generation = state.staging_generation.unwrap_or(config.generation);
    ensure!(
        generation > 0 && generation <= config.generation,
        "invalid staging generation"
    );
    let staging_name = format!(".{name}.staging-{generation}");
    let staging = root.join(&staging_name);
    let exclude = PathBuf::from(crate::git::output(
        &config.repo,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "info/exclude",
        ],
    )?);
    durable::directory(exclude.parent().context("missing exclude parent")?)?;
    if exclude.try_exists()? {
        ensure!(
            !fs::symlink_metadata(&exclude)?.file_type().is_symlink(),
            "unsafe Git exclude file"
        );
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&exclude)?;
    writeln!(file, "\n/reviews/{name}/\n/reviews/{staging_name}/")?;
    file.sync_all()?;
    durable::sync_dir(exclude.parent().unwrap())?;
    state.staging_generation = Some(generation);
    state.persist(&config.artifacts)?;
    durable::directory(&root)?;
    if staging.try_exists()? {
        validate_tree(&staging)?;
        fs::remove_dir_all(&staging)?;
        durable::sync_dir(&root)?;
    }
    durable::directory(&staging)?;
    let mut hashes = BTreeMap::new();
    for (i, role) in ROLES.iter().enumerate() {
        let folder = staging.join(role.0);
        durable::directory(&folder)?;
        for (file, body) in REPORTS.into_iter().zip(render_report(
            &reports[i],
            &state.inputs.branch,
            &state.inputs.head,
            role,
            stats,
        )) {
            durable::bytes(&folder.join(file), body.as_bytes(), false)?;
            hashes.insert(
                format!("{}/{file}", role.0),
                durable::digest(body.as_bytes()),
            );
        }
    }
    let manifest = Manifest { schema_version: "review-v1".into(), branch: state.inputs.branch.clone(), base_ref: config.base.clone(), head_sha: state.inputs.head.clone(),
        pass: state.round, reviewers: ROLES.iter().map(|(slug,name,role)| serde_json::json!({"slug":slug,"display_name":name,"role":role})).collect(),
        inputs_digest: state.inputs.digest()?, report_digests: digests, files: hashes };
    durable::write_json(&staging.join("manifest.json"), &manifest, false)?;
    durable::sync_dir(&staging)?;
    state.inputs.verify(config)?;
    fs::rename(&staging, &final_dir)?;
    durable::sync_dir(&root)?;
    state.staging_generation = None;
    state.persist(&config.artifacts)?;
    Ok(final_dir)
}

fn validate_tree(path: &Path) -> Result<()> {
    crate::durable::directory_existing(path)?;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        ensure!(!kind.is_symlink(), "unsafe staging entry");
        if kind.is_dir() {
            validate_tree(&entry.path())?;
        } else {
            ensure!(kind.is_file(), "unsafe staging entry");
        }
    }
    Ok(())
}
