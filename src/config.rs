use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use std::{
    fs,
    num::{NonZeroU32, NonZeroUsize},
    path::PathBuf,
    time::Duration,
};

use crate::git;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Implement a plan, review it, and fix findings with Crabber agents"
)]
pub struct Cli {
    /// A Markdown plan or an implementation-plan directory.
    pub plan: PathBuf,
    /// Resume only an incomplete review from this workflow artifact ID.
    #[arg(long, conflicts_with = "refresh_review")]
    pub resume_review: Option<uuid::Uuid>,
    /// Start fresh reviewers from a validated saved review, preserving its global round.
    #[arg(long, conflicts_with = "resume_review")]
    pub refresh_review: Option<uuid::Uuid>,
    /// Repository to change. Defaults to the current repository.
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
    /// Directory containing review/SKILL.md.
    #[arg(long)]
    pub skills_dir: Option<PathBuf>,
    /// Disable trace export to the local Datadog agent.
    #[arg(long, env = "CRABBER_IMPLEMENT_NO_TELEMETRY")]
    pub no_telemetry: bool,
    /// Base branch. Defaults to .ralph main_branch or main.
    #[arg(long)]
    pub base: Option<String>,
    #[arg(long, env = "CRABBER_IMPLEMENT_MODEL", default_value = "glm-5.2")]
    pub model: String,
    /// OpenAI-compatible API root, without /chat/completions.
    #[arg(
        long,
        env = "CRABBER_IMPLEMENT_BASE_URL",
        default_value = "https://opencode.ai/zen/go/v1"
    )]
    pub base_url: String,
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u32).range(1..=100))]
    pub max_reviews: u32,
    /// Maximum model turns per agent session.
    #[arg(long, env = "CRABBER_IMPLEMENT_MAX_TURNS", default_value = "256")]
    pub max_turns: NonZeroUsize,
    /// Maximum wall time per agent session.
    #[arg(long, default_value_t = 1800, value_parser = clap::value_parser!(u64).range(1..))]
    pub session_timeout: u64,
    /// Initial maximum output tokens per model response.
    #[arg(long, default_value = "16384")]
    pub max_output_tokens: NonZeroU32,
    /// Maximum output tokens requested during truncation recovery.
    #[arg(long, default_value = "65536")]
    pub max_recovery_output_tokens: NonZeroU32,
}

#[derive(Clone, Copy)]
pub struct OutputBudget {
    pub initial: NonZeroU32,
    pub ceiling: NonZeroU32,
}

impl OutputBudget {
    fn new(initial: NonZeroU32, ceiling: NonZeroU32) -> Result<Self> {
        ensure!(
            initial <= ceiling,
            "--max-recovery-output-tokens must be at least --max-output-tokens"
        );
        Ok(Self { initial, ceiling })
    }
}

pub struct RunConfig {
    pub repo: PathBuf,
    pub plan: PlanBundle,
    pub review_skill: String,
    pub base: String,
    pub model: String,
    pub base_url: String,
    pub max_reviews: u32,
    pub max_turns: NonZeroUsize,
    pub session_timeout: Duration,
    pub output_budget: OutputBudget,
    pub artifacts: PathBuf,
    pub run_id: String,
    pub _repo_lock: fs::File,
    pub executable_digest: String,
    pub generation: u64,
    pub predecessor_trace: Option<String>,
    pub resume: Option<crate::review_checkpoint::ReviewCheckpoint>,
    pub refresh_source: Option<crate::review_checkpoint::RefreshSource>,
    pub recovery_origin: Option<crate::review_checkpoint::RecoveryOrigin>,
}

pub struct PlanBundle {
    pub text: String,
}

