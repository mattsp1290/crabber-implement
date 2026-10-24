use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use std::{fs, num::NonZeroUsize, path::PathBuf, time::Duration};

use crate::git;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Implement a plan, review it, and fix findings with Crabber agents"
)]
pub struct Cli {
    /// A Markdown plan or an implementation-plan directory.
    pub plan: PathBuf,
    /// Repository to change. Defaults to the current repository.
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
    /// Review and fix-review skill directories.
    #[arg(long)]
    pub skills_dir: Option<PathBuf>,
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
}

pub struct RunConfig {
    pub repo: PathBuf,
    pub plan: PlanBundle,
    pub review_skill: String,
    pub fix_skill: String,
    pub base: String,
    pub model: String,
    pub base_url: String,
    pub max_reviews: u32,
    pub max_turns: NonZeroUsize,
    pub session_timeout: Duration,
    pub artifacts: PathBuf,
    pub run_id: String,
    pub _repo_lock: fs::File,
}

pub struct PlanBundle {
    pub text: String,
}

impl PlanBundle {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let path = path.canonicalize().context("plan path does not exist")?;
        let files = if path.is_dir() {
            let mut files = fs::read_dir(&path)?
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<std::io::Result<Vec<_>>>()?;
            files.retain(|file| {
                file.is_file()
                    && file.extension().is_some_and(|extension| extension == "md")
                    && file.file_name().is_some_and(|name| {
                        name.to_string_lossy()
                            .chars()
                            .next()
                            .is_some_and(|c| c.is_ascii_digit())
                    })
            });
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
            let body = fs::read_to_string(&file)
                .with_context(|| format!("read plan {}", file.display()))?;
            ensure!(
                !body.trim().is_empty(),
                "empty plan file {}",
                file.display()
            );
            ensure!(text.len() + body.len() <= 2_000_000, "plan exceeds 2 MB");
            for line in body.lines() {
                let status = line.replace(['*', '`'], "").to_lowercase();
                if status.trim().starts_with("status:") && status.contains("blocked") {
                    bail!("plan is blocked in {}", file.display());
                }
            }
            text.push_str(&format!(
                "\n# Plan document {}\n\n{}\n",
                file.file_name().unwrap().to_string_lossy(),
                body
            ));
        }
        Ok(Self { text })
    }
}

impl RunConfig {
    pub fn load(cli: Cli) -> Result<Self> {
        let plan = PlanBundle::load(&cli.plan)?;
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
        ensure!(
            std::env::var("OPENCODE_GO_API_KEY").is_ok_and(|key| !key.is_empty()),
            "export OPENCODE_GO_API_KEY before running"
        );
        ensure!(!cli.model.trim().is_empty(), "model cannot be empty");
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        let skills = cli
            .skills_dir
            .unwrap_or_else(|| PathBuf::from(home).join("git/dotfiles/.agents/skills"));
        let review_skill =
            fs::read_to_string(skills.join("review/SKILL.md")).context("read review skill")?;
        let fix_skill = fs::read_to_string(skills.join("fix-review/SKILL.md"))
            .context("read fix-review skill")?;
        let run_id = uuid::Uuid::new_v4().to_string();
        let git_dir = PathBuf::from(git::output(&repo, &["rev-parse", "--absolute-git-dir"])?);
        let artifacts = git_dir.join("crabber-implement").join(&run_id);
        fs::create_dir_all(&artifacts)?;
        fs::write(artifacts.join("plan.md"), &plan.text)?;
        let branch = git::output(&repo, &["branch", "--show-current"])?;
        ensure!(
            !branch.is_empty(),
            "detached HEAD is unsupported; switch to a branch"
        );
        if branch == base {
            git::output(
                &repo,
                &["switch", "-c", &format!("implement/{}", &run_id[..8])],
            )?;
        }
        Ok(Self {
            repo,
            plan,
            review_skill,
            fix_skill,
            base,
            model: cli.model,
            base_url: cli.base_url,
            max_reviews: cli.max_reviews,
            max_turns: cli.max_turns,
            session_timeout: Duration::from_secs(cli.session_timeout),
            artifacts,
            run_id,
            _repo_lock: repo_lock,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