impl PlanBundle {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let path = path.canonicalize().context("plan path does not exist")?;
        let files = if path.is_dir() {
            let mut files = Vec::new();
            for (index, entry) in fs::read_dir(&path)?.enumerate() {
                ensure!(index < 4096, "plan directory exceeds 4096 entries");
                let entry = entry?;
                let file = entry.path();
                if file.extension().is_some_and(|extension| extension == "md")
                    && file.file_name().is_some_and(|name| {
                        name.to_string_lossy()
                            .starts_with(|c: char| c.is_ascii_digit())
                    })
                {
                    ensure!(
                        entry.file_type()?.is_file(),
                        "plan input must be a regular file"
                    );
                    ensure!(files.len() < 256, "plan exceeds 256 selected documents");
                    files.push(file);
                }
            }
            files.sort();
            ensure!(
                !files.is_empty(),
                "plan directory has no numerically prefixed Markdown files"
            );
            ensure!(
                path.join("00-overview.md").is_file(),
                "plan directory needs 00-overview.md"
            );
            files
        } else {
            ensure!(
                path.extension().is_some_and(|extension| extension == "md"),
                "plan must be a Markdown file or directory"
            );
            vec![path]
        };
        let mut text = String::new();
        for file in files {
            let header = format!(
                "\n# Plan document {}\n\n",
                file.file_name().unwrap().to_string_lossy()
            );
            let remaining = 2_000_000usize
                .checked_sub(text.len() + header.len() + 1)
                .context("plan exceeds 2 MB including document headers")?;
            let body = String::from_utf8(
                crate::durable::read(&file, remaining)
                    .context("plan exceeds size limit or is not a regular file")?,
            )?;
            ensure!(
                !body.trim().is_empty(),
                "empty plan file {}",
                file.display()
            );
            for line in body.lines() {
                let status = line.replace(['*', '`'], "").to_lowercase();
                if status.trim().starts_with("status:") && status.contains("blocked") {
                    bail!("plan is blocked in {}", file.display());
                }
            }
            text.push_str(&header);
            text.push_str(&body);
            text.push('\n');
        }
        Ok(Self { text })
    }
}

impl RunConfig {
    pub fn load(cli: Cli) -> Result<Self> {
        let output_budget =
            OutputBudget::new(cli.max_output_tokens, cli.max_recovery_output_tokens)?;
        let plan = PlanBundle::load(&cli.plan)?;
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        let skills = cli
            .skills_dir
            .unwrap_or_else(|| PathBuf::from(home).join("git/dotfiles/.agents/skills"));
        let review_skill = String::from_utf8(
            crate::durable::read(&skills.join("review/SKILL.md"), 16 * 1024 * 1024)
                .context("read review skill within 16 MiB limit")?,
        )?;
        let repo = PathBuf::from(git::output(&cli.repo, &["rev-parse", "--show-toplevel"])?)
            .canonicalize()?;
        let base = cli.base.unwrap_or_else(|| {
            fs::read_to_string(repo.join(".ralph"))
                .ok()
                .and_then(|text| {
                    text.lines()
                        .find_map(|line| line.strip_prefix("main_branch=").map(str::to_owned))
                })
                .unwrap_or_else(|| "main".into())
        });
        ensure!(
            !base.starts_with('-') && !base.trim().is_empty(),
            "invalid base ref"
        );
        git::output(
            &repo,
            &["rev-parse", "--verify", &format!("{base}^{{commit}}")],
        )?;
        let git_dir = PathBuf::from(git::output(
            &repo,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?);
        let repo_lock = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(git_dir.join("crabber-implement.lock"))?;
        repo_lock
            .try_lock()
            .context("another crabber-implement process owns this repository")?;
        git::require_clean(&repo)?;
        ensure!(!cli.model.trim().is_empty(), "model cannot be empty");
        let run_id = cli
            .resume_review
            .unwrap_or_else(uuid::Uuid::new_v4)
            .to_string();
        let git_dir = PathBuf::from(git::output(&repo, &["rev-parse", "--absolute-git-dir"])?)
            .canonicalize()?;
        let artifacts = git_dir.join("crabber-implement").join(&run_id);
        let executable_digest = {
            use sha2::{Digest, Sha256};
            use std::io::Read;
            let mut file = fs::File::open(std::env::current_exe()?)?;
            let mut hasher = Sha256::new();
            let mut buffer = [0u8; 65536];
            loop {
                let n = file.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buffer[..n]);
            }
            format!("{:x}", hasher.finalize())
        };
        let resume = if cli.resume_review.is_some() {
            Some(crate::review_checkpoint::ReviewCheckpoint::load(
                &artifacts,
            )?)
        } else {
            None
        };
        let generation = match &resume {
            Some(state) => state
                .generation
                .checked_add(1)
                .context("execution generation overflow")?,
            None => 1,
        };
        let branch = git::output(&repo, &["branch", "--show-current"])?;
        ensure!(
            !branch.is_empty(),
            "detached HEAD is unsupported; switch to a branch"
        );
        let mut config = Self {
            repo,
            plan,
            review_skill,
            base,
            model: cli.model,
            base_url: cli.base_url,
            max_reviews: cli.max_reviews,
            max_turns: cli.max_turns,
            session_timeout: Duration::from_secs(cli.session_timeout),
            output_budget,
            artifacts,
            run_id,
            _repo_lock: repo_lock,
            executable_digest,
            generation,
            predecessor_trace: None,
            resume,
            refresh_source: None,
            recovery_origin: None,
        };
        if let Some(source_id) = cli.refresh_review {
            let source_root = git_dir
                .join("crabber-implement")
                .join(source_id.to_string());
            let source = crate::review_checkpoint::RefreshSource::load(&config, &source_root)?;
            require_credentials()?;
            config.refresh_source = Some(source);
        } else if let Some(state) = &config.resume {
            config.recovery_origin =
                crate::review_checkpoint::RecoveryOrigin::load(&config.artifacts)?;
            state.inputs.verify(&config)?;
            ensure!(state.workflow_id == config.run_id, "workflow ID mismatch");
            ensure!(
                git::output(
                    &config.repo,
                    &[
                        "merge-base",
                        "--is-ancestor",
                        &state.implementation_commit,
                        "HEAD"
                    ]
                )
                .is_ok(),
                "implementation anchor is not an ancestor of reviewed HEAD"
            );
            if state.needs_credentials(&config.artifacts)? {
                require_credentials()?;
            }
        } else {
            require_credentials()?;
            crate::durable::directory(&config.artifacts)?;
            crate::durable::bytes(
                &config.artifacts.join("plan.md"),
                config.plan.text.as_bytes(),
                false,
            )?;
            if branch == config.base {
                git::output(
                    &config.repo,
                    &[
                        "switch",
                        "-c",
                        &format!("implement/{}", &config.run_id[..8]),
                    ],
                )?;
            }
        }
        Ok(config)
    }
}

pub(crate) fn require_credentials() -> Result<()> {
    ensure!(
        std::env::var("OPENCODE_GO_API_KEY").is_ok_and(|key| !key.is_empty()),
        "export OPENCODE_GO_API_KEY before running"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn output_budget_defaults_and_overrides_are_validated() {
        let defaults = Cli::try_parse_from(["cli", "plan.md"]).unwrap();
        assert_eq!(defaults.max_output_tokens.get(), 16384);
        assert_eq!(defaults.max_recovery_output_tokens.get(), 65536);
        let custom = Cli::try_parse_from([
            "cli",
            "plan.md",
            "--max-output-tokens",
            "20000",
            "--max-recovery-output-tokens",
            "50000",
        ])
        .unwrap();
        let budget =
            OutputBudget::new(custom.max_output_tokens, custom.max_recovery_output_tokens).unwrap();
        assert_eq!(budget.initial.get(), 20000);
        assert_eq!(budget.ceiling.get(), 50000);
        assert!(
            OutputBudget::new(custom.max_recovery_output_tokens, custom.max_output_tokens).is_err()
        );
    }

    #[test]
    fn directory_reads_ordered_documents_and_rejects_blocked() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("01-work.md"), "Implement the widget.").unwrap();
        fs::write(directory.path().join("00-overview.md"), "Status: Ready").unwrap();
        let plan = PlanBundle::load(directory.path()).unwrap();
        assert!(plan.text.find("Status: Ready").unwrap() < plan.text.find("Implement").unwrap());
        fs::write(
            directory.path().join("00-overview.md"),
            "**Status:** Blocked",
        )
        .unwrap();
        assert!(PlanBundle::load(directory.path()).is_err());
    }
}
